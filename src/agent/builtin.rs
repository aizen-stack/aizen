//! The built-in tool surface (H3). Five orthogonal tools the agent loop advertises:
//! `memory_search` (the brain), `file_read`/`file_glob` (read-only), `file_edit`/`shell_run`
//! (destructive → approval-gated by the loop).
//!
//! Paths: `root` (captured at registry-build time) is the anchor for resolving RELATIVE paths, so
//! tools stay testable against a temp dir without mutating the process-global cwd. The workspace
//! CONFINEMENT boundary was removed by user request — file/shell ops may now reach paths anywhere
//! on disk (a relative path still resolves under `root`; `../` and absolute paths escape freely).
//! SECURITY trade-off: agent-read web/tool content can now steer a write to an arbitrary path.

use crate::agent::tools::{Tool, ToolRegistry, WorkspaceEffect};
use anyhow::{bail, Context, Result};
use ignore::{WalkBuilder, WalkState};
use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

/// Default wall-clock cap for `shell_run` (a hung command must never freeze the agent loop).
const SHELL_TIMEOUT_SECS: u64 = 120;

/// Env override for the `shell_run` cap. 120s is right for a test run and wrong for a cold
/// `cargo build` on a big workspace — a fixed ceiling turns "slow" into "killed", and the model then
/// retries the same command, paying the full wait twice. Clamped to a sane band so a typo cannot
/// disable the ceiling that keeps the loop responsive.
const SHELL_TIMEOUT_ENV: &str = "AIZEN_SHELL_TIMEOUT_SECS";

/// How long to wait for the drain threads AFTER the child is gone.
///
/// This is the fix for a confirmed hang: on Windows `kill()` reaps only the `cmd.exe` wrapper, and a
/// surviving grandchild still holds the inherited write end of our pipes, so `read_to_end` never sees
/// EOF. The old code called `join()` unconditionally and could block forever — measured still blocked
/// 12s after a kill, against a 45s sleeper. Tree-kill removes the cause; this bound means even an
/// exotic case (a descendant we could not contain, a pipe inherited by an unrelated process) costs
/// two seconds and a partial-output note instead of the whole session.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// Resolve the `shell_run` wall-clock cap: env override (clamped 10s..=3600s) or the default.
fn shell_timeout_secs() -> u64 {
    std::env::var(SHELL_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|v| v.clamp(10, 3600))
        .unwrap_or(SHELL_TIMEOUT_SECS)
}

/// How often to say which command is still running. The working pill already animates and counts
/// elapsed seconds, so plain liveness is covered; what it cannot show is *which* command is out and
/// how close the ceiling is — and without that a slow build and a wedged process look identical.
const SLOW_NOTE_EVERY_SECS: u64 = 30;

/// Note a still-running command into the transcript (once per [`SLOW_NOTE_EVERY_SECS`]).
///
/// Routed through the same trace surface the tool lines use, so it lands in the retained TUI's buffer
/// rather than being printed over the frame.
fn note_slow_command(command: &str, waited: u64, cap: u64) {
    // One line, command clipped: a 300-char pipeline would wrap and push the transcript around.
    let mut label: String = command.trim().chars().take(60).collect();
    if command.trim().chars().count() > 60 {
        label.push('…');
    }
    crate::agent::emit_trace_public(&format!(
        "→ still running after {waited}s (cap {cap}s): {label}  ·  Esc cancels"
    ));
}

/// Prefix a write tool returns when the target already held byte-identical content — so nothing
/// was written to disk. The agent loop keys off this exact prefix (`turn_made_edits`) to NOT arm
/// the verify gate for a no-op (an unchanged tree can't have broken), and the thrash guard sees it
/// as "not an edit" so a model re-writing the same bytes climbs to a stop instead of looping.
pub(crate) const NOOP_WRITE_PREFIX: &str = "no change (identical content)";

/// `file_read` budget: a WHOLE-file read over EITHER cap returns a head+tail preview with a loud
/// marker (so the model knows it has a partial view). A range/numbered read is NEVER bounded. Small
/// files (the common case) stay byte-exact so `old_string` round-trips. `0` disables a cap.
const FILE_READ_MAX_LINES: usize = 2000;
const FILE_READ_MAX_BYTES: usize = 200_000;

/// The live top-level tool surface, published once the session's registry is built. The skills
/// index consults it to hide any skill whose `requires:` tool is absent from this build/session
/// (e.g. `browser_*` when `--features browser` is off, or MCP tools when no server is configured).
/// `None` until published → the filter is a no-op (show all), so the unit tests and the offline
/// `aizen skill` path are unaffected.
static ACTIVE_TOOL_NAMES: Lazy<Mutex<Option<HashSet<String>>>> = Lazy::new(|| Mutex::new(None));

/// Publish the live tool surface (idempotent). ONLY the top-level registry calls this — never the
/// smaller `role_registry`, so the set is never wrongly shrunk when a sub-agent assembles a prompt.
fn publish_active_tools(r: &ToolRegistry) {
    *ACTIVE_TOOL_NAMES.lock().unwrap_or_else(|e| e.into_inner()) =
        Some(r.names().into_iter().collect());
}

/// The published live tool surface, or `None` if no session registry has been built yet.
pub fn active_tool_names() -> Option<HashSet<String>> {
    ACTIVE_TOOL_NAMES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Resolve + canonicalize the working-directory root — the base that relative file/shell paths
/// resolve against (NOT a hard boundary: `confine` no longer rejects paths that escape it, so an
/// absolute or `../` path may reach elsewhere on disk — see [`confine`]).
fn resolve_root() -> Result<PathBuf> {
    std::env::current_dir()
        .context("resolving cwd")?
        .canonicalize()
        .context("canonicalizing cwd")
}

/// The built-in tools rooted at `root`. Shared by the top-level registry and the `coder`
/// sub-agent role.
fn default_registry_in(root: &Path) -> ToolRegistry {
    use crate::agent::web_tools::{WebCrawl, WebFetch, WebSearch};
    // Registry construction happens exactly once per fresh top-level user turn. Apply any deferred
    // MCP `tools/list_changed` notification here — never from inside an agent run — so the model's
    // advertised tool schemas remain pinned for the duration of that run.
    crate::agent::mcp::prepare_fresh_turn();
    let mut r = ToolRegistry::new();
    r.register(Box::new(MemorySearch));
    r.register(Box::new(MemoryList));
    r.register(Box::new(MemoryProfile));
    r.register(Box::new(MemoryAsk));
    r.register(Box::new(FileRead::new(root.to_path_buf())));
    r.register(Box::new(FileGlob::new(root.to_path_buf())));
    r.register(Box::new(crate::agent::search::SearchFiles::new(
        root.to_path_buf(),
    )));
    // Semantic chunk search over the `/init` index (read-only). Self-errors with "run /init" when
    // the index is missing, so it's safe to always advertise.
    r.register(Box::new(crate::agent::codebase::CodebaseSearch));
    r.register(Box::new(WebSearch));
    r.register(Box::new(WebFetch));
    r.register(Box::new(WebCrawl));
    register_skill_load(&mut r);
    register_skill_registry(&mut r);
    register_telegram(&mut r);
    // `bot_admin` (host/persona extra bots on the owner's word) is TOP-LEVEL ONLY — never in
    // `subagent_read_only_base`, so a specialist sub-agent can't touch bot config.
    register_bot_admin(&mut r);
    register_notify(&mut r);
    r.register(Box::new(crate::features::timemachine::Checkpoint));
    r.register(Box::new(crate::features::timemachine::CheckpointRewind));
    r.register(Box::new(crate::features::timemachine::CheckpointList));
    r.register(Box::new(crate::features::timemachine::CheckpointDiff));
    r.register(Box::new(crate::features::timemachine::CheckpointRestore));
    // Who else is editing this repository right now. Top-level only, and not because it is unsafe
    // (it is read-only): a sub-agent's writes are attributed to the SESSION that spawned it, so the
    // overlap warning it would need already surfaces on this window's turn. Registering it in
    // `subagent_read_only_base` would pay its schema on every delegated run for an answer the parent
    // is the one acting on.
    r.register(Box::new(crate::features::coop::TeamStatus));
    // Memory WRITE surface — top-level only. A sub-agent gets `memory_list`/`memory_search` but may
    // never mutate the user's long-term store: a specialist run is short-lived and unsupervised, so a
    // wrong write there would outlive the run it came from with no one having seen it.
    r.register(Box::new(MemorySave));
    r.register(Box::new(MemoryUpdate));
    r.register(Box::new(MemoryForget));
    r.register(Box::new(FileEdit::new(root.to_path_buf())));
    r.register(Box::new(MultiEdit::new(root.to_path_buf())));
    r.register(Box::new(FileWrite::new(root.to_path_buf())));
    r.register(Box::new(FileMove::new(root.to_path_buf())));
    r.register(Box::new(ShellRun::new(root.to_path_buf())));
    r.register(Box::new(SkillSave));
    register_skill_refine(&mut r);
    // Top-level only (NOT in role sub-agents) — the in-session list + process pool are shared, so a
    // sub-agent must not clobber them. `role_registry` builds its own list and never gets these.
    r.register(Box::new(crate::agent::todo::TodoWrite));
    r.register(Box::new(crate::agent::process::Process::new(
        root.to_path_buf(),
    )));
    // `clarify` yields the turn back to the interactive user — meaningless inside an autonomous
    // sub-agent (no user to answer), so it stays top-level only, like todo/process.
    r.register(Box::new(crate::agent::clarify::Clarify));
    // `goal_complete` is the DONE-signal for goal mode (`/goal <text>`). Gated on `goal::is_armed()`
    // so ordinary chat/agent turns never pay its schema cost; top-level only (a sub-agent has no
    // goal of its own to declare complete), just like clarify/todo/process.
    if crate::agent::goal::is_armed() {
        r.register(Box::new(crate::agent::goal::GoalComplete));
    }
    // LSP navigation + diagnostics + symbolic edit (top-level only). Default ON + lazy: the
    // manager is armed at session start; tools register when `LSP.is_enabled()` (still true after
    // `/lsp on`, false after `/lsp off`). Sub-agents use `subagent_read_only_base` and never get it.
    if crate::agent::lsp::LSP.is_enabled() {
        r.register(Box::new(crate::agent::lsp::tools::LspReferences::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::lsp::tools::LspDefinition::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::lsp::tools::LspSymbolBody::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::lsp::tools::LspHover::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::lsp::tools::LspDocumentSymbols::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::lsp::tools::LspWorkspaceSymbol::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::lsp::tools::LspDiagnostics::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::lsp::tools::SymbolReplace::new(
            root.to_path_buf(),
        )));
        r.register(Box::new(crate::agent::lsp::tools::SymbolInsert::new(
            root.to_path_buf(),
        )));
        // The ranked codebase skeleton rides the LSP gate too (its symbols come from the servers).
        r.register(Box::new(crate::agent::repo_map::RepoMap::new(
            root.to_path_buf(),
        )));
    }
    // User-configurable MCP servers (`~/.aizen/mcp.json`) — each remote tool wrapped as
    // `mcp_<server>_<tool>`. Empty (zero cost) when MCP is unconfigured. Top-level only, like
    // todo/process: sub-agents share the same live connections via the global manager but don't
    // need the surface advertised to them.
    for t in crate::agent::mcp::discovered_tools() {
        r.register(t);
    }
    // CDP browser tools (OPT-IN: `--features browser`, default OFF). Top-level only; they connect
    // lazily to a local Chrome/Edge/Brave and return an actionable error if none is running.
    #[cfg(feature = "browser")]
    crate::agent::browser::register_browser_tools(&mut r);
    r
}

/// Advertise `skill_load` only when at least one skill exists (else it'd be a dead tool with an
/// empty `<skills>` index). Available to every registry.
fn register_skill_load(r: &mut ToolRegistry) {
    if crate::skills::has_any() {
        r.register(Box::new(SkillLoad));
    }
}

/// Advertise `skill_refine` only when at least one skill exists — you can't refine what isn't there,
/// so it'd be pure schema-token cost on a fresh install. Destructive (rewrites + archives a skill),
/// so it rides only the AUTHORING scopes (top-level + coder/specialist), never the read-only base.
fn register_skill_refine(r: &mut ToolRegistry) {
    if crate::skills::has_any() {
        r.register(Box::new(SkillRefine));
    }
}

/// The agentskill.sh marketplace tools (`skill_search`/`skill_install`) — ALWAYS available (their
/// whole point is finding a skill when you have none locally). Every registry.
fn register_skill_registry(r: &mut ToolRegistry) {
    r.register(Box::new(crate::skills::registry::SkillSearch));
    r.register(Box::new(crate::skills::registry::SkillInstall));
}

/// Advertise the Telegram tools only when a bot token + allowed chat are configured (otherwise
/// they'd be dead tools that just error). Available to every registry.
fn register_telegram(r: &mut ToolRegistry) {
    if crate::hostbot::platforms::telegram::is_configured() {
        r.register(Box::new(crate::hostbot::platforms::telegram::TelegramSend));
        r.register(Box::new(crate::hostbot::platforms::telegram::TelegramAsk));
    }
}

/// Advertise the `bot_admin` tool only when Telegram is configured — it manages the extra bots this
/// daemon hosts (add/remove/set-persona from a chat message). Top-level only: NOT added to
/// `subagent_read_only_base`/`canonical_subagent_tool`, so a specialist sub-agent can't touch the
/// bot registry or write tokens to config.
fn register_bot_admin(r: &mut ToolRegistry) {
    if crate::hostbot::platforms::telegram::is_configured() {
        r.register(Box::new(crate::hostbot::platforms::telegram::BotAdmin));
    }
}

/// Advertise the `notify` broadcast tool only when at least one outbound channel (Discord / Slack /
/// webhook) is configured — otherwise it'd be a dead tool that just errors. Every registry.
fn register_notify(r: &mut ToolRegistry) {
    if crate::channels::notify::any_configured() {
        r.register(Box::new(crate::channels::notify::Notify));
    }
}

/// The default registry PLUS the `task` sub-agent dispatch tool (top level, depth 0). The task
/// tool needs the chat creds so a spawned sub-agent can call the model itself.
///
/// `persona_create` lives ONLY here (the top-level user-facing path), never in `role_registry`:
/// a coder/tester/reviewer sub-agent has no business minting characters.
pub fn default_registry_with_task(
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    approval_mode: crate::core::approval::ApprovalMode,
    context_window: usize,
) -> Result<ToolRegistry> {
    let root = resolve_root()?;
    let mut r = default_registry_in(&root);
    r.register(Box::new(PersonaCreate));
    r.register(Box::new(crate::agent::task_tool::TaskTool::new(
        client.clone(),
        base_url.clone(),
        api_key.clone(),
        model.clone(),
        approval_mode,
        root,
        0,
        context_window,
    )));
    // The `workflow` fan-out tool is GATED: its schema costs ~350 tokens on every turn, so it
    // registers only for the delegating population — ≥1 specialist ENABLED on the allowlist, or
    // an explicit `workflow_tool: true` in config.
    if should_register_workflow() {
        r.register(Box::new(crate::agent::workflow_tool::WorkflowTool::new(
            client,
            base_url,
            api_key,
            model,
            approval_mode,
            0,
        )));
    }
    crate::agent::toolsets::apply_toolset_filter(&mut r);
    // Publish the live surface so the `<skills>` index can hide skills that `require:` an absent tool.
    publish_active_tools(&r);
    Ok(r)
}

/// Register the `workflow` tool? Config wins either way; default: only when ≥1 specialist agent is
/// ENABLED on the allowlist (the population that actually fans out — the `<agents>` index already
/// advertises exactly them). Deliberately NOT "any agent file exists": that over-triggered — a
/// bulk `agents install` never enabled, or a cloned repo shipping `.claude/agents/`, would silently
/// tax every turn with the ~350-token schema.
fn should_register_workflow() -> bool {
    match crate::core::cli_config::load().workflow_tool {
        Some(v) => v,
        // Ultimate mode orchestrates by default, so the fan-out tool must be present even when no
        // specialist agent is enabled (the model is told to prefer it in the system prompt).
        None => crate::agents::any_enabled() || crate::core::cli_config::ultimate_enabled(),
    }
}

/// The read-only tool base shared by EVERY sub-agent registry (role- or specialist-scoped): memory +
/// read/glob/search + web research + skill_load/registry + telegram/notify (when configured) + LSP
/// navigation (when enabled). NEVER includes `task`, checkpoint, edit/shell, or top-level-only
/// stateful tools. Factored out so `role_registry` and `agent_registry` cannot drift.
fn subagent_read_only_base(root: &Path) -> ToolRegistry {
    use crate::agent::web_tools::{WebCrawl, WebFetch, WebSearch};
    let mut r = ToolRegistry::new();
    r.register(Box::new(MemorySearch));
    // `memory_list` is read-only, so a sub-agent may inventory what is stored (that's what stops it
    // guessing) — but the WRITE trio (save/update/forget) is top-level only, registered in
    // `default_registry_in` and deliberately absent here.
    r.register(Box::new(MemoryList));
    r.register(Box::new(MemoryProfile));
    r.register(Box::new(MemoryAsk));
    r.register(Box::new(FileRead::new(root.to_path_buf())));
    r.register(Box::new(FileGlob::new(root.to_path_buf())));
    r.register(Box::new(crate::agent::search::SearchFiles::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::codebase::CodebaseSearch));
    r.register(Box::new(WebSearch));
    r.register(Box::new(WebFetch));
    r.register(Box::new(WebCrawl));
    register_skill_load(&mut r);
    register_skill_registry(&mut r);
    register_telegram(&mut r);
    register_notify(&mut r);
    // W17: a private, per-instance `todo_write` scratch plan (never the top-level TodoWrite — that
    // one owns the process-global list + the user's scroll region, which no sub-agent may touch).
    // Self-contained state means concurrent read-only sub-agents (planner/reviewer fan-out) never
    // race on it either.
    r.register(Box::new(crate::agent::todo::ScopedTodo::new()));
    // LSP READ navigation for sub-agents (same gate as top-level: only when manager is enabled).
    // Symbolic edit (`symbol_replace`/`symbol_insert`) is granted only to write-capable scopes
    // below — putting it here would let planner/reviewer mutate the tree.
    register_subagent_lsp_read(&mut r, root);
    r
}

/// LSP navigation tools shared by every sub-agent when LSP is enabled. Read-only (references /
/// definition / outline / workspace symbol / diagnostics / repo_map). No symbolic edit here.
fn register_subagent_lsp_read(r: &mut ToolRegistry, root: &Path) {
    if !crate::agent::lsp::LSP.is_enabled() {
        return;
    }
    r.register(Box::new(crate::agent::lsp::tools::LspReferences::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::lsp::tools::LspDefinition::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::lsp::tools::LspSymbolBody::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::lsp::tools::LspHover::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::lsp::tools::LspDocumentSymbols::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::lsp::tools::LspWorkspaceSymbol::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::lsp::tools::LspDiagnostics::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::repo_map::RepoMap::new(
        root.to_path_buf(),
    )));
}

/// Symbolic edit tools for write-capable sub-agents (coder / specialist with edit scope). Gated on
/// the same `LSP.is_enabled()` flag as top-level so `/lsp off` also hides them from sub-agents.
fn register_subagent_lsp_write(r: &mut ToolRegistry, root: &Path) {
    if !crate::agent::lsp::LSP.is_enabled() {
        return;
    }
    r.register(Box::new(crate::agent::lsp::tools::SymbolReplace::new(
        root.to_path_buf(),
    )));
    r.register(Box::new(crate::agent::lsp::tools::SymbolInsert::new(
        root.to_path_buf(),
    )));
}

/// Build a READ/WRITE-scoped registry for a sub-agent of the given `role`. NEVER includes the
/// `task` tool → a sub-agent physically cannot dispatch further sub-agents (recursion guard).
/// Scoping is deterministic (documented in `system_prompt.md`):
/// Every role also gets the read-only web research tools (`web_search`/`web_fetch`).
/// - `coder` → all builtins (read/glob/edit/shell + memory + web)
/// - `tester` → read/glob + shell + memory + web (no `file_edit`)
/// - `planner` / `reviewer` / unknown → read-only (read/glob + memory + web)
pub fn role_registry(role: &str, root: &Path) -> ToolRegistry {
    let mut r = subagent_read_only_base(root);
    match role {
        "coder" => {
            r.register(Box::new(FileEdit::new(root.to_path_buf())));
            r.register(Box::new(MultiEdit::new(root.to_path_buf())));
            r.register(Box::new(FileWrite::new(root.to_path_buf())));
            r.register(Box::new(FileMove::new(root.to_path_buf())));
            r.register(Box::new(ShellRun::new(root.to_path_buf())));
            r.register(Box::new(SkillSave));
            register_skill_refine(&mut r);
            register_subagent_lsp_write(&mut r, root);
        }
        "tester" => {
            r.register(Box::new(ShellRun::new(root.to_path_buf())));
        }
        // planner / reviewer / unknown → read-only (already has LSP nav when enabled).
        _ => {}
    }
    r
}

/// Build a tool registry for a dispatched SPECIALIST agent (see [`crate::agents`]). Same read-only
/// base as [`role_registry`], plus a destructive scope derived from the persona's `tools:` frontmatter:
/// - EMPTY `tools:` → **coder scope** (file_edit + multi_edit + file_write + file_move + shell_run +
///   skill_save) — the locked default; no wider than the trusted `coder` sub-agent (the `cmd_guard`
///   floor + per-op approval still apply underneath).
/// - non-empty `tools:` → exactly those, mapped by name (Claude-Code casing accepted via the alias
///   map in [`canonical_subagent_tool`]; duplicates collapsed).
///
/// Capability invariants (a third-party persona body is UNTRUSTED): this NEVER grants `task`
/// (recursion guard) or the top-level-only tools `todo`/`process`/`clarify`/`persona_create`/`mcp_*`
/// — they map to `None` and are silently dropped even if listed. Unknown names are ignored
/// (forward-compatible). Never calls `publish_active_tools` (only the top-level registry does).
pub fn agent_registry(def: &crate::agents::AgentDef, root: &Path) -> ToolRegistry {
    let mut r = subagent_read_only_base(root);
    if def.tools.is_empty() {
        // Locked default: coder scope.
        r.register(Box::new(FileEdit::new(root.to_path_buf())));
        r.register(Box::new(MultiEdit::new(root.to_path_buf())));
        r.register(Box::new(FileWrite::new(root.to_path_buf())));
        r.register(Box::new(FileMove::new(root.to_path_buf())));
        r.register(Box::new(ShellRun::new(root.to_path_buf())));
        r.register(Box::new(SkillSave));
        register_skill_refine(&mut r);
        register_subagent_lsp_write(&mut r, root);
        return r;
    }
    let mut granted: HashSet<&'static str> = HashSet::new();
    let mut has_file_edit = false;
    for raw in &def.tools {
        let Some(canon) = canonical_subagent_tool(raw) else {
            continue; // read-only (already in base), forbidden, or unknown
        };
        if !granted.insert(canon) {
            continue; // dedup
        }
        match canon {
            "file_edit" => {
                r.register(Box::new(FileEdit::new(root.to_path_buf())));
                has_file_edit = true;
            }
            "multi_edit" => r.register(Box::new(MultiEdit::new(root.to_path_buf()))),
            "file_write" => r.register(Box::new(FileWrite::new(root.to_path_buf()))),
            "file_move" => r.register(Box::new(FileMove::new(root.to_path_buf()))),
            "shell_run" => r.register(Box::new(ShellRun::new(root.to_path_buf()))),
            "skill_save" => {
                // A persona granted skill authoring gets the refine companion too (gated on any
                // skill existing — see register_skill_refine), so it can evolve, not just mint.
                r.register(Box::new(SkillSave));
                register_skill_refine(&mut r);
            }
            _ => unreachable!("canonical_subagent_tool only yields grantable destructive tools"),
        }
    }
    // Symbolic edit rides with any file-edit capability (same intent as top-level coder scope).
    if has_file_edit || granted.contains("multi_edit") || granted.contains("file_write") {
        register_subagent_lsp_write(&mut r, root);
    }
    r
}

/// Map a requested tool name (a persona's `tools:` entry, possibly in Claude-Code casing) to the
/// canonical name of a GRANTABLE destructive tool, or `None`. `None` covers three cases, all meaning
/// "don't add it": read-only tools (already in the base — `Read`/`Grep`/`Glob`/…), forbidden tools
/// (`task`/`todo`/`process`/`clarify`/`persona_create`/`mcp_*`), and unknown names. This is the single
/// structural choke-point for the capability invariant: only the grantable destructive tool names
/// (`file_edit`/`multi_edit`/`file_write`/`file_move`/`shell_run`/`skill_save`) can ever be returned,
/// so nothing else can be granted to a specialist.
fn canonical_subagent_tool(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        // editing
        "edit" | "file_edit" | "fileedit" | "str_replace" | "str_replace_editor" => {
            Some("file_edit")
        }
        "multiedit" | "multi_edit" => Some("multi_edit"),
        // whole-file create/overwrite (Claude-Code's "Write" maps here now that file_write exists)
        "write" | "file_write" | "filewrite" | "write_file" | "writefile" | "create"
        | "create_file" => Some("file_write"),
        // rename / move
        "file_move" | "filemove" | "move_file" | "movefile" | "mv_file" | "mv" | "file_rename"
        | "filerename" | "rename_file" | "renamefile" | "rename" => Some("file_move"),
        // shell
        "bash" | "shell" | "shell_run" | "shellrun" | "run" | "run_command" | "terminal" => {
            Some("shell_run")
        }
        // skill authoring
        "skill_save" | "skillsave" => Some("skill_save"),
        // read-only (already in base) / forbidden / unknown → not grantable.
        _ => None,
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn write_effect(root: &Path, raw: Option<&str>) -> WorkspaceEffect {
    let Some(raw) = raw else {
        return WorkspaceEffect::OpaqueWorkspace;
    };
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    let parent = if joined.exists() {
        joined.as_path()
    } else {
        joined.parent().unwrap_or(root)
    };
    match parent.canonicalize() {
        Ok(canon) if canon.starts_with(root) => WorkspaceEffect::Paths,
        _ => WorkspaceEffect::External,
    }
}

/// The DIRECTORY a write will land in — what [`Tool::workspace_target`] hands the checkpoint gate so
/// it looks for a repository where the change actually happens. Mirrors `confine`'s resolution
/// (relative → `root`, absolute as-is) but stops at the parent: a not-yet-created file has no
/// directory of its own, and the parent is what a repo lookup needs anyway. Uncanonicalized —
/// `RepoContext::discover` runs git in it, which resolves the rest.
fn write_target_dir(root: &Path, raw: Option<&str>) -> Option<std::path::PathBuf> {
    let raw = raw?;
    let path = Path::new(raw);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    };
    if joined.is_dir() {
        return Some(joined);
    }
    joined.parent().map(|p| p.to_path_buf())
}

fn move_effect(root: &Path, args: &Value) -> WorkspaceEffect {
    let from = write_effect(root, args.get("from").and_then(|v| v.as_str()));
    let to = write_effect(root, args.get("to").and_then(|v| v.as_str()));
    if matches!(from, WorkspaceEffect::External) || matches!(to, WorkspaceEffect::External) {
        WorkspaceEffect::External
    } else {
        WorkspaceEffect::Paths
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .with_context(|| format!("missing required string arg '{key}'"))
}

/// Resolve `path` (relative → `base`) into an absolute, canonicalized path.
///
/// NOTE: the workspace confinement boundary was REMOVED by user request — file/shell tools may now
/// touch paths ANYWHERE on disk, not just under `base`. `base` is retained only as the anchor for
/// resolving RELATIVE paths (so `foo.rs` still means `<base>/foo.rs` and `../sibling/x` reaches the
/// sibling project the user asked to work in). Absolute paths pass through unchanged. No escape or
/// symlink-target check is performed. SECURITY: any web page / tool output the agent reads can now
/// steer a write or delete to an arbitrary path — this is the accepted trade-off for the removal.
/// `must_exist`: canonicalize the full path (errors if missing); else canonicalize the parent and
/// re-join the file name (so a not-yet-existing target still resolves).
pub(crate) fn confine(base: &Path, path: &str, must_exist: bool) -> Result<PathBuf> {
    let raw = Path::new(path);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        base.join(raw)
    };
    let resolved = if must_exist {
        joined
            .canonicalize()
            .with_context(|| format!("resolving {}", joined.display()))?
    } else {
        let parent = joined.parent().unwrap_or(base);
        let cparent = parent
            .canonicalize()
            .with_context(|| format!("resolving parent of {}", joined.display()))?;
        let fname = joined.file_name().context("path has no file name")?;
        cparent.join(fname)
    };
    Ok(resolved)
}

/// Translate a glob (`*`, `**`, `**/`, `?`) into an anchored regex over `/`-separated paths.
fn glob_to_regex(glob: &str) -> String {
    let chars: Vec<char> = glob.chars().collect();
    let mut re = String::from("^");
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '*' => {
                if i + 1 < chars.len() && chars[i + 1] == '*' {
                    if i + 2 < chars.len() && chars[i + 2] == '/' {
                        re.push_str("(?:.*/)?"); // **/ → zero-or-more dirs
                        i += 3;
                        continue;
                    }
                    re.push_str(".*"); // ** → across dirs
                    i += 2;
                    continue;
                }
                re.push_str("[^/]*"); // * → within a path segment
            }
            '?' => re.push_str("[^/]"),
            '.' | '+' | '(' | ')' | '|' | '^' | '$' | '{' | '}' | '[' | ']' | '\\' => {
                re.push('\\');
                re.push(c);
            }
            _ => re.push(c),
        }
        i += 1;
    }
    re.push('$');
    re
}

/// Compile a glob into a `Regex`, honoring smart-case (`ci` prepends the `(?i)` inline flag). The
/// body from [`glob_to_regex`] is already `^…$`-anchored, so `(?i)^…$` is valid.
fn compile_glob(glob: &str, ci: bool) -> Result<regex::Regex> {
    let body = glob_to_regex(glob);
    let full = if ci { format!("(?i){body}") } else { body };
    regex::Regex::new(&full).context("invalid glob pattern")
}

/// The intended NAME behind a pattern: its last `/`-separated segment with glob metacharacters
/// stripped, lower-cased. `src/**/*.rs` → `.rs`… no — we want the meaningful name, so we take the
/// last segment that still carries literal characters. `**/mini_project` → `mini_project`,
/// `src/**/*.rs` → `.rs` is useless, so fall back to the last segment that ISN'T pure glob. Used to
/// rank hits and to seed the fuzzy fallback.
fn last_literal_segment(pattern: &str) -> String {
    let norm = pattern.replace('\\', "/");
    let strip = |s: &str| s.trim_matches(|c| c == '*' || c == '?').to_string();
    // Walk segments right-to-left; the first one that has a literal (non-glob) character wins.
    for seg in norm.rsplit('/') {
        let lit = strip(seg);
        if !lit.is_empty() {
            return lit.to_ascii_lowercase();
        }
    }
    String::new()
}

// ── file_glob walk engine ────────────────────────────────────────────────────
// A BOUNDED, PARALLEL directory walk (ripgrep's `ignore` crate, same engine as `search_files`)
// replaces the old single-threaded recursive DFS. The old walk had three fatal bugs on a real
// Windows tree: (1) it counted MATCHES not NODES, so a rare-match glob scanned the WHOLE drive
// before returning; (2) no wall-clock or node ceiling, so it could hang for minutes (the model then
// gave up and shelled out to `where`); (3) it followed junctions/symlinks and swallowed errors, so
// a reparse-point loop under `C:\Users` could spin forever. The engine below fixes all three:
// node-count + wall-clock BUDGET (checked per entry), `follow_links(false)` +
// `same_file_system(true)` (no junction loops, no crossing into other volumes), a bounded depth, and
// optional subtree pruning of heavy/system dirs for BROAD (home/drive-wide) searches.

/// Result of a bounded walk: the collected paths and WHY it stopped (so the tool can tell the model
/// the list may be incomplete and it should narrow, instead of trusting a silently-truncated list).
struct WalkOutcome {
    paths: Vec<PathBuf>,
    /// The node or wall-clock budget was exhausted before the tree was fully scanned.
    budget_hit: bool,
    /// The per-walk match cap was reached (more matches almost certainly exist).
    capped: bool,
}

/// Node-count + wall-clock ceiling shared across the parallel walk threads. `tick()` is called once
/// per visited entry; when either limit trips it flips an atomic flag and every thread quits.
struct WalkBudget {
    nodes: AtomicUsize,
    max_nodes: usize,
    deadline: Instant,
    tripped: std::sync::atomic::AtomicBool,
}
impl WalkBudget {
    fn new(max_nodes: usize, max_wall: Duration) -> Self {
        Self {
            nodes: AtomicUsize::new(0),
            max_nodes,
            deadline: Instant::now() + max_wall,
            tripped: std::sync::atomic::AtomicBool::new(false),
        }
    }
    /// Returns `true` while there is budget left; flips `tripped` and returns `false` once spent.
    /// The wall-clock is only polled every 512 nodes (per thread) so `Instant::now()` isn't on the
    /// hot path of every single entry.
    fn alive(&self) -> bool {
        if self.tripped.load(Ordering::Relaxed) {
            return false;
        }
        let n = self.nodes.fetch_add(1, Ordering::Relaxed);
        if n >= self.max_nodes || (n.is_multiple_of(512) && Instant::now() >= self.deadline) {
            self.tripped.store(true, Ordering::Relaxed);
            return false;
        }
        true
    }
    fn is_tripped(&self) -> bool {
        self.tripped.load(Ordering::Relaxed)
    }
}

/// Directory names whose ENTIRE subtree is pruned during a BROAD (home-/drive-wide) walk — package
/// caches, VCS internals, build output, and OS/system folders that never hold user source and would
/// otherwise burn the whole budget. NOT applied to a NARROW anchored walk (there the user pointed at
/// a specific dir and asked to see everything, build output included).
static BROAD_PRUNE: &[&str] = &[
    "node_modules",
    "target",
    ".git",
    ".hg",
    ".svn",
    "vendor",
    "dist",
    "build",
    "out",
    ".cache",
    ".cargo",
    ".rustup",
    ".npm",
    ".gradle",
    ".m2",
    ".nuget",
    ".venv",
    "venv",
    "__pycache__",
    ".next",
    ".nuxt",
    ".terraform",
    ".tox",
    ".idea",
    ".vscode",
    "AppData",
    "$Recycle.Bin",
    "System Volume Information",
    "Windows",
    "WinSxS",
    "Program Files",
    "Program Files (x86)",
    "ProgramData",
    "$WinREAgent",
    ".Trash",
];

/// Run a bounded, parallel walk over `roots`, keeping every entry (file OR directory) for which
/// `keep(&full_path, rel_slashed)` returns `true`. `rel_slashed` is the path relative to the walk
/// root that produced it, forward-slashed (what a glob regex matches against). Each root carries its
/// OWN max depth — the cwd is walked deep, ancestors shallow (so a name-find sees siblings and the
/// ancestor dir itself without re-scanning the whole tree). `prune` toggles the [`BROAD_PRUNE`]
/// subtree skipping (on for a broad name-find, off for a structured anchored glob). `match_cap`
/// bounds how many kept paths we retain (ranking later trims to a display count). All roots SHARE one
/// budget, so the global node/wall-clock ceiling bounds the total work across every root.
fn bounded_walk<F>(
    roots: &[(PathBuf, usize)],
    prune: bool,
    match_cap: usize,
    budget: &WalkBudget,
    keep: F,
) -> WalkOutcome
where
    F: Fn(&Path, &str) -> bool + Sync,
{
    let (tx, rx) = mpsc::channel::<PathBuf>();
    let kept = AtomicUsize::new(0);
    for (root, depth) in roots.iter() {
        if budget.is_tripped() {
            break;
        }
        let mut wb = WalkBuilder::new(root);
        wb.follow_links(false) // never chase junctions/symlinks → no reparse-point loops
            .same_file_system(true) // don't cross into other drives/mounts mid-walk
            .hidden(false) // SEE hidden files/dirs (dotfiles, .env) — the user asked for everything
            .git_ignore(false)
            .git_global(false)
            .git_exclude(false)
            .ignore(false)
            .parents(false)
            .max_depth(Some(*depth));
        let tx = tx.clone();
        let keep = &keep;
        let kept = &kept;
        wb.build_parallel().run(|| {
            let tx = tx.clone();
            let root = root.clone();
            Box::new(move |res| {
                if !budget.alive() {
                    return WalkState::Quit;
                }
                let dent = match res {
                    Ok(d) => d,
                    Err(_) => return WalkState::Continue, // swallow permission/loop errors, keep going
                };
                let path = dent.path();
                let is_dir = dent.file_type().map(|t| t.is_dir()).unwrap_or(false);
                // Prune heavy/system subtrees on a broad walk (depth>0 so we don't prune a root the
                // caller explicitly pointed at). This is the single highest-leverage speed-up.
                if prune && is_dir && dent.depth() > 0 {
                    if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                        if BROAD_PRUNE.iter().any(|b| b.eq_ignore_ascii_case(name)) {
                            return WalkState::Skip;
                        }
                    }
                }
                if kept.load(Ordering::Relaxed) >= match_cap {
                    return WalkState::Quit;
                }
                let rel = path
                    .strip_prefix(&root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if keep(path, &rel) {
                    if kept.fetch_add(1, Ordering::Relaxed) >= match_cap {
                        return WalkState::Quit;
                    }
                    let _ = tx.send(path.to_path_buf());
                }
                WalkState::Continue
            })
        });
    }
    drop(tx);
    let mut paths: Vec<PathBuf> = rx.into_iter().collect();
    paths.sort();
    paths.dedup();
    let capped = kept.load(Ordering::Relaxed) >= match_cap;
    WalkOutcome {
        paths,
        budget_hit: budget.is_tripped(),
        capped,
    }
}

/// Read the user's home directory from the environment WITHOUT pulling the `dirs` crate (its
/// `winsafe`-adjacent deps break our windows-gnu single-static-binary posture). `USERPROFILE` on
/// Windows, `HOME` elsewhere; falls back to the other if the primary is unset.
fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
}

/// Depth the WORKING DIR is walked to on a name-find — deep enough to reach a file buried in a
/// project, bounded so it can't run away.
const SEED_DEPTH_CWD: usize = 40;
/// Depth ANCESTORS + well-known folders are walked to — shallow, so we surface siblings, the ancestor
/// dir itself, and top-of-project files without re-scanning an entire home tree.
const SEED_DEPTH_WIDE: usize = 6;

/// The FAMILIAR-LOCATION seed list for a bare-name find (P1.1), each paired with its OWN walk depth:
/// the working dir (deep), its ancestors (shallow, up to but NOT past home — we never seed a bare
/// drive root, that's the pathology we're killing), and the well-known user folders
/// (Desktop/Documents/Downloads/Projects, incl. the OneDrive-redirected ones Windows creates —
/// shallow). Reduced to a MINIMAL set — any seed that lives inside another is dropped, so we never
/// walk the same subtree twice. This is why a model can ask for a bare name and we find it on the
/// Desktop or one folder up without scanning the entire drive.
fn seed_dirs(root: &Path) -> Vec<(PathBuf, usize)> {
    let home = home_dir();
    let mut seeds: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), SEED_DEPTH_CWD)];
    // Climb ancestors, but STOP before home and before a drive/filesystem root — we must never seed a
    // bare drive root (`C:\`) or home itself, because pruning would then skip AppData/Windows/etc. and
    // the walk would still be enormous. The well-known user folders below (Desktop/Documents/…) are
    // the widen targets instead. Capped at a handful of levels so a deeply-nested cwd doesn't seed a
    // huge ancestor. Each ancestor is walked SHALLOW.
    let mut cur = root.to_path_buf();
    for _ in 0..6 {
        let Some(parent) = cur.parent().map(|p| p.to_path_buf()) else {
            break;
        };
        // Don't seed home or a root: parent-of-parent None means `parent` is a drive/fs root.
        if home.as_deref() == Some(parent.as_path()) || parent.parent().is_none() {
            break;
        }
        seeds.push((parent.clone(), SEED_DEPTH_WIDE));
        cur = parent;
    }
    if let Some(home) = home {
        for sub in [
            "Desktop",
            "Documents",
            "Downloads",
            "Projects",
            "Code",
            "src",
            "dev",
            "repos",
        ] {
            let p = home.join(sub);
            if p.is_dir() {
                seeds.push((p, SEED_DEPTH_WIDE));
            }
        }
        // OneDrive-redirected known folders (Windows silently moves Desktop/Documents there).
        let onedrive = home.join("OneDrive");
        if onedrive.is_dir() {
            for sub in ["Desktop", "Documents"] {
                let p = onedrive.join(sub);
                if p.is_dir() {
                    seeds.push((p, SEED_DEPTH_WIDE));
                }
            }
        }
    }
    minimal_roots(seeds)
}

/// Reduce a seed list to non-overlapping roots. FIRST-SEEN WINS: seeds are proximity-ordered (the cwd
/// is first and is walked DEEP), so we never drop an earlier root in favor of a later one — a later
/// seed is dropped only when it is EQUAL TO or a DESCENDANT OF an already-kept root (already covered
/// downward). A later seed that is an ANCESTOR of a kept root is retained (walked shallow): it surfaces
/// parent/sibling names the deep cwd walk can't reach upward, and the small subtree overlap is bounded
/// by that seed's shallow depth + the shared budget. The old logic dropped the deep cwd whenever an
/// ancestor (e.g. the temp/AppData dir the cwd sits under) came along, so the cwd's own files were
/// walked shallow-and-late and lost to the budget — the exact bug this ordering fixes.
fn minimal_roots(seeds: Vec<(PathBuf, usize)>) -> Vec<(PathBuf, usize)> {
    let seeds: Vec<(PathBuf, usize)> = seeds.into_iter().filter(|(p, _)| p.is_dir()).collect();
    let mut out: Vec<(PathBuf, usize)> = Vec::new();
    for (s, d) in seeds {
        // Drop only if already covered downward by an earlier (higher-proximity) root.
        let covered = out.iter().any(|(o, _)| s == *o || s.starts_with(o));
        if covered {
            continue;
        }
        out.push((s, d));
    }
    out
}

/// Score a candidate path for ranking (P1.3). Higher is better. Combines: filename fuzzy similarity
/// to the needle (Jaro-Winkler, separator-insensitive) · an EXACT-name bonus · proximity to the
/// working dir (shared path-prefix depth) · a small recency nudge from mtime. Used both to order
/// glob hits and to pick the fuzzy-fallback suggestions, so the BEST answer is always line one.
fn score_path(p: &Path, needle: &str, root: &Path, now: std::time::SystemTime) -> f64 {
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let strip = |s: &str| {
        s.chars()
            .filter(|c| !matches!(c, '_' | '-' | ' ' | '.'))
            .collect::<String>()
    };
    let mut score = if needle.is_empty() {
        0.5
    } else {
        let sim = strsim::jaro_winkler(needle, &name)
            .max(strsim::jaro_winkler(&strip(needle), &strip(&name)));
        if name == needle {
            1.5 // exact filename — unbeatable
        } else if name.contains(needle) || strip(&name).contains(&strip(needle)) {
            sim.max(0.95)
        } else {
            sim
        }
    };
    // Proximity: how many leading path components the candidate shares with the working dir. A file
    // right next to the cwd beats an identically-named one three folders away.
    let shared = root
        .components()
        .zip(p.components())
        .take_while(|(a, b)| a == b)
        .count();
    score += (shared as f64) * 0.02;
    // Recency: files touched in the last week get a tiny nudge (breaks ties toward live work).
    if let Ok(meta) = p.metadata() {
        if let Ok(mtime) = meta.modified() {
            if let Ok(age) = now.duration_since(mtime) {
                if age < Duration::from_secs(7 * 24 * 3600) {
                    score += 0.03;
                }
            }
        }
    }
    score
}

/// Smart-case: a glob with NO uppercase letter matches case-INSENSITIVELY (the common intent — a
/// model types `readme.md` and means `README.md`); any uppercase makes the whole match
/// case-sensitive. Mirrors ripgrep/`search_files` ergonomics.
fn smart_case_insensitive(pattern: &str) -> bool {
    !pattern.chars().any(|c| c.is_uppercase())
}

/// Split a glob into (anchor_dir, sub_pattern): the leading run of segments with NO glob
/// metacharacter (`*`/`?`) becomes a directory anchor, the rest is matched relative to it. This is
/// what lets `file_glob` reach OUTSIDE the working dir — `../snakegame/**/*.js` anchors at the
/// sibling project, an absolute `C:/x/**/*.rs` anchors at the drive path. A pure-literal pattern
/// (no glob char) anchors at its parent and matches the final name. `..` and absolute paths pass
/// through; a relative anchor is joined to `root`.
fn glob_anchor(root: &Path, pattern: &str) -> (PathBuf, String) {
    let norm = pattern.replace('\\', "/");
    let segs: Vec<&str> = norm.split('/').collect();
    let glob_idx = segs.iter().position(|s| s.contains('*') || s.contains('?'));
    let (lit_segs, rest_segs): (&[&str], &[&str]) = match glob_idx {
        Some(i) => (&segs[..i], &segs[i..]),
        None if segs.is_empty() => (&[], &[]),
        // Pure literal path: anchor at the parent, match the final segment as a name.
        None => (&segs[..segs.len() - 1], &segs[segs.len() - 1..]),
    };
    let lit_str = lit_segs.join("/");
    let anchor = if lit_str.is_empty() {
        root.to_path_buf()
    } else {
        let lp = Path::new(&lit_str);
        // Absolute (unix `/…`, Windows `C:/…`) → use as-is; otherwise resolve against root (`..` ok).
        let is_abs = lp.is_absolute()
            || lit_str.starts_with('/')
            || (lit_str.len() >= 2 && lit_str.as_bytes()[1] == b':');
        if is_abs {
            lp.to_path_buf()
        } else {
            root.join(lp)
        }
    };
    let anchor = anchor.canonicalize().unwrap_or(anchor);
    (anchor, rest_segs.join("/"))
}

/// Strip a Windows verbatim/extended-length prefix (`\\?\` or `\\?\UNC\`) so a canonicalized path
/// displays as a normal `C:\…` path instead of the `//?/C:/…` a naive `\`→`/` replace would produce.
/// No-op on non-Windows paths. Kept as a plain string transform so it works on the `Cow` we already
/// have without another allocation-path branch.
fn strip_verbatim(s: &str) -> &str {
    s.strip_prefix(r"\\?\UNC\")
        .map(|_| s) // UNC verbatim is rare; leave it (rewriting it to `\\server` is lossy to reason about)
        .unwrap_or_else(|| s.strip_prefix(r"\\?\").unwrap_or(s))
}

/// Display a matched path relative to `root` when it's under the working dir, else as a cleaned
/// absolute path (so results outside the workspace are still copy-pasteable). Windows verbatim
/// prefixes (`\\?\`, left behind by `canonicalize`) are stripped so the model sees `C:/…`, not `//?/C:/…`.
fn display_path(root: &Path, p: &Path) -> String {
    // Compare on the verbatim-stripped forms so a canonicalized `p` still strips against a plain `root`.
    let ps = p.to_string_lossy();
    let ps = strip_verbatim(&ps);
    let rs = root.to_string_lossy();
    let rs = strip_verbatim(&rs);
    let rel = ps
        .strip_prefix(rs)
        .map(|r| r.trim_start_matches(['\\', '/']))
        .filter(|r| !r.is_empty() && ps.len() != rs.len());
    match rel {
        Some(r) => r.replace('\\', "/"),
        None => ps.replace('\\', "/"),
    }
}

// ── memory_search ──────────────────────────────────────────────────────────

struct MemorySearch;
impl Tool for MemorySearch {
    fn name(&self) -> &str {
        "memory_search"
    }
    fn description(&self) -> &str {
        "Find a stored fact about the user or project by lexical/semantic match. Use to recall a \
         specific past fact — project knowledge lives HERE (not in the always-on <user_memory> \
         block, which only holds STYLE + global prefs). Not for the user's overall preferences → \
         use memory_profile. Searches the current workspace + global facts by default. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "what to recall"},
                "limit": {"type": "integer", "description": "max hits (default 5)"},
                "scope": {"type": "string", "enum": ["current", "all", "global"], "description": "zones to search: current project + global (default), all zones, or global-only"},
                "category": {"type": "string", "enum": ["bug-history", "failed-attempt", "success-pattern", "arch-decision", "command", "security-rule", "deploy-note", "codebase"], "description": "restrict to one KIND of project knowledge (optional) — e.g. only past bugs, or only what previously FAILED so you don't retry a dead end"}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    // NOT concurrency-safe: memory::search has a WRITE side effect (record_reuse → record_retrieval,
    // a read-modify-write of per-fact files, plus the shared embedding cache under `dense`). Two
    // parallel searches would race and lose reinforcement counts / clobber the cache, so keep it on
    // the serial path even though it reads like a read-only tool.
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let query = str_arg(args, "query")?;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(5)
            .clamp(1, 20) as usize;
        let sel = match args
            .get("scope")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("current")
        {
            "" | "current" => crate::memory::ScopeSel::default_view(),
            "all" => crate::memory::ScopeSel::All,
            "global" => crate::memory::ScopeSel::Global,
            other => {
                return Ok(format!(
                    "error: unknown scope '{other}' (use current|all|global)"
                ))
            }
        };
        let cat = match args
            .get("category")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => None,
            Some(s) => match crate::memory::category::Category::parse(s) {
                Some(c) => Some(c),
                None => return Ok(format!("error: unknown category '{s}'")),
            },
        };
        // record_reuse (the reinforcement write side-effect) is intentionally kept on the unfiltered
        // `search_scoped` path; a category-filtered recall is an inspection, not organic reuse.
        let hits = if cat.is_some() {
            crate::memory::search_filtered_scoped_cat(query, limit, None, cat, &sel)?
        } else {
            crate::memory::search_scoped(query, limit, &sel)?
        };
        if hits.is_empty() {
            return Ok(format!("(no memory matches '{query}')"));
        }
        Ok(format_memory_hits(
            &hits,
            crate::memory::settings().search_max_tokens,
        ))
    }
}

/// Render search hits under a hard token budget (chars/4, same estimator as the frozen core):
/// hits are already best-first, so trailing ones are cut and counted rather than blowing the turn.
fn format_memory_hits(hits: &[crate::memory::Hit], max_tokens: usize) -> String {
    let mut s = String::new();
    let mut used = 0usize;
    for (shown, h) in hits.iter().enumerate() {
        let body: String = h.entry.body.chars().take(200).collect();
        let zone = match h.entry.scope.as_deref() {
            Some(z) => format!(" [p:{z}]"),
            None => String::new(),
        };
        // Surface the CoALA content type when the fact HAS one, tagged with its neural bucket
        // (episodic/semantic/procedural) so the agent can weight a past bug vs a durable decision.
        let cat = match h.entry.category {
            crate::memory::category::Category::None => String::new(),
            c => format!(" [{}/{}]", c.kind().as_str(), c.as_str()),
        };
        let line = format!(
            "[{:.2}] {} ({}){}{} — {}\n",
            h.score,
            h.entry.name,
            h.entry.mtype.as_str(),
            zone,
            cat,
            body.replace('\n', " ")
        );
        let cost = crate::memory::render::est_tokens(&line);
        if used + cost > max_tokens && shown > 0 {
            s.push_str(&format!(
                "(+{} more hit(s) over the token budget — narrow the query)\n",
                hits.len() - shown
            ));
            break;
        }
        used += cost;
        s.push_str(&line);
    }
    s.trim_end().to_string()
}

// ── memory_profile (B2) ──────────────────────────────────────────────────────

struct MemoryProfile;
impl Tool for MemoryProfile {
    fn name(&self) -> &str {
        "memory_profile"
    }
    fn description(&self) -> &str {
        "The user's aggregated working profile (verbosity / autonomy / tooling / stack / \
         language / frustrations) with per-dimension confidence + cited facts. Use to decide \
         defaults. Not a single-fact lookup → use memory_search. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    fn execute(&self, _args: &Value) -> Result<String> {
        let p = crate::memory::build_profile()?;
        Ok(serde_json::to_string(&p)?)
    }
}

// ── memory_ask (B3 dialectic) ─────────────────────────────────────────────────

struct MemoryAsk;
impl Tool for MemoryAsk {
    fn name(&self) -> &str {
        "memory_ask"
    }
    fn description(&self) -> &str {
        "Answer ONE specific question about the user from memory; ABSTAINS (says it can't tell) \
         rather than guessing. Use for 'what would the user prefer here' decisions. Not a general \
         fact search → use memory_search. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {"question": {"type": "string"}},
            "required": ["question"],
            "additionalProperties": false
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let q = str_arg(args, "question")?;
        let a = crate::memory::answer_about_user(q)?;
        let mut s = a.text.clone();
        if !a.basis.is_empty() {
            let cited: Vec<String> = a.basis.iter().take(3).map(|b| b.name.clone()).collect();
            s.push_str(&format!("\n(based on: {})", cited.join("; ")));
        }
        Ok(s)
    }
}

// ── memory_list ───────────────────────────────────────────────────────────────

/// Inventory of what is stored, WITHOUT a query. `memory_search` needs a query, so before this tool
/// existed the agent had no way to answer "what do you remember about me?" — it could only guess
/// query terms and report whatever happened to match, which reads as confidently not knowing its
/// own state. This is the tool that makes the store legible.
struct MemoryList;
impl Tool for MemoryList {
    fn name(&self) -> &str {
        "memory_list"
    }
    fn description(&self) -> &str {
        "Inventory the stored facts (id · type · zone · category · one-line summary) with NO query \
         — use to answer 'what do you remember?', to audit what's saved before editing/forgetting, \
         or to find the exact id `memory_update`/`memory_forget` needs. Not for finding one fact by \
         topic → use memory_search. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "scope": {"type": "string", "enum": ["current", "all", "global", "project"], "description": "zones to list: current project + global (default), every zone, global-only, or this project only"},
                "type": {"type": "string", "enum": ["user", "feedback", "project", "reference"], "description": "restrict to one memory type (optional)"},
                "limit": {"type": "integer", "description": "max entries (default 50, max 200)"},
                "include_archived": {"type": "boolean", "description": "list the recoverable archive instead of the live store (default false)"}
            },
            "additionalProperties": false
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(50)
            .clamp(1, 200) as usize;
        let archived = args
            .get("include_archived")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mtype = match args
            .get("type")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => None,
            Some(s) => match crate::memory::store::MemoryType::parse_strict(s) {
                Some(t) => Some(t),
                None => {
                    return Ok(format!(
                        "error: unknown type '{s}' (user|feedback|project|reference)"
                    ))
                }
            },
        };
        let sel = match args
            .get("scope")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .unwrap_or("current")
        {
            "" | "current" => crate::memory::ScopeSel::default_view(),
            "all" => crate::memory::ScopeSel::All,
            "global" => crate::memory::ScopeSel::Global,
            "project" => crate::memory::ScopeSel::Project(crate::core::config::project_slug()),
            other => {
                return Ok(format!(
                    "error: unknown scope '{other}' (use current|all|global|project)"
                ))
            }
        };
        Ok(crate::memory::inventory(&sel, mtype, limit, archived)?)
    }
}

// ── memory_save ───────────────────────────────────────────────────────────────

/// Deliberate write. Distinct from the passive learning pipeline: when the user says "remember
/// this", the agent should be able to act on it in the same turn instead of hoping an extractor
/// picks it up later.
struct MemorySave;
impl Tool for MemorySave {
    fn name(&self) -> &str {
        "memory_save"
    }
    fn description(&self) -> &str {
        "Store ONE durable fact the user asked you to remember, or a project fact worth keeping \
         across sessions. Check memory_list/memory_search FIRST — if a fact on this topic already \
         exists, use memory_update instead of adding a near-duplicate. Not for scratch notes within \
         one turn. Defaults to the current project zone; set scope:'global' for a fact true \
         everywhere."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "short, unique, kebab-case-ish title — becomes the id"},
                "body": {"type": "string", "description": "the fact itself, self-contained (a reader with no chat context must understand it); use absolute dates, not 'yesterday'"},
                "description": {"type": "string", "description": "one-line summary used for recall ranking (optional)"},
                "type": {"type": "string", "enum": ["user", "feedback", "project", "reference"], "description": "user = who they are; feedback = how they want you to work; project = this codebase's state/goals; reference = external pointers. Default project."},
                "scope": {"type": "string", "description": "'global' for a fact true in every workspace; omit (or 'project') to scope it to the current project"}
            },
            "required": ["name", "body"],
            "additionalProperties": false
        })
    }
    // Writes a file + is the kind of thing the user wants to see; keep it off the parallel path.
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let name = str_arg(args, "name")?;
        let body = str_arg(args, "body")?;
        if body.trim().is_empty() {
            return Ok("error: empty body — nothing to remember".to_string());
        }
        let desc = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let t = match args
            .get("type")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => crate::memory::store::MemoryType::Project,
            Some(s) => match crate::memory::store::MemoryType::parse_strict(s) {
                Some(t) => t,
                None => {
                    return Ok(format!(
                        "error: unknown type '{s}' (user|feedback|project|reference)"
                    ))
                }
            },
        };
        // Global only when explicitly asked; anything else (absent, "project", "current") stays in
        // this workspace's zone — a fact learned here must not leak into unrelated projects.
        let scope = match args.get("scope").and_then(|v| v.as_str()).map(str::trim) {
            Some(s) if s.eq_ignore_ascii_case("global") => None,
            _ => Some(crate::core::config::project_slug()),
        };
        match crate::memory::store::add_scoped(name, desc, t, body.trim(), scope.as_deref()) {
            Ok(id) => Ok(format!(
                "saved memory '{id}' (type={}, {}). Use memory_update to revise it.",
                t.as_str(),
                scope
                    .as_deref()
                    .map(|s| format!("zone {s}"))
                    .unwrap_or_else(|| "global".into())
            )),
            // The store refuses to overwrite; surface the "update instead" path rather than failing.
            Err(e) => Ok(format!("error: {e}")),
        }
    }
}

// ── memory_update ─────────────────────────────────────────────────────────────

struct MemoryUpdate;
impl Tool for MemoryUpdate {
    fn name(&self) -> &str {
        "memory_update"
    }
    fn description(&self) -> &str {
        "Revise a stored fact in place by id — correct it, sharpen the wording, retype it, or move \
         it between global/project scope. Only the fields you pass change; the id and every other \
         field stay put. Use this instead of saving a second, contradictory fact on the same topic. \
         Get ids from memory_list."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "the fact's id (or exact name) from memory_list"},
                "body": {"type": "string", "description": "replacement body — the corrected fact, in full"},
                "description": {"type": "string", "description": "replacement one-liner (empty string clears it)"},
                "name": {"type": "string", "description": "replacement display title (the id does NOT change)"},
                "type": {"type": "string", "enum": ["user", "feedback", "project", "reference"]},
                "scope": {"type": "string", "description": "'global' to make it apply everywhere, or 'project' to scope it to the current workspace"}
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let id = str_arg(args, "id")?;
        let mtype = match args
            .get("type")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            None => None,
            Some(s) => match crate::memory::store::MemoryType::parse_strict(s) {
                Some(t) => Some(t),
                None => {
                    return Ok(format!(
                        "error: unknown type '{s}' (user|feedback|project|reference)"
                    ))
                }
            },
        };
        let scope = args
            .get("scope")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .map(|s| {
                if s.eq_ignore_ascii_case("global") || s.is_empty() {
                    None
                } else {
                    Some(crate::core::config::project_slug())
                }
            });
        let patch = crate::memory::store::EntryPatch {
            name: args
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            description: args
                .get("description")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            mtype,
            body: args
                .get("body")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            scope,
            preserve_updated: false, // a content update IS a touch — stamp the aging clock
            // Not exposed to the model: dropping a `supersedes:` claim revives a retired fact, which
            // is `aizen memory revive` — a human decision with its own audit line, not a field an
            // edit tool gets to flip as a side effect of rewording a body.
            clear_supersede: false,
        };
        if patch.is_empty() {
            return Ok(
                "error: nothing to update — pass at least one of body/description/name/type/scope"
                    .to_string(),
            );
        }
        let e = match crate::memory::resolve_entry(id) {
            Ok(e) => e,
            Err(err) => return Ok(format!("error: {err}")),
        };
        match crate::memory::store::update(&e, &patch) {
            Ok(()) => Ok(format!("updated memory '{}'", e.id)),
            Err(err) => Ok(format!("error: {err}")),
        }
    }
}

// ── memory_forget ─────────────────────────────────────────────────────────────

/// Retire a fact. Deliberately a SOFT delete (archive, restorable by the human) and
/// approval-gated: the store's premise is that a fact is never lost, and a model concluding
/// something is obsolete is exactly the case where that premise earns its keep.
struct MemoryForget;
impl Tool for MemoryForget {
    fn name(&self) -> &str {
        "memory_forget"
    }
    fn description(&self) -> &str {
        "Retire a stored fact the user says is wrong or obsolete. Moves it to a recoverable archive \
         (the user can restore it) — it is NOT erased. Prefer memory_update when the fact is merely \
         out of date, and only forget when it should stop applying entirely. Get ids from memory_list."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "the fact's id (or exact name) from memory_list"},
                "reason": {"type": "string", "description": "why it should stop applying (shown to the user in the approval prompt)"}
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let id = str_arg(args, "id")?;
        let e = match crate::memory::resolve_entry(id) {
            Ok(e) => e,
            Err(err) => return Ok(format!("error: {err}")),
        };
        match crate::memory::store::retire(&e) {
            Ok(archived) => Ok(format!(
                "retired memory '{}' → archived as '{archived}' (user can restore: `aizen memory restore {archived}`)",
                e.id
            )),
            Err(err) => Ok(format!("error: {err}")),
        }
    }
}

// ── file_read ──────────────────────────────────────────────────────────────

struct FileRead {
    root: PathBuf,
}
impl FileRead {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}
impl Tool for FileRead {
    fn name(&self) -> &str {
        "file_read"
    }
    fn description(&self) -> &str {
        "Read a file (optionally a 1-based line range). Use before editing. Set number:true to \
         prefix each line with its 1-based number (`N|line`) for orientation — leave it off (the \
         default) when you'll feed the text back into file_edit's old_string. A relative path \
         resolves under the working directory; an absolute path or `../` reads elsewhere too. \
         To learn a file's shape use lsp_document_symbols, and to read ONE named item use \
         read_symbol — both are far cheaper than dumping the whole file here. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "start": {"type": "integer", "description": "1-based first line (optional)"},
                "end": {"type": "integer", "description": "1-based last line (optional)"},
                "number": {"type": "boolean", "description": "prefix each line with `N|` (default false)"}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let path = str_arg(args, "path")?;
        let resolved = confine(&self.root, path, true)?;
        let content = std::fs::read_to_string(&resolved)
            .with_context(|| format!("reading {}", resolved.display()))?;
        let start = args.get("start").and_then(|v| v.as_u64());
        let end = args.get("end").and_then(|v| v.as_u64());
        let number = args
            .get("number")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        // The whole file — verbatim under budget (the common case; keeps old_string round-trips
        // byte-exact), or a clearly-marked head+tail preview when it's pathologically large.
        if start.is_none() && end.is_none() && !number {
            return Ok(budget_view(
                &content,
                path,
                FILE_READ_MAX_LINES,
                FILE_READ_MAX_BYTES,
            ));
        }
        let lines: Vec<&str> = content.lines().collect();
        let s = start.unwrap_or(1).max(1) as usize;
        let e = (end.unwrap_or(lines.len() as u64) as usize).min(lines.len());
        if s > e {
            return Ok(String::new());
        }
        if number {
            let body = lines[s - 1..e]
                .iter()
                .enumerate()
                .map(|(i, l)| format!("{}|{l}", s + i))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(body)
        } else {
            Ok(lines[s - 1..e].join("\n"))
        }
    }
}

/// (start,end) byte span per line; `end` EXCLUDES the trailing `\n` (same scan as
/// `indent_tolerant_blocks`). The final span runs to `content.len()`.
fn line_byte_spans(content: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in content.char_indices() {
        if ch == '\n' {
            spans.push((start, idx));
            start = idx + 1;
        }
    }
    spans.push((start, content.len()));
    spans
}

/// First `max` bytes of `s`, snapped DOWN to a char boundary (never panics on multibyte).
fn take_bytes_prefix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}
/// Last `max` bytes of `s`, snapped UP to a char boundary.
fn take_bytes_suffix(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    &s[start..]
}

/// Bound a WHOLE-file read: under both caps → verbatim; over → head+tail with a LOUD marker stating
/// the total size + exact shown ranges, so the model knows it has a partial view (and won't feed the
/// spliced text back as an exact `old_string` — a slice is either fully inside head/tail, which
/// round-trips, or spans the omitted sentinel, which is a clean exact-miss). Slices are byte-exact
/// (real line endings preserved — NOT the lossy `lines()` path). `max_*`=0 disables that cap.
fn budget_view(content: &str, path: &str, max_lines: usize, max_bytes: usize) -> String {
    let total_bytes = content.len();
    let spans = line_byte_spans(content);
    let total_lines = spans.len();
    let over_lines = max_lines > 0 && total_lines > max_lines;
    let over_bytes = max_bytes > 0 && total_bytes > max_bytes;
    if !over_lines && !over_bytes {
        return content.to_string();
    }
    let kb = total_bytes / 1024;
    // Per-slice byte clamp (a giant line inside the head/tail window can't blow the budget).
    let bcap = if max_bytes > 0 {
        (max_bytes / 2).max(1)
    } else {
        usize::MAX
    };
    if over_lines {
        let half = (max_lines / 2).max(1);
        let head = take_bytes_prefix(&content[..spans[half].0], bcap);
        let tail = take_bytes_suffix(&content[spans[total_lines - half].0..], bcap);
        let tail_first = total_lines - half + 1;
        let omitted = total_lines - 2 * half;
        format!(
            "[file_read: {path} is {total_lines} lines ({kb} KB) — over the {max_lines}-line read \
             budget. Showing lines 1-{half} and {tail_first}-{total_lines}; pass start/end (e.g. \
             start:{}, end:{}) or use search_files to read the omitted middle.]\n{head}\n…[{omitted} \
             lines omitted: {}-{}]…\n{tail}",
            half + 1,
            half + 500,
            half + 1,
            tail_first - 1,
        )
    } else {
        // Byte-dominated (a few very long lines): split by bytes, char-safe.
        let half = (max_bytes / 2).max(1);
        let head = take_bytes_prefix(content, half);
        let tail = take_bytes_suffix(content, half);
        let omitted = total_bytes.saturating_sub(head.len() + tail.len());
        format!(
            "[file_read: {path} is {kb} KB across {total_lines} line(s) — over the {} KB read budget. \
             Showing the first and last {} KB; use start/end or search_files for a specific part.]\n\
             {head}\n…[{omitted} bytes omitted]…\n{tail}",
            max_bytes / 1024,
            (half / 1024).max(1),
        )
    }
}

// ── file_glob ──────────────────────────────────────────────────────────────

struct FileGlob {
    root: PathBuf,
}
impl FileGlob {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}
impl Tool for FileGlob {
    fn name(&self) -> &str {
        "file_glob"
    }
    fn description(&self) -> &str {
        "Find files AND directories by NAME or glob (*, **, ?). This is the RIGHT tool for locating \
         a file or folder — do NOT shell out to `where`, `dir /s`, `Get-ChildItem -Recurse`, `find`, \
         or `fd` (they hang on large trees and aren't installed everywhere; this is faster and always \
         present). Give a bare name (`Cargo.toml`, `snake_game.js`) or a glob (`src/**/*.rs`, \
         `**/mini_project`); matching is case-insensitive unless your pattern has an uppercase \
         letter. It automatically searches the working dir, its parent folders, and your \
         Desktop/Documents/Downloads/home — so a bare name is found even when it lives above the cwd \
         or on the Desktop, without you naming the path. A leading `../` or an absolute path \
         (`C:/…`) targets a specific place. Results are RANKED best-first (line 1 is the most likely \
         answer). If nothing matches exactly it falls back to the closest names (typo- and \
         `_`/`-`/space-tolerant). Hidden files are included; heavy/system dirs (node_modules, target, \
         .git, Windows, AppData…) are skipped on a broad search. Not for file CONTENT → use \
         search_files. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {"pattern": {"type": "string", "description": "e.g. src/**/*.rs, ../sibling/**/*.py, or a bare name like 'confg.toml' for a fuzzy lookup"}},
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let pattern = str_arg(args, "pattern")?;
        let now = std::time::SystemTime::now();
        // Split into a literal directory anchor + the glob remainder, so `../x/**/*.rs` and
        // `C:/abs/**/*.rs` reach a SPECIFIC place. A bare-name / bare-`**/name` pattern (no explicit
        // directory the user pointed at) is a BROAD search: we walk the familiar-location seeds
        // (cwd → ancestors → Desktop/Documents/Downloads/home) so the file is found even when it
        // lives above the cwd — the whole reason a model no longer needs to shell out to `where`.
        let (anchor, sub) = glob_anchor(&self.root, pattern);
        let sub_re = if sub.is_empty() {
            pattern.replace('\\', "/")
        } else {
            sub.clone()
        };
        // STRUCTURED vs BARE-NAME. A pattern with a path SEPARATOR, a glob metacharacter (`*`/`?`), a
        // `..`, or an absolute prefix is a deliberate STRUCTURED query → walk just its anchor DEEP with
        // NO pruning (the caller pointed somewhere specific and wants everything under it, build output
        // included — this is what `**/*.rs`, `src/**/*.ts`, `../sib/**/*.js` need). A pattern that is a
        // BARE NAME (`Cargo.toml`, `miniproject`) is a "find this by name" → walk the familiar-location
        // seeds (cwd deep, ancestors + Desktop/Documents/… shallow) WITH heavy/system-dir pruning and
        // ranking, so it's found even above the cwd without scanning the whole drive.
        let norm = pattern.replace('\\', "/");
        let structured =
            norm.contains('/') || norm.contains('*') || norm.contains('?') || anchor != self.root;
        let narrow = structured;
        let ci = smart_case_insensitive(&sub_re);
        let re = compile_glob(&sub_re, ci)?;

        // ── exact glob pass ──────────────────────────────────────────────────
        // NARROW: walk just the anchor deep (no pruning). BROAD: walk the seed roots (cwd deep,
        // ancestors + familiar folders shallow) with subtree pruning of heavy/system dirs. Match the
        // entry's path RELATIVE to the walk root against the glob; for a broad walk we also test the
        // BASENAME so a bare `**/*.rs`-style regex matches by name regardless of how deep the seed sits.
        let roots: Vec<(PathBuf, usize)> = if narrow {
            vec![(anchor.clone(), SEED_DEPTH_CWD)]
        } else {
            seed_dirs(&self.root)
        };
        let budget = WalkBudget::new(
            if narrow { 400_000 } else { 250_000 },
            Duration::from_millis(if narrow { 4000 } else { 2500 }),
        );
        let re_ref = &re;
        let outcome = bounded_walk(&roots, !narrow, 2000, &budget, |p, rel| {
            if re_ref.is_match(rel) {
                return true;
            }
            // On a broad seed walk the glob is usually name-only; also match the basename so
            // `**/foo` and `foo*` hit regardless of how deep the seed root sits.
            if !narrow {
                if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                    return re_ref.is_match(name);
                }
            }
            false
        });
        if !outcome.paths.is_empty() {
            // Rank best-first so line 1 is the most likely answer (P1.3). The needle for scoring is
            // the last literal segment of the pattern (the intended name).
            let needle = last_literal_segment(pattern);
            let mut ranked = outcome.paths;
            ranked.sort_by(|a, b| {
                score_path(b, &needle, &self.root, now)
                    .partial_cmp(&score_path(a, &needle, &self.root, now))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let shown = 50.min(ranked.len());
            let mut lines: Vec<String> = ranked[..shown]
                .iter()
                .map(|p| {
                    let s = display_path(&self.root, p);
                    if p.is_dir() {
                        format!("{s}/")
                    } else {
                        s
                    }
                })
                .collect();
            // Truncation / budget flags TEACH the model to narrow instead of trusting a partial list.
            if ranked.len() > shown {
                lines.push(format!(
                    "…[{} more — showing the {shown} best; narrow the pattern to see the rest]",
                    ranked.len() - shown
                ));
            } else if outcome.capped {
                lines.push(
                    "…[result cap reached — more matches exist; narrow the pattern]".to_string(),
                );
            } else if outcome.budget_hit {
                lines.push("…[search budget reached before the whole tree was scanned — narrow the pattern or pass a ../ or absolute path to search a specific place]".to_string());
            }
            return Ok(lines.join("\n"));
        }

        // ── fuzzy fallback on the NAME ───────────────────────────────────────
        // No exact glob hit → find the closest names (typo / separator tolerance). Reuse the SAME
        // bounded walk over the same roots (P1.4) — no second bespoke recursive scan.
        let needle = last_literal_segment(pattern);
        let needle = needle
            .trim_matches(|c| c == '*' || c == '?')
            .to_ascii_lowercase();
        if needle.is_empty() {
            return Ok(format!(
                "no files matched '{pattern}' (give a name or a glob like src/**/*.rs)"
            ));
        }
        let strip = |s: &str| {
            s.chars()
                .filter(|c| !matches!(c, '_' | '-' | ' ' | '.'))
                .collect::<String>()
        };
        let needle_ref = &needle;
        let fuzzy_budget = WalkBudget::new(
            if narrow { 400_000 } else { 250_000 },
            Duration::from_millis(if narrow { 4000 } else { 2500 }),
        );
        let pool = bounded_walk(&roots, !narrow, 6000, &fuzzy_budget, |p, _rel| {
            match p.file_name().and_then(|n| n.to_str()) {
                Some(name) => {
                    let name = name.to_ascii_lowercase();
                    name.contains(needle_ref)
                        || strip(&name).contains(&strip(needle_ref))
                        || strsim::jaro_winkler(needle_ref, &name) >= 0.82
                }
                None => false,
            }
        });
        let mut scored: Vec<(f64, PathBuf)> = pool
            .paths
            .into_iter()
            .map(|p| (score_path(&p, &needle, &self.root, now), p))
            .filter(|(s, _)| *s >= 0.72)
            .collect();
        if scored.is_empty() {
            return Ok(format!("no file or folder named like '{pattern}' found nearby (searched the working dir, its parents, and your Desktop/Documents/Downloads). Pass a ../ or absolute path to search a specific place."));
        }
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(15);
        // Lead with a NEUTRAL header (never negative prose that reads like a failure) and tag folders.
        let lines: Vec<String> = scored
            .iter()
            .map(|(_, p)| {
                let s = display_path(&self.root, p);
                if p.is_dir() {
                    format!("{s}/  (folder)")
                } else {
                    s
                }
            })
            .collect();
        Ok(format!(
            "closest matches for '{pattern}' (ranked):\n{}",
            lines.join("\n")
        ))
    }
}

// ── file_edit ────────────────────────────────────────────────────────────────

struct FileEdit {
    root: PathBuf,
}
impl FileEdit {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}
impl Tool for FileEdit {
    fn name(&self) -> &str {
        "file_edit"
    }
    fn description(&self) -> &str {
        "Edit a file by exact string replacement (or create one when old_string is empty and the \
         file does not exist). old_string must be unique unless replace_all. If the exact text \
         isn't found, a whitespace/indentation-tolerant retry is attempted for a single matching \
         block. To create OR fully rewrite a whole file, use file_write instead. Returns a \
         before→after preview. Read the file first. Relative paths resolve under the working \
         directory; an absolute path or a leading `../` may write elsewhere on disk."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_string": {"type": "string", "description": "exact text to replace; empty = create new file"},
                "new_string": {"type": "string"},
                "replace_all": {"type": "boolean"}
            },
            "required": ["path", "new_string"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, args: &Value) -> WorkspaceEffect {
        write_effect(&self.root, args.get("path").and_then(|v| v.as_str()))
    }
    fn workspace_target(&self, args: &Value) -> Option<std::path::PathBuf> {
        write_target_dir(&self.root, args.get("path").and_then(|v| v.as_str()))
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let path = str_arg(args, "path")?;
        let new = str_arg(args, "new_string")?;
        let old = args
            .get("old_string")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let replace_all = args
            .get("replace_all")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if old.is_empty() {
            // create-new path
            let target = confine(&self.root, path, false)?;
            if target.exists() {
                bail!("{path} exists; provide old_string to edit it, or use file_write to overwrite the whole file");
            }
            crate::core::persist::create_if_absent(&target, new.as_bytes())
                .with_context(|| format!("creating {}", target.display()))?;
            return Ok(format!("created {path}"));
        }

        let target = confine(&self.root, path, true)?;
        let (content_bytes, expected) = crate::core::persist::read_with_fingerprint(&target)?;
        let content = String::from_utf8(content_bytes.context("file disappeared while reading")?)
            .with_context(|| format!("{} is not valid UTF-8", target.display()))?;
        let applied = apply_one_edit(&content, old, new, replace_all, path)?;
        // No-op guard: a match that produced byte-identical content (e.g. old_string == new_string)
        // must not touch disk or arm the verify gate (W16).
        if applied.content == content {
            return Ok(format!(
                "{NOOP_WRITE_PREFIX}: {path} unchanged (old_string == new_string)"
            ));
        }
        crate::core::persist::compare_and_atomic_write(
            &target,
            &expected,
            applied.content.as_bytes(),
        )
        .with_context(|| format!("writing {}", target.display()))?;
        let mut out = format!(
            "edited {path} ({})\n{}",
            applied.summary(),
            diff_preview(&applied.before, &applied.after)
        );
        // Post-edit LSP fold: NEW diagnostics land in THIS result (zero extra round-trips to
        // discover breakage). Fail-soft + hard-capped inside; a no-op when LSP is off.
        if let Some(fb) = crate::agent::lsp::LSP.edit_feedback(&target) {
            out.push('\n');
            out.push_str(&fb);
        }
        Ok(out)
    }
}

// ── file_write ─────────────────────────────────────────────────────────────

/// Create a new file OR completely overwrite an existing one in a single call. This is the tool a
/// model reaches for when it wants to "write the file from scratch". WITHOUT it, models fall back to
/// shelling out (`type NUL > f` to blank the file, then a heredoc to refill it) — which destroys the
/// file and then fails, the exact thrash this tool removes. `file_edit` stays the right tool for a
/// localized change; `file_write` is for whole-file create/replace.
struct FileWrite {
    root: PathBuf,
}
impl FileWrite {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}
impl Tool for FileWrite {
    fn name(&self) -> &str {
        "file_write"
    }
    fn description(&self) -> &str {
        "Create a file, or COMPLETELY overwrite an existing one, with the given content — the whole \
         file in one call. Use this to write a new file, or to rewrite a file from scratch. NEVER \
         blank or build files with shell (`type NUL > f`, `> f`, `echo >`, heredocs) — use this \
         tool. For a small change to an existing file, prefer file_edit. The parent directory must \
         already exist. A relative path resolves under the working directory; an absolute path or \
         a leading `../` may write ANYWHERE on disk."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "content": {"type": "string", "description": "the full file content to write"}
            },
            "required": ["path", "content"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, args: &Value) -> WorkspaceEffect {
        write_effect(&self.root, args.get("path").and_then(|v| v.as_str()))
    }
    fn workspace_target(&self, args: &Value) -> Option<std::path::PathBuf> {
        write_target_dir(&self.root, args.get("path").and_then(|v| v.as_str()))
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let path = str_arg(args, "path")?;
        let content = str_arg(args, "content")?;
        // must_exist = false → create-or-overwrite; `confine` still keeps the target inside root and
        // requires the PARENT dir to exist (a clear error the model can act on if it doesn't).
        let target = confine(&self.root, path, false)?;
        let (before_bytes, expected) = crate::core::persist::read_with_fingerprint(&target)?;
        let existed = expected.exists;
        let before = match before_bytes {
            Some(bytes) => String::from_utf8(bytes)
                .with_context(|| format!("{} is not valid UTF-8", target.display()))?,
            None => String::new(),
        };
        // No-op guard: an overwrite that changes nothing must not touch disk (no mtime churn, no
        // needless git-diff noise) and must not arm the verify gate (W16). A create with empty
        // content is a real op (the file did not exist), so gate on `existed`.
        if existed && before == content {
            return Ok(format!(
                "{NOOP_WRITE_PREFIX}: {path} already holds this exact content"
            ));
        }
        crate::core::persist::compare_and_atomic_write(&target, &expected, content.as_bytes())
            .with_context(|| format!("writing {}", target.display()))?;
        let n = content.lines().count();
        let verb = if existed { "overwrote" } else { "created" };
        let mut out = format!("{verb} {path} ({n} line(s))");
        // On an overwrite, show what changed so the human (and the model) sees the diff. Skipped for
        // a brand-new file or a no-op rewrite.
        if existed && before != content {
            out.push('\n');
            out.push_str(&diff_preview(&before, content));
        }
        // Same post-write LSP fold as file_edit — surface new diagnostics in this result.
        if let Some(fb) = crate::agent::lsp::LSP.edit_feedback(&target) {
            out.push('\n');
            out.push_str(&fb);
        }
        Ok(out)
    }
}

// ── file_move ──────────────────────────────────────────────────────────────

/// Rename or move a file or directory in one call — the tool a model reaches for instead of shelling
/// out to `mv`/`move`/`Rename-Item` (which vary by OS and silently clobber). Uses `std::fs::rename`
/// (an atomic same-filesystem rename that PRESERVES the inode + metadata), falling back to a
/// copy-then-remove only across filesystems. Guards against accidental clobber: an existing
/// destination is a hard error unless `overwrite:true`.
struct FileMove {
    root: PathBuf,
}
impl FileMove {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}
impl Tool for FileMove {
    fn name(&self) -> &str {
        "file_move"
    }
    fn description(&self) -> &str {
        "Rename or move a file or directory (from → to) in a single call. Use this instead of \
         shelling out to mv / move / Rename-Item. An existing destination is a hard error unless \
         `overwrite` is true (so you never clobber a file by accident). Set `create_dirs` true to \
         create missing parent directories of the destination. Preserves file metadata (it is an \
         OS-level rename on the same drive). Relative paths resolve under the working directory; an \
         absolute path or a leading `../` may move ANYWHERE on disk."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "from": {"type": "string", "description": "existing source path (file or directory)"},
                "to": {"type": "string", "description": "destination path"},
                "overwrite": {"type": "boolean", "description": "replace the destination if it already exists (default false)"},
                "create_dirs": {"type": "boolean", "description": "create missing parent directories of the destination (default false)"}
            },
            "required": ["from", "to"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, args: &Value) -> WorkspaceEffect {
        move_effect(&self.root, args)
    }
    /// `from`, not `to`: the preimage worth capturing is the source, since a move REMOVES it and
    /// that is the part a rewind has to put back. (When both sides live in one repo — the common
    /// case — either would find the same work tree anyway.)
    fn workspace_target(&self, args: &Value) -> Option<std::path::PathBuf> {
        write_target_dir(&self.root, args.get("from").and_then(|v| v.as_str()))
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let from = str_arg(args, "from")?;
        let to = str_arg(args, "to")?;
        let overwrite = args
            .get("overwrite")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let create_dirs = args
            .get("create_dirs")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        // Source MUST exist (must_exist=true canonicalizes the whole path → clear error if missing).
        let src =
            confine(&self.root, from, true).with_context(|| format!("source {from} not found"))?;
        // Destination need only have an existing parent (must_exist=false), unless create_dirs asks
        // us to make it. Resolve the parent ourselves so we can create it BEFORE confine() tries to
        // canonicalize it (confine requires the parent to already exist).
        let raw_to = Path::new(to);
        let joined_to = if raw_to.is_absolute() {
            raw_to.to_path_buf()
        } else {
            self.root.join(raw_to)
        };
        if create_dirs {
            if let Some(parent) = joined_to.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("creating parent directories of {to}"))?;
            }
        }
        let dst = confine(&self.root, to, false)
            .with_context(|| format!("resolving destination {to} (its parent directory must exist — pass create_dirs:true to create it)"))?;

        // Moving a path onto itself is a no-op (don't churn the FS or arm the verify gate).
        if src == dst {
            return Ok(format!(
                "{NOOP_WRITE_PREFIX}: {from} and {to} are the same path"
            ));
        }
        // CASE-ONLY (or otherwise same-inode) rename on a case-insensitive FS: `foo.txt` → `Foo.txt`.
        // `src` is canonicalized to the on-disk casing while `dst` keeps the requested casing, so
        // `src == dst` above is FALSE — yet `dst.exists()` is TRUE because both names hit the same
        // inode. The old code then DELETED that one inode and renamed a now-missing source into the
        // void: permanent data loss. Detect it by canonicalizing `dst`; if it resolves to `src`, this
        // is the same file and a direct rename just changes its recorded casing — never delete.
        let dst_is_src = dst.canonicalize().map(|c| c == src).unwrap_or(false);
        if dst_is_src {
            move_path(&src, &dst).with_context(|| format!("renaming {from} → {to}"))?;
            let kind = if dst.is_dir() { "directory" } else { "file" };
            return Ok(format!("moved {kind} {from} → {to}"));
        }
        if dst.exists() {
            if !overwrite {
                bail!("destination {to} already exists; pass overwrite:true to replace it");
            }
            // Overwrite WITHOUT a pre-delete window: if we deleted `dst` first and the subsequent
            // rename then failed (permissions, a race, source vanished), `dst` would be gone with
            // nothing put in its place — an irrecoverable clobber. Instead STASH the existing dst to a
            // sibling temp, attempt the move, and only delete the stash once the move succeeds; on any
            // failure, rename the stash back so `dst` is exactly as it was.
            let stash = stash_path(&dst);
            std::fs::rename(&dst, &stash)
                .with_context(|| format!("staging existing {to} aside before overwrite"))?;
            match move_path(&src, &dst) {
                Ok(()) => {
                    // Move landed — the stashed old destination is now safe to drop.
                    if stash.is_dir() {
                        let _ = std::fs::remove_dir_all(&stash);
                    } else {
                        let _ = std::fs::remove_file(&stash);
                    }
                }
                Err(move_err) => {
                    // Roll back: put the original destination back exactly where it was, then report
                    // the move failure (not the rollback) as the actionable error.
                    let _ = std::fs::rename(&stash, &dst);
                    return Err(anyhow::Error::new(move_err)).with_context(|| {
                        format!("moving {from} → {to} (destination left unchanged)")
                    });
                }
            }
            let kind = if dst.is_dir() { "directory" } else { "file" };
            return Ok(format!("moved {kind} {from} → {to}"));
        }
        move_path(&src, &dst).with_context(|| format!("moving {from} → {to}"))?;
        let kind = if dst.is_dir() { "directory" } else { "file" };
        Ok(format!("moved {kind} {from} → {to}"))
    }
}

/// Write `content` to `target` ATOMICALLY: stream into a sibling temp file, fsync it, then
/// `fs::rename` it over the target. A same-directory rename is atomic on every OS we support (POSIX
/// `rename(2)`; Windows `MoveFileExW` with REPLACE_EXISTING), so a concurrent reader — and, more
/// importantly, the on-disk state after a crash/kill/disk-full mid-write — is either the intact old
/// file or the fully-written new one, never a truncated/partial file. Plain `fs::write` truncates
/// the target in place before streaming, so an interrupted write destroys the original. Preserves
/// the target's existing permission bits (temp files are created with default perms, so an in-place
/// rewrite would otherwise silently reset mode). The temp file is cleaned up on any failure before
/// the rename lands. The temp lives in the target's own directory, so the rename never crosses a
/// filesystem boundary (which would make it non-atomic).
pub(crate) fn atomic_write(target: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    static TMP_COUNTER: AtomicUsize = AtomicUsize::new(0);
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let fname = target.file_name().and_then(|n| n.to_str()).unwrap_or("out");
    let n = TMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{fname}.aizen-tmp.{}.{n}", std::process::id()));

    // Write + fsync, then CLOSE the temp handle before renaming (Windows won't replace the
    // destination while a handle to the source is open). Any error aborts before the rename, so the
    // original target is untouched; clean up the temp.
    let write_res = (|| -> std::io::Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(content)?;
        f.sync_all()?;
        Ok(())
    })();
    if let Err(e) = write_res {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    // Carry over the target's mode so an atomic rewrite preserves permissions. Best-effort: a
    // metadata/permission hiccup must never turn into data loss.
    if let Ok(meta) = std::fs::metadata(target) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }

    if let Err(e) = std::fs::rename(&tmp, target) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// A sibling temp path for staging an existing file/dir out of the way during an overwrite. Lives in
/// the same directory as `p`, so the stash rename stays on one filesystem (and is thus atomic).
fn stash_path(p: &Path) -> PathBuf {
    static STASH_COUNTER: AtomicUsize = AtomicUsize::new(0);
    let parent = p
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let fname = p.file_name().and_then(|n| n.to_str()).unwrap_or("dst");
    let n = STASH_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".{fname}.aizen-stash.{}.{n}", std::process::id()))
}

/// Rename `src` → `dst`, falling back to copy-then-delete when they live on different filesystems
/// (`fs::rename` returns `ErrorKind::CrossesDevices`, or a raw OS error on older toolchains). The
/// happy path is a single atomic `rename` that preserves the inode + metadata.
fn move_path(src: &Path, dst: &Path) -> std::io::Result<()> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(e) if is_cross_device(&e) => {
            // Cross-device: recursively copy then remove the source. Directories are walked; files
            // are a straight copy (which carries permissions via std on both Windows and Unix).
            copy_recursive(src, dst)?;
            if src.is_dir() {
                std::fs::remove_dir_all(src)
            } else {
                std::fs::remove_file(src)
            }
        }
        Err(e) => Err(e),
    }
}

/// Whether a rename error is "these paths are on different filesystems" (the only case worth a
/// copy-then-delete fallback). `CrossesDevices` is unstable-named but stable-valued; also match the
/// raw OS codes (Windows ERROR_NOT_SAME_DEVICE=17, Unix EXDEV=18) so we don't depend on the name.
fn is_cross_device(e: &std::io::Error) -> bool {
    matches!(e.raw_os_error(), Some(17) | Some(18))
}

/// Recursively copy `src` → `dst` (file or directory). Used only on the cross-filesystem move path.
fn copy_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    if src.is_dir() {
        std::fs::create_dir_all(dst)?;
        for entry in std::fs::read_dir(src)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &dst.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        if let Some(parent) = dst.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::copy(src, dst).map(|_| ())
    }
}

/// The outcome of ONE replacement (pure; computed in memory). `before`/`after` feed the diff
/// preview; `count`/`rung` feed the human summary (which rung matched is surfaced so a
/// looser-than-exact match is always visible in the result).
#[derive(Debug)]
struct EditApplied {
    content: String,
    before: String,
    after: String,
    count: usize,
    /// Which ladder rung applied: "exact" | "indent" | "ws-norm" | "anchor-trim" | "unescape" |
    /// "blank-norm".
    rung: &'static str,
}
impl EditApplied {
    /// Human summary. The "exact" and "indent" wordings are byte-identical to the original
    /// `file_edit` strings (regression-gated by the existing tests).
    fn summary(&self) -> String {
        match self.rung {
            "indent" => "1 replacement, indentation-tolerant match".to_string(),
            "ws-norm" => "1 replacement, whitespace-normalized match".to_string(),
            "anchor-trim" => "1 replacement, shared-context-trimmed match".to_string(),
            "unescape" => "1 replacement, escape-normalized match".to_string(),
            "blank-norm" => "1 replacement, blank-line-insensitive match".to_string(),
            _ => format!("{} replacement(s)", self.count),
        }
    }
}

/// Rungs R3–R5 skip oversized hunks (quadratic-ish scans buy nothing on giant blocks).
const LADDER_MAX_LINES: usize = 200;
const LADDER_MAX_BYTES: usize = 16 * 1024;
/// The nearest-miss report is skipped on very large files (the scan is O(lines × hunk-lines)).
const NEAREST_MISS_MAX_FILE_LINES: usize = 20_000;

/// Apply ONE replacement to `content` — the shared matcher behind both `file_edit` and
/// `multi_edit` (pure, no IO). A LADDER of progressively looser strategies (the Aider lesson:
/// every rescued edit is a saved round-trip — a ~9× edit-error reduction was measured for this
/// class of matcher), with one invariant at every rung: EXACTLY one match applies; more than one
/// is a hard "ambiguous" error that never falls through to a looser rung (a looser rung must not
/// resolve an ambiguity a stricter one detected); zero falls through.
///
///   R1 exact           — `content.matches(old)` (`replace_all` lives here ONLY)
///   R2 indent-tolerant — per-line trim (the #1 real failure: leading indentation)
///   R3 ws-normalized   — per-line interior whitespace collapsed (`foo( a,  b )` ≡ `foo(a, b)`)
///   R4 anchor-trim     — a first/last line byte-identical in old AND new (pure context) is
///                        dropped from BOTH and R1–R3 retried: semantics-preserving by construction
///   R5 escape-norm     — JSON-unescape old AND new (the double-escaped-arguments failure mode)
///   R5.5 blank-norm    — match ignoring blank lines (the model added/dropped an empty line inside
///                        the block, so its fixed line count no longer aligns with the file)
///   R6 nearest-miss    — terminal failure with the best-scoring region quoted (line numbers +
///                        "copy EXACTLY from this"), so the retry lands in ONE turn
///
/// `old` MUST be non-empty (create-new is the caller's concern). `label` names the target in
/// errors (a path for file_edit; "edit #N (path)" for multi_edit).
fn apply_one_edit(
    content: &str,
    old: &str,
    new: &str,
    replace_all: bool,
    label: &str,
) -> Result<EditApplied> {
    if old.is_empty() {
        bail!("empty old_string is only valid for creating a new file (file_edit), not mid-edit");
    }
    // R1 exact.
    let count = content.matches(old).count();
    if count > 0 {
        if count > 1 && !replace_all {
            bail!("old_string is not unique in {label} ({count} matches); add context or set replace_all");
        }
        let updated = if replace_all {
            content.replace(old, new)
        } else {
            content.replacen(old, new, 1)
        };
        return Ok(EditApplied {
            content: updated,
            before: old.to_string(),
            after: new.to_string(),
            count,
            rung: "exact",
        });
    }
    // R2 indent-tolerant (kept uncapped + byte-stable messages — the original fallback).
    if let Some(a) = block_rung(content, old, new, trim_norm, "indent", label)? {
        return Ok(a);
    }
    let within_caps = old.lines().count() <= LADDER_MAX_LINES && old.len() <= LADDER_MAX_BYTES;
    if within_caps {
        // R3 whitespace-run-normalized.
        if let Some(a) = block_rung(content, old, new, ws_collapse, "ws-norm", label)? {
            return Ok(a);
        }
        // R4 anchor-trim: drop a SHARED (old==new) first/last context line from both sides.
        for (o2, n2) in anchor_trim_variants(old, new) {
            if let Some(a) = try_pair(content, &o2, &n2, "anchor-trim", label)? {
                return Ok(a);
            }
        }
        // R5 escape-normalized (only when unescaping actually changes old).
        if let Some(o2) = json_unescape(old) {
            let n2 = json_unescape(new).unwrap_or_else(|| new.to_string());
            if let Some(a) = try_pair(content, &o2, &n2, "unescape", label)? {
                return Ok(a);
            }
        }
        // R5.5 blank-line-insensitive: the model added or dropped a blank line INSIDE the block, so
        // its line count no longer lines up with the file and the fixed-`k` rungs above all miss.
        // Match on the non-blank trimmed lines only; splice the whole spanned region (interior blanks
        // included) so `new` fully redefines it. Same 1-match invariant as every other rung.
        {
            let ranges = blank_insensitive_blocks(content, old);
            match ranges.len() {
                0 => {}
                1 => {
                    let (bs, be) = ranges[0];
                    let before = content[bs..be].to_string();
                    let spliced = preserve_eol(new, &before, &content[be..]);
                    let updated = format!("{}{}{}", &content[..bs], spliced, &content[be..]);
                    return Ok(EditApplied {
                        content: updated,
                        before,
                        after: new.to_string(),
                        count: 1,
                        rung: "blank-norm",
                    });
                }
                n => bail!(
                    "old_string (ignoring blank lines) matches {n} blocks in {label}; add more surrounding context to disambiguate"
                ),
            }
        }
    }
    // R6 terminal: nearest-miss report so the model's retry can copy the real bytes.
    if content.lines().count() <= NEAREST_MISS_MAX_FILE_LINES {
        if let Some(hint) = nearest_miss(content, old) {
            bail!(
                "old_string not found in {label} (tried exact, indentation-tolerant, \
                 whitespace-normalized, anchor-trimmed, escape-normalized, and \
                 blank-line-insensitive matching).\n{hint}"
            );
        }
    }
    bail!("old_string not found in {label} (even ignoring indentation) — re-read the file; it may have changed")
}

/// One full sub-ladder (R1 exact → R2 trim → R3 ws-collapse) over a DERIVED (old,new) pair —
/// the driver for the R4/R5 variants. Same invariant: 1 ⇒ apply as `rung`, >1 ⇒ hard error,
/// 0 ⇒ `None` (fall through).
fn try_pair(
    content: &str,
    old: &str,
    new: &str,
    rung: &'static str,
    label: &str,
) -> Result<Option<EditApplied>> {
    if old.trim().is_empty() {
        return Ok(None); // a variant that trimmed away all signal proves nothing
    }
    let count = content.matches(old).count();
    if count == 1 {
        let updated = content.replacen(old, new, 1);
        return Ok(Some(EditApplied {
            content: updated,
            before: old.to_string(),
            after: new.to_string(),
            count: 1,
            rung,
        }));
    }
    if count > 1 {
        bail!("old_string ({rung} form) matches {count} places in {label}; add more surrounding context to disambiguate");
    }
    if let Some(a) = block_rung(content, old, new, trim_norm, rung, label)? {
        return Ok(Some(a));
    }
    block_rung(content, old, new, ws_collapse, rung, label)
}

/// One block-matching rung over normalized lines: exactly 1 block ⇒ apply, >1 ⇒ hard ambiguous
/// error, 0 ⇒ `None` (fall through). The "indent" wording stays byte-identical to the original.
fn block_rung(
    content: &str,
    old: &str,
    new: &str,
    norm: fn(&str) -> String,
    rung: &'static str,
    label: &str,
) -> Result<Option<EditApplied>> {
    let ranges = normalized_blocks(content, old, norm);
    match ranges.len() {
        0 => Ok(None),
        1 => {
            let (bs, be) = ranges[0];
            let before = content[bs..be].to_string();
            let spliced = preserve_eol(new, &before, &content[be..]);
            let updated = format!("{}{}{}", &content[..bs], spliced, &content[be..]);
            Ok(Some(EditApplied {
                content: updated,
                before,
                after: new.to_string(),
                count: 1,
                rung,
            }))
        }
        n => {
            if rung == "indent" {
                bail!("old_string (ignoring indentation) matches {n} blocks in {label}; add more surrounding context to disambiguate");
            }
            bail!("old_string ({rung} form) matches {n} blocks in {label}; add more surrounding context to disambiguate");
        }
    }
}

/// Preserve the file's line endings across a tolerant-rung splice. The matched span (`before`)
/// includes the trailing `\r` of a CRLF line while the `\n` itself sits just past it — `after_be`
/// (the tail from `be`) starts with that `\n`. A model-supplied `new` is typically bare-LF, so a
/// naive splice drops the boundary `\r` and leaves a lone-LF line among CRLF neighbours (Windows
/// mixed-ending corruption). When the replaced region was CRLF, convert `new`'s internal breaks to
/// CRLF and restore the boundary `\r`. LF files are returned untouched (byte-identical to before).
fn preserve_eol(new: &str, before: &str, after_be: &str) -> String {
    let crlf = before.contains("\r\n") || before.ends_with('\r');
    if !crlf {
        return new.to_string();
    }
    let mut out = if new.contains('\n') && !new.contains("\r\n") {
        new.replace('\n', "\r\n")
    } else {
        new.to_string()
    };
    if after_be.starts_with('\n') && !out.ends_with('\r') {
        out.push('\r');
    }
    out
}

/// Per-line trim — the R2 normalization (leading indentation + trailing whitespace + CRLF).
fn trim_norm(l: &str) -> String {
    l.trim().to_string()
}

/// Per-line whitespace-INSENSITIVE comparison — the R3 normalization (`foo( a,  b )` ≡
/// `foo(a, b)`). All whitespace is dropped for matching; the theoretical conflation (`let x` vs
/// `letx`) is guarded by the exactly-1-match invariant, and the rung name in the result summary
/// makes any loose match visible.
fn ws_collapse(l: &str) -> String {
    l.split_whitespace().collect::<Vec<_>>().concat()
}

/// R4 variants: when the first and/or last line of `old` is BYTE-IDENTICAL in `new` (pure
/// unchanged context), dropping it from BOTH sides cannot change the result — but can rescue a
/// match when that context line was mis-copied elsewhere. Order: drop-first, drop-last, drop-both.
fn anchor_trim_variants(old: &str, new: &str) -> Vec<(String, String)> {
    let ol: Vec<&str> = old.lines().collect();
    let nl: Vec<&str> = new.lines().collect();
    let mut out = Vec::new();
    if ol.len() < 2 || nl.is_empty() {
        return out;
    }
    let first_shared = ol.first() == nl.first();
    let last_shared = ol.last() == nl.last();
    if first_shared {
        out.push((ol[1..].join("\n"), nl[1..].join("\n")));
    }
    if last_shared {
        out.push((ol[..ol.len() - 1].join("\n"), nl[..nl.len() - 1].join("\n")));
    }
    if first_shared && last_shared && ol.len() >= 3 && nl.len() >= 2 {
        out.push((
            ol[1..ol.len() - 1].join("\n"),
            nl[1..nl.len() - 1].join("\n"),
        ));
    }
    out
}

/// Undo one level of JSON string escaping (`\n` `\t` `\r` `\"` `\\`) — the double-escaped-arguments
/// failure mode where the model ships literal `\n` sequences instead of newlines. `None` when
/// nothing changed (the rung is then skipped). Unknown escapes pass through untouched.
fn json_unescape(s: &str) -> Option<String> {
    if !s.contains('\\') {
        return None;
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    let mut changed = false;
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => {
                    out.push('\n');
                    changed = true;
                }
                Some('t') => {
                    out.push('\t');
                    changed = true;
                }
                Some('r') => {
                    out.push('\r');
                    changed = true;
                }
                Some('"') => {
                    out.push('"');
                    changed = true;
                }
                Some('\\') => {
                    out.push('\\');
                    changed = true;
                }
                Some(other) => {
                    out.push('\\');
                    out.push(other);
                }
                None => out.push('\\'),
            }
        } else {
            out.push(c);
        }
    }
    changed.then_some(out)
}

/// R6: the best-scoring window of the file vs `old` (mean per-line Jaro-Winkler on trimmed
/// lines), quoted with line numbers so the model's retry copies REAL bytes. `None` below the 0.55
/// similarity floor ("no similar region" is more honest than a random quote).
fn nearest_miss(content: &str, old: &str) -> Option<String> {
    let lines: Vec<&str> = content.lines().collect();
    let ol: Vec<&str> = old.lines().collect();
    let k = ol.len().clamp(1, LADDER_MAX_LINES);
    if lines.is_empty() || lines.len() < k {
        return None;
    }
    let mut best_i = 0usize;
    let mut best_score = 0.0f64;
    for i in 0..=(lines.len() - k) {
        let score: f64 = (0..k)
            .map(|j| strsim::jaro_winkler(lines[i + j].trim(), ol.get(j).map_or("", |l| l.trim())))
            .sum::<f64>()
            / k as f64;
        if score > best_score {
            best_score = score;
            best_i = i;
        }
    }
    if best_score < 0.55 {
        return None;
    }
    let ctx_start = best_i.saturating_sub(3);
    let ctx_end = (best_i + k + 3).min(lines.len());
    let mut excerpt = String::new();
    for (n, line) in lines[ctx_start..ctx_end].iter().enumerate() {
        excerpt.push_str(&format!("{:>5}| {line}\n", ctx_start + n + 1));
        if excerpt.len() > 2_000 || n >= 40 {
            excerpt.push_str("    …\n");
            break;
        }
    }
    Some(format!(
        "Nearest match (similarity {best_score:.2}) at lines {}-{}:\n{excerpt}Copy old_string EXACTLY from the excerpt above (including whitespace) and retry.",
        best_i + 1,
        best_i + k
    ))
}

/// Find full-line blocks in `content` that match `old` under a per-line normalization. Returns the
/// byte range [start, end) of each matching block (line-aligned: from the first matched line's
/// start through the last matched line's end, excluding the trailing newline).
fn normalized_blocks(content: &str, old: &str, norm: fn(&str) -> String) -> Vec<(usize, usize)> {
    let old_norm: Vec<String> = old.lines().map(norm).collect();
    let k = old_norm.len();
    if k == 0 {
        return Vec::new();
    }
    // Byte spans of every line in content (line content excludes the '\n').
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in content.char_indices() {
        if ch == '\n' {
            spans.push((start, idx));
            start = idx + 1;
        }
    }
    spans.push((start, content.len()));

    let cnorm: Vec<String> = spans.iter().map(|&(a, b)| norm(&content[a..b])).collect();
    let mut out = Vec::new();
    if cnorm.len() < k {
        return out;
    }
    for i in 0..=(cnorm.len() - k) {
        if (0..k).all(|j| cnorm[i + j] == old_norm[j]) {
            out.push((spans[i].0, spans[i + k - 1].1));
        }
    }
    out
}

/// R5.5: find blocks matching `old` when blank lines are IGNORED on both sides — the model added or
/// dropped an empty line inside the block, so its fixed line count no longer aligns and every rung
/// above (which needs exactly `k` consecutive lines) misses. The comparison is over the trimmed,
/// NON-blank lines only; the returned byte range still covers the WHOLE spanned region in the file
/// (interior blank lines included), so splicing `new` in redefines it wholesale. Returns each
/// matching block's `[start, end)` (line-aligned, excluding the trailing newline). A block whose
/// non-blank content is empty proves nothing → no ranges (the caller then falls through to R6).
fn blank_insensitive_blocks(content: &str, old: &str) -> Vec<(usize, usize)> {
    // The signature we match: `old`'s non-blank lines, trimmed.
    let want: Vec<String> = old
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    let k = want.len();
    if k == 0 {
        return Vec::new();
    }
    // Byte spans of every line in content (line content excludes the '\n').
    let mut spans: Vec<(usize, usize)> = Vec::new();
    let mut start = 0usize;
    for (idx, ch) in content.char_indices() {
        if ch == '\n' {
            spans.push((start, idx));
            start = idx + 1;
        }
    }
    spans.push((start, content.len()));

    // Index the NON-blank lines (trimmed text + which content-line they came from), so a matched run
    // of `k` non-blank lines can be mapped back to the first/last physical line for the byte span.
    let nonblank: Vec<(usize, String)> = spans
        .iter()
        .enumerate()
        .map(|(li, &(a, b))| (li, content[a..b].trim().to_string()))
        .filter(|(_, t)| !t.is_empty())
        .collect();
    if nonblank.len() < k {
        return Vec::new();
    }
    let mut out = Vec::new();
    for i in 0..=(nonblank.len() - k) {
        if (0..k).all(|j| nonblank[i + j].1 == want[j]) {
            let first_line = nonblank[i].0;
            let last_line = nonblank[i + k - 1].0;
            out.push((spans[first_line].0, spans[last_line].1));
        }
    }
    out
}

/// A compact **unified diff** of an edit: common leading/trailing lines are trimmed away (so a big
/// block collapses to just its changed window), the removed lines are prefixed `-`, the added lines
/// `+`, and a couple of context lines (prefixed with a space) bracket the change. Gives the model
/// cheap verifiability AND is what the TUI colourises (removed = salmon, added = green) — the lines
/// are `^[-+]`-prefixed at column 0 so the display can pick them out unambiguously. Both sides are
/// capped so a giant replacement can't flood the result.
///
/// Callers pass the SMALL changed region (file_edit: `applied.before`/`after`; multi_edit: each
/// per-edit before/after), so the prefix/suffix trim is enough — no full LCS needed.
fn diff_preview(before: &str, after: &str) -> String {
    const CTX: usize = 2; // context lines kept on each side of the change
    const MAX_SIDE: usize = 40; // cap removed / added lines shown per side

    let b: Vec<&str> = before.lines().collect();
    let a: Vec<&str> = after.lines().collect();

    // Longest common prefix, then longest common suffix that doesn't overlap the prefix.
    let mut p = 0;
    while p < b.len() && p < a.len() && b[p] == a[p] {
        p += 1;
    }
    let mut s = 0;
    while s < b.len().saturating_sub(p)
        && s < a.len().saturating_sub(p)
        && b[b.len() - 1 - s] == a[a.len() - 1 - s]
    {
        s += 1;
    }
    let removed = &b[p..b.len() - s];
    let added = &a[p..a.len() - s];

    let mut out = String::new();
    // leading context (from the shared prefix)
    for line in &b[p.saturating_sub(CTX)..p] {
        out.push_str(&format!(" {line}\n"));
    }
    for line in removed.iter().take(MAX_SIDE) {
        out.push_str(&format!("-{line}\n"));
    }
    if removed.len() > MAX_SIDE {
        out.push_str(&format!(
            "…({} more lines removed)\n",
            removed.len() - MAX_SIDE
        ));
    }
    for line in added.iter().take(MAX_SIDE) {
        out.push_str(&format!("+{line}\n"));
    }
    if added.len() > MAX_SIDE {
        out.push_str(&format!("…({} more lines added)\n", added.len() - MAX_SIDE));
    }
    // trailing context (from the shared suffix)
    let suf_start = b.len() - s;
    for line in &b[suf_start..(suf_start + CTX).min(b.len())] {
        out.push_str(&format!(" {line}\n"));
    }
    out.trim_end_matches('\n').to_string()
}

// ── multi_edit ───────────────────────────────────────────────────────────────

/// Apply an ORDERED list of edits to ONE file in a single atomic write. Collapses what would be N
/// `file_edit` round-trips (each invalidating the model's byte offsets) into one call / one turn.
struct MultiEdit {
    root: PathBuf,
}
impl MultiEdit {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}
impl Tool for MultiEdit {
    fn name(&self) -> &str {
        "multi_edit"
    }
    fn description(&self) -> &str {
        "Apply an ORDERED list of exact-string edits to ONE file in a single atomic write — all \
         succeed or the file is left untouched. Each edit is {old_string, new_string, replace_all?} \
         and applies to the result of the previous one (same matching as file_edit, incl. the \
         indentation-tolerant retry). For a SINGLE edit use file_edit. Read the file first. A relative \
         path resolves under the working directory; an absolute path or a leading `../` may write \
         elsewhere on disk."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "ordered; each applies to the result of the previous edit",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": {"type": "string", "description": "exact text to replace (non-empty)"},
                            "new_string": {"type": "string"},
                            "replace_all": {"type": "boolean", "description": "replace every occurrence in the current buffer (default false)"}
                        },
                        "required": ["old_string", "new_string"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["path", "edits"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, args: &Value) -> WorkspaceEffect {
        write_effect(&self.root, args.get("path").and_then(|v| v.as_str()))
    }
    fn workspace_target(&self, args: &Value) -> Option<std::path::PathBuf> {
        write_target_dir(&self.root, args.get("path").and_then(|v| v.as_str()))
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let path = str_arg(args, "path")?;
        let edits = args
            .get("edits")
            .and_then(|v| v.as_array())
            .filter(|a| !a.is_empty())
            .context("multi_edit requires a non-empty 'edits' array")?;
        let target = confine(&self.root, path, true)?;
        let (original_bytes, expected) = crate::core::persist::read_with_fingerprint(&target)?;
        let original = String::from_utf8(original_bytes.context("file disappeared while reading")?)
            .with_context(|| format!("{} is not valid UTF-8", target.display()))?;

        // Compute the whole result in memory; write ONCE at the end. Any edit error returns before
        // the write is reached → atomic ("nothing written"), no temp file / rollback needed. Each
        // edit re-searches the EVOLVING buffer, so offsets can never go stale (string search, not
        // byte offsets) and a later edit may target text an earlier one produced.
        let mut buf = original.clone();
        let mut summaries: Vec<String> = Vec::with_capacity(edits.len());
        // Diff each edit's OWN before/after (the small changed region) rather than the whole file:
        // precise per-hunk output, and it keeps `diff_preview`'s prefix/suffix trim on small inputs
        // (a whole-file trim would merge far-apart edits into one giant spurious hunk).
        let mut diffs: Vec<String> = Vec::with_capacity(edits.len());
        for (i, e) in edits.iter().enumerate() {
            let n = i + 1;
            let old = e
                .get("old_string")
                .and_then(|v| v.as_str())
                .with_context(|| format!("edit #{n}: missing old_string"))?;
            let new = e
                .get("new_string")
                .and_then(|v| v.as_str())
                .with_context(|| format!("edit #{n}: missing new_string"))?;
            let replace_all = e
                .get("replace_all")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let applied =
                apply_one_edit(&buf, old, new, replace_all, &format!("edit #{n} ({path})"))?;
            let detail = match applied.rung {
                "indent" => "1 replacement, indentation-tolerant".to_string(),
                "exact" if replace_all => format!("{} replacement(s), replace_all", applied.count),
                "exact" => format!("{} replacement(s)", applied.count),
                other => format!("1 replacement, {other} match"),
            };
            summaries.push(format!("  #{n}: {detail}"));
            diffs.push(diff_preview(&applied.before, &applied.after));
            buf = applied.content;
        }

        // No-op guard: every edit is individually valid (else apply_one_edit already bailed above),
        // but a sequence that nets out to the original content (e.g. a change immediately undone by
        // a later edit) must not touch disk or arm the verify gate (W16).
        if buf == original {
            return Ok(format!(
                "{NOOP_WRITE_PREFIX}: {path} unchanged after {} edit(s) net to nothing",
                edits.len()
            ));
        }
        crate::core::persist::compare_and_atomic_write(&target, &expected, buf.as_bytes())
            .with_context(|| format!("writing {}", target.display()))?;
        let mut out = format!(
            "edited {path} ({} edits applied)\n{}\n{}",
            edits.len(),
            summaries.join("\n"),
            diffs.join("\n")
        );
        // One fold after the single atomic write (same as file_edit).
        if let Some(fb) = crate::agent::lsp::LSP.edit_feedback(&target) {
            out.push('\n');
            out.push_str(&fb);
        }
        Ok(out)
    }
}

// ── shell_run ────────────────────────────────────────────────────────────────

struct ShellRun {
    root: PathBuf,
}
impl ShellRun {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }
}
impl Tool for ShellRun {
    fn name(&self) -> &str {
        "shell_run"
    }
    fn description(&self) -> &str {
        "Run a shell command in the working directory and return its stdout/stderr + exit code. \
         Use to build, test, run tools, or manage files. For content search use search_files (not \
         grep here). Wall-clock cap: 120s by default (AIZEN_SHELL_TIMEOUT_SECS overrides it, \
         10..3600) — on timeout the whole process tree is killed. For anything that should keep \
         running (dev servers, watchers, very long builds) use the process tool instead, which has \
         no cap. Destructive — the user is asked to confirm."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "cwd": {"type": "string", "description": "optional working dir for the command (a subdir, or a ../ or absolute path elsewhere)"}
            },
            "required": ["command"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, _args: &Value) -> WorkspaceEffect {
        WorkspaceEffect::OpaqueWorkspace
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let command = str_arg(args, "command")?;
        let dir = match args.get("cwd").and_then(|v| v.as_str()) {
            Some(c) => confine(&self.root, c, true)?,
            None => self.root.clone(),
        };
        let mut cmd = if cfg!(windows) {
            // Switch the cmd instance to the UTF-8 codepage first so `dir` and other legacy
            // builtins emit UTF-8 (real accented filenames) instead of the OEM codepage that
            // `drain` can only decode lossily. `>nul` hides chcp's own "Active code page" banner;
            // `&` chains it before the real command. The whole thing is one `/C` argument.
            let mut c = Command::new("cmd");
            c.arg("/C").arg(format!("chcp 65001>nul & {command}"));
            c
        } else {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            c
        };
        // stdin = null (not inherited): on Windows a shared console-input handle lets `cmd.exe`
        // (and the CRT) call SetConsoleMode on OUR input buffer and reset it on exit — clearing
        // ENABLE_MOUSE_INPUT and re-enabling QuickEdit. That silently kills the retained TUI's mouse
        // capture, so after a shell command the wheel leaks back through as ↑/↓ keys and the
        // transcript can no longer be scrolled (the "can't scroll during/after a task" bug). A null
        // stdin means the child never touches the console input handle, so our mode survives. The
        // command already runs non-interactively (drained pipes, wall-clock timeout), so it has no
        // legitimate use for the terminal's stdin anyway.
        cmd.current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Contain the tree BEFORE anything can go wrong with it: on Windows we spawn a `cmd.exe`
        // wrapper, so killing our direct child leaves the real work (cargo, node, a dev server)
        // orphaned — still running, and still holding the write end of the pipes below.
        crate::core::proctree::prepare(&mut cmd);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning shell for `{command}`"))?;
        let containment = crate::core::proctree::contain(&child);

        // Drain the pipes on threads so a chatty command can't deadlock on a full buffer,
        // while we poll with a wall-clock timeout (kill the tree if it overruns).
        let out_pipe = child.stdout.take();
        let err_pipe = child.stderr.take();
        let oh = std::thread::spawn(move || drain(out_pipe));
        let eh = std::thread::spawn(move || drain(err_pipe));

        let timeout = Duration::from_secs(shell_timeout_secs());
        let start = Instant::now();
        let mut next_note = SLOW_NOTE_EVERY_SECS;
        let mut cancelled = false;
        let status = loop {
            match child.try_wait()? {
                Some(st) => break Some(st),
                None => {
                    // User pressed Esc (cooperative cancel) — kill the tree now instead of blocking
                    // the whole turn up to the timeout. This is what makes Esc responsive during a
                    // long command (the confirmed "can't cancel while a tool runs" bug).
                    let cancel_requested = crate::core::cancel::current()
                        .or_else(crate::ui::tui::active_cancel_token)
                        .is_some_and(|token| token.is_cancelled());
                    if cancel_requested {
                        crate::core::proctree::kill_tree(&mut child, &containment);
                        cancelled = true;
                        break None;
                    }
                    if start.elapsed() >= timeout {
                        crate::core::proctree::kill_tree(&mut child, &containment);
                        break None;
                    }
                    // Tell the user what is taking so long, once every 30s. The working pill already
                    // animates and counts elapsed seconds, so liveness is covered — what it cannot
                    // say is WHICH command is still out, or that a ceiling is approaching. Without
                    // that, a 2-minute build and a wedged process look identical from the outside.
                    let waited = start.elapsed().as_secs();
                    if waited >= next_note {
                        note_slow_command(command, waited, timeout.as_secs());
                        next_note = waited + SLOW_NOTE_EVERY_SECS;
                    }
                    std::thread::sleep(Duration::from_millis(40));
                }
            }
        };
        // Never join a pipe reader unbounded. If containment failed (or a descendant escaped the job
        // via its own detached wrapper) an orphan can still hold the write end, and `read_to_end`
        // only returns on EOF = last writer closed — a wait that a wedged process never ends. A
        // reproduction of exactly this blocked >12s on a 45s sleeper. Take whatever the readers have
        // finished within the grace window; a lost tail beats a frozen agent.
        let (mut stdout, out_cut) = crate::core::proctree::join_drain(oh, DRAIN_GRACE);
        let (stderr, err_cut) = crate::core::proctree::join_drain(eh, DRAIN_GRACE);
        if out_cut || err_cut {
            // Say so rather than presenting a truncated log as complete — the model would otherwise
            // reason from output it cannot tell is partial.
            stdout.push_str(
                "\n[output truncated: a surviving child process still held the pipe open; \
                 the command's own result above may be incomplete]",
            );
        }

        match status {
            None if cancelled => Ok("error: command cancelled by the user (Esc)".to_string()),
            None => {
                // Surface stderr too (the success branch already does) so a killed build's diagnostics
                // aren't lost to the model — they're often the most useful part of a timeout.
                let secs = shell_timeout_secs();
                // Report the kill's real reach. Claiming "whole tree" when containment was
                // unavailable would hide a live orphan from the one person who could stop it.
                let scope = if containment.is_contained() {
                    "killed, whole process tree"
                } else {
                    "killed the shell only — this platform refused process containment, so a \
                     descendant may still be running"
                };
                let mut s = format!(
                    "error: command timed out after {secs}s ({scope})\n\
                     If it needs longer, re-run it with the `process` tool (action=start) which has \
                     no wall-clock cap, or raise AIZEN_SHELL_TIMEOUT_SECS.\n{stdout}"
                );
                if !stderr.trim().is_empty() {
                    s.push_str("\n[stderr]\n");
                    s.push_str(&stderr);
                }
                Ok(s.trim_end().to_string())
            }
            Some(st) => {
                let mut s = format!("exit {}\n", st.code().unwrap_or(-1));
                s.push_str(&stdout);
                if !stderr.trim().is_empty() {
                    s.push_str("\n[stderr]\n");
                    s.push_str(&stderr);
                }
                Ok(s.trim_end().to_string())
            }
        }
    }
}

/// Read a child pipe to a string (best-effort; used on a drain thread).
///
/// Reads RAW BYTES then decodes lossily — NOT `read_to_string`, which returns `Err` and leaves
/// the buffer EMPTY on the first invalid-UTF-8 byte. On non-English Windows, `cmd`/PowerShell emit
/// output in the OEM/ANSI codepage (CP437/850/1258, etc.), so `dir` listings and accented
/// filenames are not valid UTF-8 → `read_to_string` would silently drop the ENTIRE output and the
/// agent would see a blank result. Lossy decoding keeps the ASCII structure (paths, sizes, the
/// listing layout) and only the odd accented byte degrades to `�`.
pub(crate) fn drain<R: std::io::Read>(pipe: Option<R>) -> String {
    match pipe {
        Some(mut p) => {
            let mut bytes = Vec::new();
            let _ = p.read_to_end(&mut bytes);
            String::from_utf8_lossy(&bytes).into_owned()
        }
        None => String::new(),
    }
}

// ── skill_load ───────────────────────────────────────────────────────────────

struct SkillLoad;
impl Tool for SkillLoad {
    fn name(&self) -> &str {
        "skill_load"
    }
    fn description(&self) -> &str {
        "Load a saved skill (a reusable step-by-step procedure) by name and return its steps to \
         follow. The available skills are listed in <skills>. Use when the task matches a skill's \
         trigger. Not for recalling facts → use memory_search. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {"name": {"type": "string", "description": "a skill name from <skills>"}},
            "required": ["name"],
            "additionalProperties": false
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let name = str_arg(args, "name")?;
        match crate::skills::load(name) {
            Some(sk) => {
                // Voyager reinforcement: a real load is organic reuse — bump `uses` so a repeatedly
                // useful skill floats to the top of the always-on index and survives its line cap.
                // Best-effort: a bump failure (repo-shipped skill, or I/O) must never break the load.
                let _ = crate::skills::record_use(name);
                Ok(crate::skills::render_loaded(&sk))
            }
            None => {
                let avail: Vec<String> =
                    crate::skills::list().into_iter().map(|s| s.name).collect();
                Ok(format!(
                    "(no skill named '{name}'; available: {})",
                    avail.join(", ")
                ))
            }
        }
    }
}

// ── skill_save ───────────────────────────────────────────────────────────────

struct SkillSave;
impl Tool for SkillSave {
    fn name(&self) -> &str {
        "skill_save"
    }
    fn description(&self) -> &str {
        "Save a reusable procedure as a named skill (so it can be loaded later with skill_load). \
         Use when the user asks to remember HOW to do a recurring task. For facts/preferences \
         (WHAT is true) use memory instead. Writes to ~/.aizen/skills — the user is asked to confirm."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string"},
                "description": {"type": "string", "description": "one-line summary"},
                "when": {"type": "string", "description": "when this skill applies (trigger hint)"},
                "body": {"type": "string", "description": "the steps / procedure (markdown)"},
                "scope": {"type": "string", "enum": ["global", "project"], "description": "global (default) = usable everywhere; project = only this workspace"}
            },
            "required": ["name", "body"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let name = str_arg(args, "name")?;
        let body = str_arg(args, "body")?;
        let description = args
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let when = args.get("when").and_then(|v| v.as_str()).unwrap_or("");
        let project_zone = matches!(
            args.get("scope").and_then(|v| v.as_str()).map(str::trim),
            Some("project")
        );
        let path = crate::skills::save_scoped(name, description, when, body, project_zone)?;
        Ok(format!("saved skill '{name}' → {}", path.display()))
    }
}

// ── skill_refine ─────────────────────────────────────────────────────────────

/// Voyager curriculum: improve an EXISTING skill's steps in place, archiving the prior version so
/// nothing learned is lost and the usage track record carries forward. This is the "skills get
/// better with experience" tool — distinct from `skill_save` (which mints a NEW skill and refuses
/// to touch an existing one).
struct SkillRefine;
impl Tool for SkillRefine {
    fn name(&self) -> &str {
        "skill_refine"
    }
    fn description(&self) -> &str {
        "Improve an EXISTING skill's steps after you found a better way — e.g. a step failed and a \
         corrected sequence worked. Rewrites the skill's body, bumps its version, archives the old \
         copy (nothing is lost), and preserves its usage count. Use this instead of skill_save when \
         the skill already exists. The new steps replace the old ones, so include the whole procedure."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "the existing skill's name (from <skills>)"},
                "body": {"type": "string", "description": "the improved full steps / procedure (replaces the old body)"},
                "description": {"type": "string", "description": "optional new one-line summary (kept if omitted)"},
                "when": {"type": "string", "description": "optional new trigger hint (kept if omitted)"}
            },
            "required": ["name", "body"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let name = str_arg(args, "name")?;
        let body = str_arg(args, "body")?;
        let description = args.get("description").and_then(|v| v.as_str());
        let when = args.get("when").and_then(|v| v.as_str());
        let (version, archived) = crate::skills::refine(name, body, description, when)?;
        Ok(format!(
            "refined skill '{name}' → v{version} (prior version archived at {})",
            archived.display()
        ))
    }
}

// ── persona_create ─────────────────────────────────────────────────────────────

/// Mint (or overwrite) a character persona mid-chat and optionally become it. This is what lets
/// "create a persona named X who is a grumpy detective, and be them" work conversationally — the
/// model fills in name/role/voice/body from the request and calls this tool.
struct PersonaCreate;
impl Tool for PersonaCreate {
    fn name(&self) -> &str {
        "persona_create"
    }
    fn description(&self) -> &str {
        "Create a character persona (a role-play identity: name + role + voice + backstory) and, by \
         default, switch to it. Call this — do NOT just reply in prose — whenever the user asks you \
         to BE / become / invent a character, OR PASTES a character card / system prompt / persona \
         description and says make/create/save/turn-this-into a character. In the paste case, EXTRACT \
         the name (invent a fitting one if none) and rewrite the pasted text into `body` (values, \
         manner, how they speak, boundaries); don't ask for details you can infer from the paste. A \
         switch takes full effect from the user's next message. Not for facts about the user → use \
         memory. Writes to ~/.aizen/personas (the user confirms)."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "the character's name"},
                "role": {"type": "string", "description": "one line — who they are (e.g. 'a grumpy noir detective')"},
                "voice": {"type": "string", "description": "how they speak (e.g. 'terse, sardonic, clipped')"},
                "body": {"type": "string", "description": "backstory / values / manner / boundaries (markdown)"},
                "activate": {"type": "boolean", "description": "switch to this persona now (default true)"}
            },
            "required": ["name", "body"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let name = str_arg(args, "name")?;
        let body = str_arg(args, "body")?;
        let role = args.get("role").and_then(|v| v.as_str()).unwrap_or("");
        let voice = args.get("voice").and_then(|v| v.as_str()).unwrap_or("");
        let activate = args
            .get("activate")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let path = crate::persona::save(name, role, voice, body)?;
        if activate {
            crate::persona::set_active(name)?;
            Ok(format!(
                "created persona '{name}' → {} and switched to it (takes full effect from the \
                 user's next message).",
                path.display()
            ))
        } else {
            Ok(format!(
                "created persona '{name}' → {} (not active; switch with /persona).",
                path.display()
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("aizen-agent-tool-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn format_memory_hits_respects_the_token_budget() {
        use crate::memory::store::{MemoryEntry, MemoryType};
        let hit = |id: &str, scope: Option<&str>| crate::memory::Hit {
            entry: MemoryEntry {
                id: id.into(),
                name: id.into(),
                mtype: MemoryType::Reference,
                body: "word ".repeat(40),
                scope: scope.map(str::to_string),
                ..Default::default()
            },
            score: 1.0,
        };
        let hits: Vec<_> = (0..20).map(|i| hit(&format!("fact-{i}"), None)).collect();
        // Each line ≈ 55 tokens; a 120-token budget fits ~2 lines then cuts with a counted tail.
        let out = format_memory_hits(&hits, 120);
        assert!(out.contains("fact-0"), "{out}");
        assert!(out.contains("more hit(s) over the token budget"), "{out}");
        assert!(
            crate::memory::render::est_tokens(&out) <= 160,
            "output stays near the budget: {out}"
        );
        // zone tag renders for project-scoped hits
        let tagged = format_memory_hits(&[hit("zoned", Some("proj-00000001"))], 1200);
        assert!(tagged.contains("[p:proj-00000001]"), "{tagged}");
        // and the first hit always renders even when it alone exceeds the budget
        let one = format_memory_hits(&[hit("big", None)], 1);
        assert!(one.contains("big"));
    }

    #[test]
    fn glob_to_regex_matches_expected() {
        let re = regex::Regex::new(&glob_to_regex("src/**/*.rs")).unwrap();
        assert!(re.is_match("src/a.rs"));
        assert!(re.is_match("src/x/y/z.rs"));
        assert!(!re.is_match("src/a.ts"));
        assert!(!re.is_match("other/a.rs"));
        let re2 = regex::Regex::new(&glob_to_regex("*.md")).unwrap();
        assert!(re2.is_match("README.md"));
        assert!(!re2.is_match("docs/README.md"), "* must not cross /");
    }

    #[test]
    fn confine_resolves_relative_and_reaches_outside_root() {
        // Confinement was removed by user request: `confine` is now a pure path resolver, no escape
        // check. A relative in-tree path still resolves; a `..` path that leaves `root` now RESOLVES
        // (used to be rejected) so the agent can work on sibling projects the user points it at.
        let root = temp_root("confine");
        std::fs::write(root.join("ok.txt"), "hi").unwrap();
        assert!(
            confine(&root, "ok.txt", true).is_ok(),
            "in-tree path resolves"
        );
        // A create-target (must_exist=false) that escapes the root no longer errors — it resolves to
        // a path OUTSIDE root. Its parent (root's parent) exists, so canonicalize+rejoin succeeds.
        let escaped =
            confine(&root, "../ng-confine-escape.txt", false).expect("escape now resolves");
        assert!(
            !escaped.starts_with(&root),
            "resolved target is outside the root: {}",
            escaped.display()
        );
    }

    #[test]
    fn diff_preview_is_a_trimmed_unified_diff() {
        // A localized change in a longer block: the shared prefix/suffix collapse to ±2 context
        // lines, and the change renders as column-0 `-`/`+` lines (what the TUI colourises).
        let before = "1\n2\n3\n4\n5\nOLD\n7\n8\n9\n10";
        let after = "1\n2\n3\n4\n5\nNEW\n7\n8\n9\n10";
        let d = diff_preview(before, after);
        assert!(
            d.lines().any(|l| l == "-OLD"),
            "removed line, column-0 '-': {d:?}"
        );
        assert!(
            d.lines().any(|l| l == "+NEW"),
            "added line, column-0 '+': {d:?}"
        );
        assert!(
            d.lines().any(|l| l == " 5"),
            "keeps a leading context line: {d:?}"
        );
        assert!(
            d.lines().any(|l| l == " 7"),
            "keeps a trailing context line: {d:?}"
        );
        assert!(
            !d.contains("--- before") && !d.contains("+++ after"),
            "no old block headers: {d:?}"
        );
        assert!(
            !d.contains('1'),
            "far prefix/suffix bulk is trimmed away: {d:?}"
        );
    }

    #[test]
    fn file_read_reads_and_ranges() {
        let root = temp_root("read");
        std::fs::write(root.join("f.txt"), "l1\nl2\nl3\nl4").unwrap();
        let t = FileRead::new(root.clone());
        let all = t.execute(&serde_json::json!({"path":"f.txt"})).unwrap();
        assert_eq!(all, "l1\nl2\nl3\nl4");
        let mid = t
            .execute(&serde_json::json!({"path":"f.txt","start":2,"end":3}))
            .unwrap();
        assert_eq!(mid, "l2\nl3");
    }

    #[test]
    fn file_read_errors_on_missing_path() {
        // NOTE: confinement was removed — a `../` path is NOT rejected for escaping the root. This
        // errors only because the target doesn't exist (canonicalize fails). A `../` path that DOES
        // exist is read successfully by design (see `confine`'s doc). This test guards the
        // missing-file error, not a boundary that no longer exists.
        let root = temp_root("read-missing");
        let t = FileRead::new(root);
        assert!(t
            .execute(&serde_json::json!({"path":"../../nonexistent-xyzzy-secret"}))
            .is_err());
    }

    #[test]
    fn file_glob_lists_matches() {
        let root = temp_root("glob");
        std::fs::create_dir_all(root.join("src/sub")).unwrap();
        std::fs::write(root.join("src/a.rs"), "").unwrap();
        std::fs::write(root.join("src/sub/b.rs"), "").unwrap();
        std::fs::write(root.join("src/c.ts"), "").unwrap();
        let t = FileGlob::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern":"src/**/*.rs"}))
            .unwrap();
        assert!(out.contains("src/a.rs"));
        assert!(out.contains("src/sub/b.rs"));
        assert!(!out.contains("c.ts"));
    }

    #[test]
    fn file_glob_includes_hidden_and_build_dirs() {
        // Hidden files and heavy build dirs (target, node_modules, .git) are NO LONGER skipped —
        // the user asked file_glob to see everything.
        let root = temp_root("glob-hidden");
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join("target/debug/app.rs"), "").unwrap();
        std::fs::write(root.join(".hidden/secret.rs"), "").unwrap();
        std::fs::write(root.join(".env.rs"), "").unwrap();
        let t = FileGlob::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern":"**/*.rs"}))
            .unwrap();
        assert!(
            out.contains("target/debug/app.rs"),
            "build dir walked: {out}"
        );
        assert!(
            out.contains(".hidden/secret.rs"),
            "hidden dir walked: {out}"
        );
        assert!(out.contains(".env.rs"), "hidden file matched: {out}");
    }

    #[test]
    fn file_glob_reaches_outside_the_root() {
        // A `../sibling/...` pattern must escape the working dir (confinement removed, #67). Anchor
        // the tool at a subdir and glob back up into a sibling.
        let base = temp_root("glob-escape");
        std::fs::create_dir_all(base.join("proj_a")).unwrap();
        std::fs::create_dir_all(base.join("proj_b")).unwrap();
        std::fs::write(base.join("proj_b/level.js"), "").unwrap();
        let t = FileGlob::new(base.join("proj_a").canonicalize().unwrap());
        let out = t
            .execute(&serde_json::json!({"pattern":"../proj_b/**/*.js"}))
            .unwrap();
        assert!(
            out.contains("level.js"),
            "should reach the sibling project: {out}"
        );
    }

    #[test]
    fn file_glob_fuzzy_fallback_suggests_near_names() {
        // No glob match → fall back to a fuzzy match on the file NAME (#68). A typo'd needle should
        // still surface the real file among the closest suggestions.
        let root = temp_root("glob-fuzzy");
        std::fs::write(root.join("snake_game.js"), "").unwrap();
        std::fs::write(root.join("readme.md"), "").unwrap();
        let t = FileGlob::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern":"snakegame.js"}))
            .unwrap();
        assert!(
            out.contains("closest matches"),
            "fuzzy header present: {out}"
        );
        assert!(out.contains("snake_game.js"), "near-name surfaced: {out}");
    }

    #[test]
    fn file_glob_matches_directories_not_just_files() {
        // "Find the X folder" must return the DIRECTORY itself, not only files under it.
        let root = temp_root("glob-dirs");
        std::fs::create_dir_all(root.join("src/mini_project")).unwrap();
        std::fs::write(root.join("src/mini_project/main.rs"), "").unwrap();
        let t = FileGlob::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern":"**/mini_project"}))
            .unwrap();
        assert!(
            out.contains("src/mini_project"),
            "the folder itself is listed: {out}"
        );
    }

    #[test]
    fn file_glob_fuzzy_finds_parent_folder_by_near_name() {
        // The workspace is often a subdir (e.g. .../mini_project/aizen). Asking for the parent
        // folder by a separator-insensitive near name ("miniproject") must surface "mini_project"
        // even though it lives ABOVE the anchor — a downward-only walk never reaches it.
        let base = temp_root("glob-parent");
        let ws = base.join("mini_project").join("aizen");
        std::fs::create_dir_all(&ws).unwrap();
        let t = FileGlob::new(ws.canonicalize().unwrap());
        let out = t
            .execute(&serde_json::json!({"pattern":"miniproject"}))
            .unwrap();
        assert!(
            out.contains("mini_project"),
            "parent folder surfaced by fuzzy: {out}"
        );
        assert!(out.contains("(folder)"), "it's tagged as a folder: {out}");
    }

    #[test]
    fn file_glob_smart_case_insensitive_by_default() {
        // A lowercase pattern matches case-insensitively (a model types `readme.md`, means
        // `README.md`); an uppercase letter in the pattern makes the match case-sensitive.
        let root = temp_root("glob-case");
        std::fs::write(root.join("README.md"), "").unwrap();
        let t = FileGlob::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern":"readme.md"}))
            .unwrap();
        assert!(
            out.contains("README.md"),
            "lowercase pattern is case-insensitive: {out}"
        );
    }

    #[test]
    fn file_glob_ranks_exact_name_first() {
        // With several matches, the EXACT-name file must sort to line 1 (best-first ranking, P1.3),
        // so a model that reads only the first line still gets the right answer.
        let root = temp_root("glob-rank");
        std::fs::create_dir_all(root.join("a")).unwrap();
        std::fs::create_dir_all(root.join("b")).unwrap();
        std::fs::write(root.join("a/config.toml.bak"), "").unwrap();
        std::fs::write(root.join("b/config.toml"), "").unwrap();
        let t = FileGlob::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern":"**/config.toml"}))
            .unwrap();
        let first = out.lines().next().unwrap_or("");
        assert!(
            first.ends_with("b/config.toml"),
            "exact name ranked first: {out}"
        );
    }

    #[test]
    fn file_glob_no_match_message_is_not_negative_prose() {
        // A true no-match must NOT open with parenthetical negative prose (the old "(no exact
        // match…)" that made models distrust the tool and shell out). It names where it looked.
        let root = temp_root("glob-nomatch");
        std::fs::write(root.join("alpha.rs"), "").unwrap();
        let t = FileGlob::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern":"zzqwx_nonexistent_qq"}))
            .unwrap();
        assert!(
            !out.starts_with('('),
            "no leading negative parenthetical: {out}"
        );
        assert!(out.contains("searched"), "says where it looked: {out}");
    }

    #[test]
    fn file_edit_replaces_uniquely() {
        let root = temp_root("edit");
        std::fs::write(root.join("f.txt"), "hello world").unwrap();
        let t = FileEdit::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"path":"f.txt","old_string":"world","new_string":"rust"}))
            .unwrap();
        assert!(r.contains("edited"));
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "hello rust"
        );
    }

    #[test]
    fn file_read_number_prefixes_lines() {
        let root = temp_root("read-num");
        std::fs::write(root.join("f.txt"), "alpha\nbeta\ngamma").unwrap();
        let t = FileRead::new(root);
        let out = t
            .execute(&serde_json::json!({"path":"f.txt","number":true}))
            .unwrap();
        assert_eq!(out, "1|alpha\n2|beta\n3|gamma");
        // default (no number) stays byte-exact so old_string round-trips into file_edit cleanly
        let plain = t.execute(&serde_json::json!({"path":"f.txt"})).unwrap();
        assert_eq!(plain, "alpha\nbeta\ngamma");
    }

    #[test]
    fn budget_view_under_cap_is_verbatim() {
        // Small file with CRLF + trailing newline → byte-identical (the round-trip invariant).
        let c = "a\r\nb\r\nc\r\n";
        assert_eq!(budget_view(c, "f", 2000, 200_000), c);
    }

    #[test]
    fn budget_view_over_lines_marks_headtail_byte_exact() {
        let c = "L1\r\nL2\r\nL3\r\nL4\r\nL5\r\n";
        let out = budget_view(c, "f.txt", 4, 0); // line cap 4, byte cap disabled
        assert!(
            out.contains("over the 4-line read budget"),
            "loud marker: {out}"
        );
        assert!(
            out.contains("L1\r\nL2\r\n"),
            "head slice is byte-exact incl CRLF"
        );
        assert!(out.contains("L5"), "tail slice present");
        assert!(!out.contains("L3"), "the omitted middle is not shown");
        assert!(
            out.contains("lines omitted: 3-4"),
            "names the omitted range: {out}"
        );
    }

    #[test]
    fn budget_view_over_bytes_one_long_line() {
        let c = "x".repeat(5000);
        let out = budget_view(&c, "min.js", 0, 2048); // byte cap only; one giant line
        assert!(out.starts_with("[file_read:"), "marker first");
        assert!(out.contains("KB read budget"));
        assert!(out.contains("bytes omitted"));
        assert!(
            out.len() < c.len(),
            "result is bounded below the original size"
        );
    }

    #[test]
    fn budget_view_disabled_when_zero() {
        let c = "line\n".repeat(5000);
        assert_eq!(budget_view(&c, "f", 0, 0), c, "both caps 0 → verbatim");
    }

    #[test]
    fn file_read_over_budget_via_execute() {
        // The real consts (2000 lines): a whole-file read of a 2500-line file is trimmed + marked.
        let root = temp_root("read-budget");
        let big: String = (1..=2500)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("big.txt"), &big).unwrap();
        let t = FileRead::new(root);
        let out = t.execute(&serde_json::json!({"path":"big.txt"})).unwrap();
        assert!(
            out.contains("over the 2000-line read budget"),
            "marker present"
        );
        assert!(out.contains("line1\n"), "head present");
        assert!(out.contains("line2500"), "tail present");
        assert!(
            !out.contains("line1300\n"),
            "the omitted middle is not shown"
        );
    }

    #[test]
    fn file_read_explicit_range_skips_budget() {
        // An explicit start/end on the SAME large file returns the exact range — never re-bounded.
        let root = temp_root("read-budget-range");
        let big: String = (1..=2500)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("big.txt"), &big).unwrap();
        let t = FileRead::new(root);
        let out = t
            .execute(&serde_json::json!({"path":"big.txt","start":5,"end":7}))
            .unwrap();
        assert_eq!(out, "line5\nline6\nline7");
        assert!(
            !out.contains("read budget"),
            "range reads are never budgeted"
        );
    }

    #[test]
    fn ladder_ws_normalized_matches_interior_spacing() {
        // Interior spacing differs (`foo( a,  b )` vs `foo(a, b)`) — R2 trim can't fix that, R3 can.
        let content = "start\n    foo( a,  b );\nend\n";
        let a = apply_one_edit(content, "foo(a, b);", "bar(a, b);", false, "t").unwrap();
        assert_eq!(a.rung, "ws-norm");
        assert!(a.content.contains("bar(a, b);"), "{}", a.content);
        assert!(a.summary().contains("whitespace-normalized"));
    }

    #[test]
    fn ladder_anchor_trim_only_when_context_shared() {
        // First line of old == first line of new (pure context) but that line is WRONG in old
        // ("fn mian" typo'd context) — dropping it from BOTH sides rescues the edit.
        let content = "fn main() {\n    body();\n}\n";
        let old = "fn mian() {\n    body();"; // first line mis-copied
        let new = "fn mian() {\n    new_body();"; // …but identical in new → shared context
        let a = apply_one_edit(content, old, new, false, "t").unwrap();
        assert_eq!(a.rung, "anchor-trim");
        assert!(a.content.contains("new_body();"), "{}", a.content);
        assert!(
            a.content.contains("fn main() {"),
            "the real context line is untouched"
        );
        // NOT shared (the first line actually differs between old and new) → no R4, hard failure.
        let old2 = "fn mian() {\n    body();";
        let new2 = "fn other() {\n    new_body();";
        assert!(
            apply_one_edit(content, old2, new2, false, "t").is_err(),
            "no anchor-trim without shared context"
        );
    }

    #[test]
    fn ladder_ambiguous_at_any_rung_refuses() {
        // Two ws-normalized matches → hard error, must NOT fall through to a looser rung.
        let content = "foo( a );\nfoo(  a );\n";
        let err = apply_one_edit(content, "foo(a );", "bar();", false, "t")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("2") && err.contains("t"),
            "ambiguity is a hard refusal: {err}"
        );
    }

    #[test]
    fn ladder_unescape_applies_pair() {
        // The model shipped literal \n escapes in BOTH strings — R5 unescapes the pair together.
        let content = "alpha\nbeta\ngamma\n";
        let a = apply_one_edit(content, "alpha\\nbeta", "alpha\\nBETA", false, "t").unwrap();
        assert_eq!(a.rung, "unescape");
        assert!(a.content.contains("alpha\nBETA\ngamma"), "{}", a.content);
    }

    #[test]
    fn ladder_blank_line_insensitive_match() {
        // The file has a blank line INSIDE the block that the model's old_string omitted — every
        // fixed-`k` rung misses (line counts differ), but R5.5 matches on the non-blank lines and
        // splices the whole spanned region (blank included) so `new` redefines it.
        let content = "fn f() {\n    let a = 1;\n\n    let b = 2;\n}\n";
        let old = "let a = 1;\n    let b = 2;"; // no blank between — 2 lines vs the file's 3
        let new = "let a = 10;\n    let b = 20;";
        let a = apply_one_edit(content, old, new, false, "t").unwrap();
        assert_eq!(a.rung, "blank-norm");
        assert!(a.content.contains("let a = 10;"), "{}", a.content);
        assert!(a.content.contains("let b = 20;"), "{}", a.content);
        assert!(
            !a.content.contains("let a = 1;"),
            "old first line replaced: {}",
            a.content
        );
        assert!(a.summary().contains("blank-line-insensitive"));
    }

    #[test]
    fn ladder_blank_norm_ambiguous_refuses() {
        // Both occurrences have a blank INSIDE them, so old (no blank) misses every fixed-`k` rung
        // and only R5.5 matches — twice → hard error, never a silent wrong splice.
        let content = "a = 1;\n\nb = 2;\nMID\na = 1;\n\nb = 2;\n";
        let err = apply_one_edit(content, "a = 1;\nb = 2;", "z = 9;", false, "t")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("blank lines") && err.contains('2'),
            "ambiguity is a hard refusal: {err}"
        );
    }

    #[test]
    fn ladder_nearest_miss_reports_line_numbers() {
        let content = (1..=30)
            .map(|i| format!("line number {i} content"))
            .collect::<Vec<_>>()
            .join("\n");
        let err = apply_one_edit(&content, "line number 17 contnet", "x", false, "f.rs")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Nearest match"), "{err}");
        assert!(
            err.contains("17| "),
            "the real line is quoted with its number: {err}"
        );
        assert!(err.contains("Copy old_string EXACTLY"), "{err}");
    }

    #[test]
    fn ladder_no_similar_region_keeps_plain_error() {
        let content = "completely different text\n";
        let err = apply_one_edit(content, "zzz qqq www", "x", false, "f.rs")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
        assert!(
            !err.contains("Nearest match"),
            "below the similarity floor → no random quote: {err}"
        );
    }

    #[test]
    fn json_unescape_only_reports_real_changes() {
        assert_eq!(json_unescape("a\\nb").as_deref(), Some("a\nb"));
        assert_eq!(json_unescape("a\\\\b").as_deref(), Some("a\\b"));
        assert_eq!(json_unescape("plain"), None, "no backslash → no rung");
        assert_eq!(
            json_unescape("C:\\path\\dir"),
            None,
            "unknown escapes alone don't count as a change"
        );
    }

    #[test]
    fn file_edit_indentation_tolerant_fallback() {
        let root = temp_root("edit-indent");
        // File uses 4-space indentation; the model's old_string uses 2 spaces (a real mismatch).
        std::fs::write(
            root.join("f.rs"),
            "fn main() {\n    let x = 1;\n    foo();\n}\n",
        )
        .unwrap();
        let t = FileEdit::new(root.clone());
        let r = t
            .execute(&serde_json::json!({
                "path": "f.rs",
                "old_string": "  let x = 1;\n  foo();",      // wrong indentation, right content
                "new_string": "    let x = 2;\n    bar();"
            }))
            .unwrap();
        assert!(r.contains("indentation-tolerant"), "got: {r}");
        // the unified diff preview shows the removed/added lines (column-0 -/+ markers)
        assert!(
            r.lines().any(|l| l.starts_with('-')),
            "diff shows a removed line: {r}"
        );
        assert!(
            r.lines().any(|l| l.starts_with('+')),
            "diff shows an added line: {r}"
        );
        let after = std::fs::read_to_string(root.join("f.rs")).unwrap();
        assert_eq!(after, "fn main() {\n    let x = 2;\n    bar();\n}\n");
    }

    #[test]
    fn file_edit_preserves_crlf_via_tolerant_rung() {
        let root = temp_root("edit-crlf");
        // A Windows (CRLF) file; the model's old_string is bare-LF with the wrong indent — a real
        // mismatch the indent-tolerant rung rescues. The replaced line MUST stay CRLF, not silently
        // degrade to a lone-LF line among CRLF neighbours (Windows mixed-ending corruption).
        std::fs::write(root.join("f.rs"), "a\r\n    b\r\nc\r\n").unwrap();
        let t = FileEdit::new(root.clone());
        t.execute(&serde_json::json!({"path":"f.rs","old_string":"  b","new_string":"X"}))
            .unwrap();
        let after = std::fs::read_to_string(root.join("f.rs")).unwrap();
        // The invariant is line endings, not indentation (the indent rung reapplies leading space):
        // the edited line is written with CRLF, and no lone-LF line survives anywhere.
        assert!(
            after.contains("X\r\n"),
            "edited line written with CRLF: {after:?}"
        );
        assert!(
            !after.replace("\r\n", "").contains('\n'),
            "no lone-LF line survived the edit: {after:?}"
        );
    }

    #[test]
    fn preserve_eol_restores_boundary_and_internal_crlf() {
        // CRLF block, bare-LF replacement, `\n` sits at the boundary → restore the trailing `\r`.
        assert_eq!(preserve_eol("X", "b\r", "\nc"), "X\r");
        // Multi-line bare-LF new against a CRLF region → every internal break becomes CRLF too.
        assert_eq!(preserve_eol("X\nY", "b\r", "\nc"), "X\r\nY\r");
        // LF file → untouched (byte-identical).
        assert_eq!(preserve_eol("X", "b", "\nc"), "X");
        assert_eq!(preserve_eol("X\nY", "b", "\nc"), "X\nY");
    }

    #[test]
    fn file_edit_ambiguous_tolerant_match_refuses() {
        let root = temp_root("edit-indent-dup");
        std::fs::write(root.join("f.txt"), "  a\nb\n    a\nb\n").unwrap();
        let t = FileEdit::new(root);
        // "a\nb" matches two blocks once indentation is ignored → refuse, don't corrupt.
        let r =
            t.execute(&serde_json::json!({"path":"f.txt","old_string":"a\nb","new_string":"X"}));
        assert!(r.is_err(), "ambiguous tolerant match must refuse");
    }

    #[test]
    fn file_edit_rejects_nonunique_without_replace_all() {
        let root = temp_root("edit-dup");
        std::fs::write(root.join("f.txt"), "a a a").unwrap();
        let t = FileEdit::new(root);
        assert!(t
            .execute(&serde_json::json!({"path":"f.txt","old_string":"a","new_string":"b"}))
            .is_err());
    }

    #[test]
    fn file_edit_creates_new_when_old_empty() {
        let root = temp_root("edit-new");
        let t = FileEdit::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"path":"new.txt","old_string":"","new_string":"content"}))
            .unwrap();
        assert!(r.contains("created"));
        assert_eq!(
            std::fs::read_to_string(root.join("new.txt")).unwrap(),
            "content"
        );
    }

    #[test]
    fn file_edit_is_destructive() {
        assert!(FileEdit::new(PathBuf::from(".")).is_destructive());
        assert!(ShellRun::new(PathBuf::from(".")).is_destructive());
        assert!(!FileRead::new(PathBuf::from(".")).is_destructive());
    }

    #[test]
    fn write_tools_point_the_checkpoint_gate_at_the_target_not_the_cwd() {
        // THE invariant behind the "run `git init`" misfire: the gate must look for a work tree
        // where the write LANDS. A session rooted in a home directory editing a project two levels
        // down asked about the wrong directory, found no repo, and told the user to `git init` —
        // which, followed literally, inits the home directory. Every path-naming tool must name its
        // own destination so the lookup happens in the project.
        let root = temp_root("target-dir");
        let proj = root.join("mini_project").join("web");
        std::fs::create_dir_all(proj.join("src")).unwrap();

        // A relative path resolves under root, and the DIRECTORY (not the file) is reported.
        let w = FileWrite::new(root.clone());
        assert_eq!(
            w.workspace_target(&serde_json::json!({"path":"mini_project/web/src/app.js"})),
            Some(proj.join("src")),
            "file_write must point at the file's parent directory"
        );
        // Same for a not-yet-existing file: the parent is what a repo lookup needs.
        assert_eq!(
            w.workspace_target(&serde_json::json!({"path":"mini_project/web/src/brand-new.js"})),
            Some(proj.join("src"))
        );
        // An absolute path passes through unchanged (never re-anchored under root).
        let outside = temp_root("target-dir-abs");
        let abs = outside.join("x.txt");
        assert_eq!(
            w.workspace_target(&serde_json::json!({"path": abs.to_string_lossy()})),
            Some(outside.clone())
        );
        // file_edit and multi_edit share the contract.
        assert_eq!(
            FileEdit::new(root.clone())
                .workspace_target(&serde_json::json!({"path":"mini_project/web/src/app.js"})),
            Some(proj.join("src"))
        );
        assert_eq!(
            MultiEdit::new(root.clone())
                .workspace_target(&serde_json::json!({"path":"mini_project/web/src/app.js"})),
            Some(proj.join("src"))
        );
        // file_move must snapshot the SOURCE — that is the side whose content disappears.
        assert_eq!(
            FileMove::new(root.clone()).workspace_target(
                &serde_json::json!({"from":"mini_project/web/src/app.js","to":"other/app.js"})
            ),
            Some(proj.join("src"))
        );
        // A malformed / absent path names no directory rather than guessing at one.
        assert_eq!(w.workspace_target(&serde_json::json!({})), None);
        // shell_run names no path, so it keeps the cwd-relative behavior by declining to answer.
        assert_eq!(
            ShellRun::new(root).workspace_target(&serde_json::json!({"command":"ls"})),
            None
        );
    }

    #[test]
    fn file_write_creates_and_overwrites() {
        let root = temp_root("write");
        let t = FileWrite::new(root.clone());
        // create
        let r = t
            .execute(&serde_json::json!({"path":"a.txt","content":"one\ntwo\n"}))
            .unwrap();
        assert!(r.starts_with("created a.txt"), "{r:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\ntwo\n"
        );
        // overwrite an EXISTING non-empty file wholesale — the case that used to force `type NUL >`.
        let r2 = t
            .execute(&serde_json::json!({"path":"a.txt","content":"brand new\n"}))
            .unwrap();
        assert!(r2.starts_with("overwrote a.txt"), "{r2:?}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "brand new\n"
        );
        assert!(t.is_destructive() && !t.is_concurrency_safe());
    }

    #[test]
    fn file_write_reaches_outside_but_needs_parent() {
        let root = temp_root("write-guard");
        let t = FileWrite::new(root.clone());
        // Confinement was REMOVED: a `../` path now writes to the sibling location (its parent, the
        // temp dir, exists) instead of being refused. Clean up the file we create outside root.
        let outside = root.parent().unwrap().join("evil.txt");
        let _ = std::fs::remove_file(&outside);
        assert!(t
            .execute(&serde_json::json!({"path":"../evil.txt","content":"x"}))
            .is_ok());
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "x");
        let _ = std::fs::remove_file(&outside);
        // The one remaining guard: writing into a non-existent subdir still errors (parent must exist).
        assert!(t
            .execute(&serde_json::json!({"path":"nope/deep.txt","content":"x"}))
            .is_err());
    }

    #[test]
    fn multi_edit_applies_ordered_edits() {
        let root = temp_root("medit-order");
        std::fs::write(root.join("f.txt"), "alpha beta gamma").unwrap();
        let t = MultiEdit::new(root.clone());
        let r = t
            .execute(&serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "alpha", "new_string": "A"},
                    {"old_string": "gamma", "new_string": "G"}
                ]
            }))
            .unwrap();
        assert!(r.contains("2 edits applied"), "got: {r}");
        assert!(r.contains("#1") && r.contains("#2"));
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "A beta G"
        );
    }

    #[test]
    fn multi_edit_sequential_sees_prior_result() {
        // Edit #2 targets text PRODUCED by edit #1 (evolving-buffer proof).
        let root = temp_root("medit-evolve");
        std::fs::write(root.join("f.txt"), "one").unwrap();
        let t = MultiEdit::new(root.clone());
        t.execute(&serde_json::json!({
            "path": "f.txt",
            "edits": [
                {"old_string": "one", "new_string": "two"},
                {"old_string": "two", "new_string": "three"}
            ]
        }))
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "three"
        );
    }

    #[test]
    fn multi_edit_atomic_rollback_on_failure() {
        // Edit #1 valid, #2 absent → whole call fails and the file equals the ORIGINAL (atomicity).
        let root = temp_root("medit-atomic");
        std::fs::write(root.join("f.txt"), "keep me exactly").unwrap();
        let t = MultiEdit::new(root.clone());
        let r = t.execute(&serde_json::json!({
            "path": "f.txt",
            "edits": [
                {"old_string": "keep", "new_string": "KEEP"},
                {"old_string": "does-not-exist", "new_string": "X"}
            ]
        }));
        assert!(r.is_err(), "a failing edit must abort the whole call");
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "keep me exactly",
            "nothing must be written when any edit fails"
        );
    }

    #[test]
    fn multi_edit_reports_failing_index() {
        let root = temp_root("medit-idx");
        std::fs::write(root.join("f.txt"), "a b c").unwrap();
        let t = MultiEdit::new(root);
        let err = t
            .execute(&serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "a", "new_string": "A"},
                    {"old_string": "zzz", "new_string": "Z"}
                ]
            }))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("edit #2"),
            "error names the failing edit index: {err}"
        );
    }

    #[test]
    fn multi_edit_replace_all_per_edit() {
        let root = temp_root("medit-all");
        std::fs::write(root.join("f.txt"), "x x x | y").unwrap();
        let t = MultiEdit::new(root.clone());
        t.execute(&serde_json::json!({
            "path": "f.txt",
            "edits": [
                {"old_string": "x", "new_string": "Z", "replace_all": true},
                {"old_string": "y", "new_string": "Y"}
            ]
        }))
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "Z Z Z | Y"
        );
        // A non-unique edit WITHOUT replace_all aborts atomically.
        std::fs::write(root.join("g.txt"), "a a").unwrap();
        let r = t.execute(&serde_json::json!({
            "path": "g.txt",
            "edits": [{"old_string": "a", "new_string": "b"}]
        }));
        assert!(r.is_err(), "non-unique without replace_all must error");
        assert_eq!(std::fs::read_to_string(root.join("g.txt")).unwrap(), "a a");
    }

    #[test]
    fn multi_edit_indent_tolerant_per_edit() {
        // A 2-line block with wrong indentation on BOTH lines isn't an accidental exact substring,
        // so it genuinely exercises the indentation-tolerant fallback (mirrors the file_edit test).
        let root = temp_root("medit-indent");
        std::fs::write(
            root.join("f.rs"),
            "fn main() {\n    let x = 1;\n    foo();\n}\n",
        )
        .unwrap();
        let t = MultiEdit::new(root.clone());
        let r = t
            .execute(&serde_json::json!({
                "path": "f.rs",
                "edits": [{"old_string": "  let x = 1;\n  foo();", "new_string": "    let x = 2;\n    bar();"}]
            }))
            .unwrap();
        assert!(r.contains("indentation-tolerant"), "got: {r}");
        assert_eq!(
            std::fs::read_to_string(root.join("f.rs")).unwrap(),
            "fn main() {\n    let x = 2;\n    bar();\n}\n"
        );
    }

    #[test]
    fn multi_edit_empty_edits_array_errs() {
        let root = temp_root("medit-empty");
        std::fs::write(root.join("f.txt"), "hi").unwrap();
        let t = MultiEdit::new(root);
        assert!(t
            .execute(&serde_json::json!({"path": "f.txt", "edits": []}))
            .is_err());
    }

    #[test]
    fn multi_edit_rejects_escape() {
        let root = temp_root("medit-escape");
        let t = MultiEdit::new(root);
        let r = t.execute(&serde_json::json!({
            "path": "../../secret",
            "edits": [{"old_string": "a", "new_string": "b"}]
        }));
        assert!(r.is_err(), "path escaping the working dir must be refused");
    }

    #[test]
    fn multi_edit_nonexistent_file_errs() {
        let root = temp_root("medit-nofile");
        let t = MultiEdit::new(root);
        assert!(t
            .execute(&serde_json::json!({"path": "nope.txt", "edits": [{"old_string": "a", "new_string": "b"}]}))
            .is_err());
    }

    #[test]
    fn multi_edit_is_destructive_not_concurrency_safe() {
        let t = MultiEdit::new(PathBuf::from("."));
        assert!(t.is_destructive(), "must be approval-gated like file_edit");
        assert!(!t.is_concurrency_safe(), "must take the serial path");
    }

    #[test]
    fn multi_edit_empty_old_string_errs() {
        // Mid-edit create-new is meaningless; apply_one_edit rejects an empty old_string.
        let root = temp_root("medit-emptyold");
        std::fs::write(root.join("f.txt"), "hi").unwrap();
        let t = MultiEdit::new(root.clone());
        let r = t.execute(&serde_json::json!({
            "path": "f.txt",
            "edits": [{"old_string": "", "new_string": "X"}]
        }));
        assert!(r.is_err(), "empty old_string mid-edit must error");
        assert_eq!(
            std::fs::read_to_string(root.join("f.txt")).unwrap(),
            "hi",
            "nothing written"
        );
    }

    #[test]
    fn file_write_noop_on_identical_content_does_not_touch_disk() {
        // W16: overwriting a file with the bytes it already holds must not rewrite it (no mtime
        // churn) and must return the no-op marker (so the loop won't arm the verify gate).
        let root = temp_root("fw-noop");
        let p = root.join("f.txt");
        std::fs::write(&p, "same\n").unwrap();
        let mtime_before = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let t = FileWrite::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"path": "f.txt", "content": "same\n"}))
            .unwrap();
        assert!(r.starts_with(NOOP_WRITE_PREFIX), "got: {r}");
        assert_eq!(
            std::fs::metadata(&p).unwrap().modified().unwrap(),
            mtime_before,
            "no-op overwrite must not rewrite the file"
        );
    }

    #[test]
    fn file_write_empty_new_file_is_a_real_op() {
        // A create with empty content is a genuine op (the file did not exist) — not a no-op.
        let root = temp_root("fw-newempty");
        let t = FileWrite::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"path": "new.txt", "content": ""}))
            .unwrap();
        assert!(
            !r.starts_with(NOOP_WRITE_PREFIX),
            "creating a new file is never a no-op: {r}"
        );
        assert!(root.join("new.txt").exists());
    }

    #[test]
    fn file_edit_noop_when_old_equals_new() {
        // W16: old_string == new_string produces byte-identical content → no write, no-op marker.
        let root = temp_root("fe-noop");
        let p = root.join("f.txt");
        std::fs::write(&p, "alpha beta\n").unwrap();
        let mtime_before = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let t = FileEdit::new(root.clone());
        let r = t
            .execute(
                &serde_json::json!({"path": "f.txt", "old_string": "beta", "new_string": "beta"}),
            )
            .unwrap();
        assert!(r.starts_with(NOOP_WRITE_PREFIX), "got: {r}");
        assert_eq!(
            std::fs::metadata(&p).unwrap().modified().unwrap(),
            mtime_before
        );
    }

    #[test]
    fn multi_edit_noop_when_edits_net_to_original() {
        // W16: two edits that cancel out (X→Y then Y→X) net to the original → no write, no-op marker.
        let root = temp_root("me-noop");
        let p = root.join("f.txt");
        std::fs::write(&p, "one two three\n").unwrap();
        let mtime_before = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let t = MultiEdit::new(root.clone());
        let r = t
            .execute(&serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"old_string": "two", "new_string": "TWO"},
                    {"old_string": "TWO", "new_string": "two"}
                ]
            }))
            .unwrap();
        assert!(r.starts_with(NOOP_WRITE_PREFIX), "got: {r}");
        assert_eq!(
            std::fs::metadata(&p).unwrap().modified().unwrap(),
            mtime_before
        );
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one two three\n");
    }

    #[test]
    fn file_move_renames_a_file() {
        let root = temp_root("fmv-rename");
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        let t = FileMove::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"from": "a.txt", "to": "b.txt"}))
            .unwrap();
        assert!(r.starts_with("moved file"), "got: {r}");
        assert!(!root.join("a.txt").exists(), "source is gone after a move");
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "hello\n"
        );
    }

    #[test]
    fn file_move_errors_on_missing_source() {
        let root = temp_root("fmv-missing");
        let t = FileMove::new(root.clone());
        let err = t
            .execute(&serde_json::json!({"from": "nope.txt", "to": "x.txt"}))
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("nope.txt"),
            "error names the source: {err:#}"
        );
    }

    #[test]
    fn file_move_wont_clobber_without_overwrite() {
        let root = temp_root("fmv-clobber");
        std::fs::write(root.join("a.txt"), "A\n").unwrap();
        std::fs::write(root.join("b.txt"), "B\n").unwrap();
        let t = FileMove::new(root.clone());
        // Default: refuse to overwrite an existing destination.
        let err = t
            .execute(&serde_json::json!({"from": "a.txt", "to": "b.txt"}))
            .unwrap_err();
        assert!(
            format!("{err:#}").contains("already exists"),
            "got: {err:#}"
        );
        // Both files must be untouched after the refusal.
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "A\n");
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "B\n");
    }

    #[test]
    fn file_move_overwrite_replaces_destination() {
        let root = temp_root("fmv-overwrite");
        std::fs::write(root.join("a.txt"), "A\n").unwrap();
        std::fs::write(root.join("b.txt"), "B\n").unwrap();
        let t = FileMove::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"from": "a.txt", "to": "b.txt", "overwrite": true}))
            .unwrap();
        assert!(r.starts_with("moved file"), "got: {r}");
        assert!(!root.join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("b.txt")).unwrap(),
            "A\n",
            "dest now holds source content"
        );
    }

    #[test]
    fn file_move_create_dirs_makes_parents() {
        let root = temp_root("fmv-mkdirs");
        std::fs::write(root.join("a.txt"), "hi\n").unwrap();
        let t = FileMove::new(root.clone());
        // Destination parent (`nested/deep/`) does not exist yet.
        let no_parent = t.execute(&serde_json::json!({"from": "a.txt", "to": "nested/deep/b.txt"}));
        assert!(
            no_parent.is_err(),
            "missing parent errors without create_dirs"
        );
        // With create_dirs it makes the parents and moves.
        let r = t
            .execute(&serde_json::json!({"from": "a.txt", "to": "nested/deep/b.txt", "create_dirs": true}))
            .unwrap();
        assert!(r.starts_with("moved file"), "got: {r}");
        assert_eq!(
            std::fs::read_to_string(root.join("nested/deep/b.txt")).unwrap(),
            "hi\n"
        );
    }

    #[test]
    fn file_move_moves_a_directory() {
        let root = temp_root("fmv-dir");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/f.txt"), "x\n").unwrap();
        let t = FileMove::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"from": "src", "to": "dst"}))
            .unwrap();
        assert!(r.starts_with("moved directory"), "got: {r}");
        assert!(!root.join("src").exists());
        assert_eq!(
            std::fs::read_to_string(root.join("dst/f.txt")).unwrap(),
            "x\n"
        );
    }

    #[test]
    fn file_move_same_path_is_a_noop() {
        let root = temp_root("fmv-noop");
        std::fs::write(root.join("a.txt"), "keep\n").unwrap();
        let t = FileMove::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"from": "a.txt", "to": "a.txt"}))
            .unwrap();
        assert!(r.starts_with(NOOP_WRITE_PREFIX), "got: {r}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "keep\n"
        );
    }

    #[test]
    fn file_move_is_destructive_and_arms_verify_gate() {
        // The verify gate arms on is_destructive() (not a tool-name list), so file_move must report
        // destructive to be verified like file_edit/file_write.
        assert!(FileMove::new(PathBuf::from(".")).is_destructive());
        assert!(!FileMove::new(PathBuf::from(".")).is_concurrency_safe());
    }

    #[test]
    fn atomic_write_replaces_content_and_leaves_no_temp() {
        let root = temp_root("atomic-write");
        let target = root.join("f.txt");
        // Fresh create.
        atomic_write(&target, b"first\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "first\n");
        // Overwrite.
        atomic_write(&target, b"second\n").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "second\n");
        // No sibling temp file survives (the rename consumed it, no failure path leaked one).
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".aizen-tmp."))
            .collect();
        assert!(leftovers.is_empty(), "temp file leaked: {leftovers:?}");
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_preserves_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("atomic-perms");
        let target = root.join("script.sh");
        atomic_write(&target, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A rewrite must carry the executable bit over (temp files are created 0644 by default).
        atomic_write(&target, b"#!/bin/sh\necho hi\n").unwrap();
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "rewrite reset the mode: {:o}",
            mode & 0o777
        );
    }

    #[test]
    fn file_write_is_atomic_and_leaves_no_temp() {
        // The file_write tool goes through atomic_write; a successful overwrite must leave the target
        // holding exactly the new bytes and no staging temp behind.
        let root = temp_root("fw-atomic");
        std::fs::write(root.join("a.txt"), "old\n").unwrap();
        let t = FileWrite::new(root.clone());
        let r = t
            .execute(&serde_json::json!({"path": "a.txt", "content": "new\n"}))
            .unwrap();
        assert!(r.starts_with("overwrote"), "got: {r}");
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "new\n"
        );
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                n.contains(".aizen-tmp.") || n.contains(".aizen-stash.")
            })
            .collect();
        assert!(leftovers.is_empty(), "staging file leaked: {leftovers:?}");
    }

    #[test]
    fn file_move_overwrite_leaves_no_stash_behind() {
        // The overwrite path stashes the old destination out of the way, moves, then deletes the
        // stash. On success no `.aizen-stash.` sibling may survive.
        let root = temp_root("fmv-stash");
        std::fs::write(root.join("a.txt"), "A\n").unwrap();
        std::fs::write(root.join("b.txt"), "B\n").unwrap();
        let t = FileMove::new(root.clone());
        t.execute(&serde_json::json!({"from": "a.txt", "to": "b.txt", "overwrite": true}))
            .unwrap();
        assert_eq!(std::fs::read_to_string(root.join("b.txt")).unwrap(), "A\n");
        assert!(!root.join("a.txt").exists());
        let leftovers: Vec<_> = std::fs::read_dir(&root)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".aizen-stash."))
            .collect();
        assert!(leftovers.is_empty(), "stash file leaked: {leftovers:?}");
    }

    #[test]
    fn file_move_overwrite_dir_replaces_contents() {
        // Overwriting a directory must leave the destination holding the SOURCE tree, not a merge
        // with the old destination's files (the stash removes the old dst entirely before the move).
        let root = temp_root("fmv-dir-overwrite");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/new.txt"), "new\n").unwrap();
        std::fs::create_dir_all(root.join("dst")).unwrap();
        std::fs::write(root.join("dst/old.txt"), "old\n").unwrap();
        let t = FileMove::new(root.clone());
        t.execute(&serde_json::json!({"from": "src", "to": "dst", "overwrite": true}))
            .unwrap();
        assert!(
            root.join("dst/new.txt").exists(),
            "dst holds the source tree"
        );
        assert!(
            !root.join("dst/old.txt").exists(),
            "old dst contents are gone, not merged"
        );
        assert!(!root.join("src").exists(), "source consumed by the move");
    }

    #[test]
    fn shell_run_executes() {
        let root = temp_root("shell");
        let t = ShellRun::new(root);
        let out = t
            .execute(&serde_json::json!({"command":"echo hello-ng"}))
            .unwrap();
        assert!(out.contains("hello-ng"));
        assert!(out.starts_with("exit 0"));
    }

    #[test]
    fn shell_run_timeout_returns_promptly_even_with_a_surviving_grandchild() {
        // The end-to-end form of the hang: a command whose real work lives in a grandchild used to
        // time out, kill only the `cmd.exe` wrapper, and then block FOREVER in `oh.join()` because
        // the orphan still held the pipe's write end. The tool must come back on its own.
        //
        // Timing is the assertion. A 10s cap (the clamp floor) plus the 2s drain grace bounds the
        // honest worst case well under the 40s the grandchild would otherwise sleep, so a regression
        // shows up as a failure rather than a suite that never finishes.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(SHELL_TIMEOUT_ENV, "10");
        let command = if cfg!(windows) {
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            format!(
                r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe -NoProfile -Command Start-Sleep -Seconds 40"
            )
        } else {
            "sleep 40 & wait".to_string()
        };
        let t = ShellRun::new(temp_root("shell-timeout"));
        let started = Instant::now();
        let out = t.execute(&serde_json::json!({"command": command})).unwrap();
        let elapsed = started.elapsed();
        std::env::remove_var(SHELL_TIMEOUT_ENV);

        assert!(
            out.starts_with("error: command timed out"),
            "should report a timeout; got: {out}"
        );
        assert!(
            elapsed < Duration::from_secs(30),
            "shell_run took {elapsed:?} for a 10s cap — it is waiting on a pipe an orphan still holds"
        );
    }

    #[test]
    fn shell_timeout_env_is_clamped_to_a_sane_band() {
        // A cap that can be set to 0 (or to a week) is not a cap. The band keeps the agent loop
        // responsive no matter what the environment says.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::set_var(SHELL_TIMEOUT_ENV, "0");
        assert_eq!(shell_timeout_secs(), 10, "0 clamps up to the floor");
        std::env::set_var(SHELL_TIMEOUT_ENV, "999999");
        assert_eq!(
            shell_timeout_secs(),
            3600,
            "absurd values clamp to the ceiling"
        );
        std::env::set_var(SHELL_TIMEOUT_ENV, "600");
        assert_eq!(
            shell_timeout_secs(),
            600,
            "a reasonable value passes through"
        );
        std::env::set_var(SHELL_TIMEOUT_ENV, "not-a-number");
        assert_eq!(
            shell_timeout_secs(),
            SHELL_TIMEOUT_SECS,
            "garbage falls back to the default"
        );
        std::env::remove_var(SHELL_TIMEOUT_ENV);
        assert_eq!(shell_timeout_secs(), SHELL_TIMEOUT_SECS, "unset = default");
    }

    #[test]
    fn drain_keeps_output_on_invalid_utf8() {
        // Regression: `read_to_string` returns Err and leaves the buffer EMPTY on the first
        // invalid-UTF-8 byte, so non-English Windows `dir` output (OEM codepage) was dropped
        // wholesale and the agent saw a blank result. Lossy decode must keep the ASCII structure.
        let bytes = b"Directory of C:\\\nfile-\xe9\xff.txt\n2 files".to_vec();
        let got = drain(Some(std::io::Cursor::new(bytes)));
        assert!(got.contains("Directory of"), "ASCII structure preserved");
        assert!(got.contains("file-"));
        assert!(got.contains("2 files"));
        assert!(
            got.contains('\u{fffd}'),
            "bad bytes degrade to the replacement char, not loss"
        );
    }

    #[test]
    fn persona_create_writes_and_activates() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-persona-tool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("AIZEN_HOME", &dir);

        let t = PersonaCreate;
        assert!(t.is_destructive(), "writes to ~/.aizen → approval-gated");

        // default activate=true → minted AND becomes active
        let out = t
            .execute(&serde_json::json!({
                "name": "Sherlock",
                "role": "a sharp consulting detective",
                "voice": "clipped, deductive",
                "body": "You reason from observation. You are blunt but never cruel."
            }))
            .unwrap();
        assert!(out.contains("created persona 'Sherlock'"));
        assert!(out.contains("switched to it"));
        assert_eq!(
            crate::core::cli_config::load().persona.as_deref(),
            Some("Sherlock")
        );
        let p = crate::persona::load("sherlock").expect("persona card written");
        assert_eq!(p.role, "a sharp consulting detective");

        // activate=false → authored for later, active persona unchanged
        let out2 = t
            .execute(
                &serde_json::json!({"name":"Watson","body":"A loyal chronicler.","activate":false}),
            )
            .unwrap();
        assert!(out2.contains("not active"));
        assert_eq!(
            crate::core::cli_config::load().persona.as_deref(),
            Some("Sherlock"),
            "still Sherlock"
        );

        // missing required body → error (no half-formed card)
        assert!(t.execute(&serde_json::json!({"name":"Empty"})).is_err());

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_subagent_scope_gets_a_scratch_plan_but_never_the_top_level_todo() {
        // W17: a sub-agent can track its own multi-step plan (todo_write present in every scope via
        // the read-only base) — but it's the per-instance ScopedTodo, NOT the process-global
        // TodoWrite (which is top-level only, sharing the user's list + scroll region).
        let root = std::env::temp_dir();
        for role in ["coder", "tester", "planner", "reviewer", "unknown-role"] {
            let r = role_registry(role, &root);
            assert!(
                r.get("todo_write").is_some(),
                "role {role} has a scratch plan"
            );
        }
        // A specialist (empty tools = coder scope) also gets it.
        let spec = agent_registry(
            &crate::agents::AgentDef {
                name: "S".into(),
                description: String::new(),
                color: String::new(),
                emoji: String::new(),
                vibe: String::new(),
                tools: vec![],
                model: None,
                body: "b".into(),
                division: None,
                source: crate::agents::AgentSource::AizenHome,
                source_path: std::path::PathBuf::new(),
            },
            &root,
        );
        assert!(
            spec.get("todo_write").is_some(),
            "specialist has a scratch plan"
        );
        // The scratch plan is a scratch plan — writing to it never touches the process-global list
        // (proven per-instance in todo::tests; here we only assert presence + read-only classification).
        assert!(
            crate::agent::task_tool::dispatch_is_read_only(&role_registry("planner", &root)),
            "todo_write does not make a read-only role a writer (it's not workspace mutation)"
        );
    }
}
