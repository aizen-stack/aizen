//! The built-in tool surface (H3). Five orthogonal tools the agent loop advertises:
//! `memory_search` (the brain), `file_read`/`file_glob` (read-only), `file_edit`/`shell_run`
//! (destructive → approval-gated by the loop).
//!
//! Safety: every file/shell op is CONFINED to the cwd subtree (`root`) — the path-traversal
//! guard (`assertInsideWorkspace` equivalent the extension was found to be missing). The root
//! is captured at registry-build time, so tools are testable against a temp dir without
//! mutating the process-global cwd.

use crate::agent::tools::{Tool, ToolRegistry};
use anyhow::{bail, Context, Result};
use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Hard wall-clock cap for `shell_run` (a hung command must never freeze the agent loop).
const SHELL_TIMEOUT_SECS: u64 = 120;

/// `file_read` budget: a WHOLE-file read over EITHER cap returns a head+tail preview with a loud
/// marker (so the model knows it has a partial view). A range/numbered read is NEVER bounded. Small
/// files (the common case) stay byte-exact so `old_string` round-trips. `0` disables a cap.
const FILE_READ_MAX_LINES: usize = 2000;
const FILE_READ_MAX_BYTES: usize = 200_000;

/// The live top-level tool surface, published once the session's registry is built. The skills
/// index consults it to hide any skill whose `requires:` tool is absent from this build/session
/// (e.g. `browser_*` when `--features browser` is off, or MCP tools when no server is configured).
/// `None` until published → the filter is a no-op (show all), so the unit tests and the offline
/// `ng skill` path are unaffected.
static ACTIVE_TOOL_NAMES: Lazy<Mutex<Option<HashSet<String>>>> = Lazy::new(|| Mutex::new(None));

/// Publish the live tool surface (idempotent). ONLY the top-level registry calls this — never the
/// smaller `role_registry`, so the set is never wrongly shrunk when a sub-agent assembles a prompt.
fn publish_active_tools(r: &ToolRegistry) {
    *ACTIVE_TOOL_NAMES.lock().unwrap_or_else(|e| e.into_inner()) = Some(r.names().into_iter().collect());
}

/// The published live tool surface, or `None` if no session registry has been built yet.
pub fn active_tool_names() -> Option<HashSet<String>> {
    ACTIVE_TOOL_NAMES.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Resolve + canonicalize the working-directory root (the confinement boundary for file/shell).
fn resolve_root() -> Result<PathBuf> {
    std::env::current_dir()
        .context("resolving cwd")?
        .canonicalize()
        .context("canonicalizing cwd")
}

/// Build the default tool registry rooted at the current working directory (no `task` tool —
/// that needs creds; use `default_registry_with_task`). Kept for the existing tests.
#[allow(dead_code)] // test-only convenience; the binary builds registries via default_registry_in
pub fn default_registry() -> Result<ToolRegistry> {
    Ok(default_registry_in(&resolve_root()?))
}

/// The 7 built-in tools rooted at `root`. Shared by the top-level registry and the `coder`
/// sub-agent role.
fn default_registry_in(root: &Path) -> ToolRegistry {
    use crate::agent::web_tools::{WebCrawl, WebFetch, WebSearch};
    let mut r = ToolRegistry::new();
    r.register(Box::new(MemorySearch));
    r.register(Box::new(MemoryProfile));
    r.register(Box::new(MemoryAsk));
    r.register(Box::new(FileRead::new(root.to_path_buf())));
    r.register(Box::new(FileGlob::new(root.to_path_buf())));
    r.register(Box::new(crate::agent::search::SearchFiles::new(root.to_path_buf())));
    r.register(Box::new(WebSearch));
    r.register(Box::new(WebFetch));
    r.register(Box::new(WebCrawl));
    register_skill_load(&mut r);
    register_skill_registry(&mut r);
    register_telegram(&mut r);
    register_notify(&mut r);
    r.register(Box::new(crate::features::timemachine::Checkpoint));
    r.register(Box::new(FileEdit::new(root.to_path_buf())));
    r.register(Box::new(MultiEdit::new(root.to_path_buf())));
    r.register(Box::new(ShellRun::new(root.to_path_buf())));
    r.register(Box::new(SkillSave));
    // Top-level only (NOT in role sub-agents) — the in-session list + process pool are shared, so a
    // sub-agent must not clobber them. `role_registry` builds its own list and never gets these.
    r.register(Box::new(crate::agent::todo::TodoWrite));
    r.register(Box::new(crate::agent::process::Process::new(root.to_path_buf())));
    // `clarify` yields the turn back to the interactive user — meaningless inside an autonomous
    // sub-agent (no user to answer), so it stays top-level only, like todo/process.
    r.register(Box::new(crate::agent::clarify::Clarify));
    // LSP navigation + diagnostics (top-level only; default OFF). Registered ONLY once the user
    // runs `/lsp on` (the registry is rebuilt per turn, so it appears next turn). Sub-agents use
    // `subagent_read_only_base` and deliberately never get it.
    if crate::agent::lsp::LSP.is_enabled() {
        r.register(Box::new(crate::agent::lsp::tools::LspReferences::new(root.to_path_buf())));
        r.register(Box::new(crate::agent::lsp::tools::LspDefinition::new(root.to_path_buf())));
        r.register(Box::new(crate::agent::lsp::tools::LspDocumentSymbols::new(root.to_path_buf())));
        r.register(Box::new(crate::agent::lsp::tools::LspWorkspaceSymbol::new(root.to_path_buf())));
        r.register(Box::new(crate::agent::lsp::tools::LspDiagnostics::new(root.to_path_buf())));
    }
    // User-configurable MCP servers (`~/.nextgen/mcp.json`) — each remote tool wrapped as
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

/// The agentskill.sh marketplace tools (`skill_search`/`skill_install`) — ALWAYS available (their
/// whole point is finding a skill when you have none locally). Every registry.
fn register_skill_registry(r: &mut ToolRegistry) {
    r.register(Box::new(crate::skills::registry::SkillSearch));
    r.register(Box::new(crate::skills::registry::SkillInstall));
}

/// Advertise the Telegram tools only when a bot token + allowed chat are configured (otherwise
/// they'd be dead tools that just error). Available to every registry.
fn register_telegram(r: &mut ToolRegistry) {
    if crate::channels::telegram::is_configured() {
        r.register(Box::new(crate::channels::telegram::TelegramSend));
        r.register(Box::new(crate::channels::telegram::TelegramAsk));
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
    auto_approve: bool,
) -> Result<ToolRegistry> {
    let root = resolve_root()?;
    let mut r = default_registry_in(&root);
    r.register(Box::new(PersonaCreate));
    r.register(Box::new(crate::agent::task_tool::TaskTool::new(
        client, base_url, api_key, model, auto_approve, root, 0,
    )));
    // Publish the live surface so the `<skills>` index can hide skills that `require:` an absent tool.
    publish_active_tools(&r);
    Ok(r)
}

/// The read-only tool base shared by EVERY sub-agent registry (role- or specialist-scoped): memory +
/// read/glob/search + web research + skill_load/registry + telegram/notify (when configured) +
/// checkpoint. NEVER includes `task` (recursion guard), edit/shell (added per-scope by the caller),
/// or the top-level-only tools (todo/process/clarify/mcp/persona_create). Factored out so
/// `role_registry` and `agent_registry` cannot drift.
fn subagent_read_only_base(root: &Path) -> ToolRegistry {
    use crate::agent::web_tools::{WebCrawl, WebFetch, WebSearch};
    let mut r = ToolRegistry::new();
    r.register(Box::new(MemorySearch));
    r.register(Box::new(MemoryProfile));
    r.register(Box::new(MemoryAsk));
    r.register(Box::new(FileRead::new(root.to_path_buf())));
    r.register(Box::new(FileGlob::new(root.to_path_buf())));
    r.register(Box::new(crate::agent::search::SearchFiles::new(root.to_path_buf())));
    r.register(Box::new(WebSearch));
    r.register(Box::new(WebFetch));
    r.register(Box::new(WebCrawl));
    register_skill_load(&mut r);
    register_skill_registry(&mut r);
    register_telegram(&mut r);
    register_notify(&mut r);
    r.register(Box::new(crate::features::timemachine::Checkpoint));
    r
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
            r.register(Box::new(ShellRun::new(root.to_path_buf())));
            r.register(Box::new(SkillSave));
        }
        "tester" => {
            r.register(Box::new(ShellRun::new(root.to_path_buf())));
        }
        // planner / reviewer / unknown → read-only.
        _ => {}
    }
    r
}

/// Build a tool registry for a dispatched SPECIALIST agent (see [`crate::agents`]). Same read-only
/// base as [`role_registry`], plus a destructive scope derived from the persona's `tools:` frontmatter:
/// - EMPTY `tools:` → **coder scope** (file_edit + multi_edit + shell_run + skill_save) — the locked
///   default; no wider than the trusted `coder` sub-agent (the `cmd_guard` floor + per-op approval
///   still apply underneath).
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
        r.register(Box::new(ShellRun::new(root.to_path_buf())));
        r.register(Box::new(SkillSave));
        return r;
    }
    let mut granted: HashSet<&'static str> = HashSet::new();
    for raw in &def.tools {
        let Some(canon) = canonical_subagent_tool(raw) else {
            continue; // read-only (already in base), forbidden, or unknown
        };
        if !granted.insert(canon) {
            continue; // dedup
        }
        match canon {
            "file_edit" => r.register(Box::new(FileEdit::new(root.to_path_buf()))),
            "multi_edit" => r.register(Box::new(MultiEdit::new(root.to_path_buf()))),
            "shell_run" => r.register(Box::new(ShellRun::new(root.to_path_buf()))),
            "skill_save" => r.register(Box::new(SkillSave)),
            _ => unreachable!("canonical_subagent_tool only yields grantable destructive tools"),
        }
    }
    r
}

/// Map a requested tool name (a persona's `tools:` entry, possibly in Claude-Code casing) to the
/// canonical name of a GRANTABLE destructive tool, or `None`. `None` covers three cases, all meaning
/// "don't add it": read-only tools (already in the base — `Read`/`Grep`/`Glob`/…), forbidden tools
/// (`task`/`todo`/`process`/`clarify`/`persona_create`/`mcp_*`), and unknown names. This is the single
/// structural choke-point for the capability invariant: only these four strings can ever be returned,
/// so nothing else can be granted to a specialist.
fn canonical_subagent_tool(raw: &str) -> Option<&'static str> {
    match raw.trim().to_ascii_lowercase().as_str() {
        // editing
        "edit" | "write" | "file_edit" | "fileedit" | "str_replace" | "str_replace_editor" => {
            Some("file_edit")
        }
        "multiedit" | "multi_edit" => Some("multi_edit"),
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

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .with_context(|| format!("missing required string arg '{key}'"))
}

/// Resolve `path` (relative → `base`) and ensure it stays within the `base` subtree.
/// `must_exist`: canonicalize the full path; else canonicalize the parent + re-join the name.
pub(crate) fn confine(base: &Path, path: &str, must_exist: bool) -> Result<PathBuf> {
    let raw = Path::new(path);
    let joined = if raw.is_absolute() { raw.to_path_buf() } else { base.join(raw) };
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
    if !resolved.starts_with(base) {
        bail!("path escapes the working directory: {path}");
    }
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

fn walk(dir: &Path, base: &Path, re: &regex::Regex, out: &mut Vec<String>, cap: usize) {
    if out.len() >= cap {
        return;
    }
    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        if out.len() >= cap {
            return;
        }
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_string();
        let is_dir = e.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            if name.starts_with('.') || name == "target" || name == "node_modules" {
                continue;
            }
            walk(&p, base, re, out, cap);
        } else {
            let rel = p.strip_prefix(base).unwrap_or(&p).to_string_lossy().replace('\\', "/");
            if re.is_match(&rel) {
                out.push(rel);
            }
        }
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
         specific past fact. Not for the user's overall preferences → use memory_profile. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "what to recall"},
                "limit": {"type": "integer", "description": "max hits (default 5)"}
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
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5).clamp(1, 20) as usize;
        let hits = crate::memory::search(query, limit)?;
        if hits.is_empty() {
            return Ok(format!("(no memory matches '{query}')"));
        }
        let mut s = String::new();
        for h in &hits {
            let body: String = h.entry.body.chars().take(200).collect();
            s.push_str(&format!(
                "[{:.2}] {} ({}) — {}\n",
                h.score,
                h.entry.name,
                h.entry.mtype.as_str(),
                body.replace('\n', " ")
            ));
        }
        Ok(s.trim_end().to_string())
    }
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
         default) when you'll feed the text back into file_edit's old_string. Confined to the \
         working directory. Read-only."
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
        let number = args.get("number").and_then(|v| v.as_bool()).unwrap_or(false);
        // The whole file — verbatim under budget (the common case; keeps old_string round-trips
        // byte-exact), or a clearly-marked head+tail preview when it's pathologically large.
        if start.is_none() && end.is_none() && !number {
            return Ok(budget_view(&content, path, FILE_READ_MAX_LINES, FILE_READ_MAX_BYTES));
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
    let bcap = if max_bytes > 0 { (max_bytes / 2).max(1) } else { usize::MAX };
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
        "List files matching a glob (*, **, ?) under the working directory. Use to locate files \
         before reading. Not for searching file CONTENT → use search_files. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {"pattern": {"type": "string", "description": "e.g. src/**/*.rs"}},
            "required": ["pattern"],
            "additionalProperties": false
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let pattern = str_arg(args, "pattern")?;
        let re = regex::Regex::new(&glob_to_regex(pattern)).context("invalid glob pattern")?;
        let mut out = Vec::new();
        walk(&self.root, &self.root, &re, &mut out, 200);
        if out.is_empty() {
            return Ok(format!("(no files match '{pattern}')"));
        }
        Ok(out.join("\n"))
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
         block. Returns a before→after preview. Read the file first. Confined to the working directory."
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
    fn execute(&self, args: &Value) -> Result<String> {
        let path = str_arg(args, "path")?;
        let new = str_arg(args, "new_string")?;
        let old = args.get("old_string").and_then(|v| v.as_str()).unwrap_or("");
        let replace_all = args.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);

        if old.is_empty() {
            // create-new path
            let target = confine(&self.root, path, false)?;
            if target.exists() {
                bail!("{path} exists; provide old_string to edit it");
            }
            std::fs::write(&target, new).with_context(|| format!("creating {}", target.display()))?;
            return Ok(format!("created {path}"));
        }

        let target = confine(&self.root, path, true)?;
        let content = std::fs::read_to_string(&target)
            .with_context(|| format!("reading {}", target.display()))?;
        let applied = apply_one_edit(&content, old, new, replace_all, path)?;
        std::fs::write(&target, &applied.content)
            .with_context(|| format!("writing {}", target.display()))?;
        Ok(format!("edited {path} ({})\n{}", applied.summary(), diff_preview(&applied.before, &applied.after)))
    }
}

/// The outcome of ONE exact-or-indent-tolerant replacement (pure; computed in memory). `before`/
/// `after` feed the diff preview; `count`/`tolerant` feed the human summary.
struct EditApplied {
    content: String,
    before: String,
    after: String,
    count: usize,
    tolerant: bool,
}
impl EditApplied {
    /// Human summary, byte-identical to the original `file_edit` wording (regression-gated).
    fn summary(&self) -> String {
        if self.tolerant {
            "1 replacement, indentation-tolerant match".to_string()
        } else {
            format!("{} replacement(s)", self.count)
        }
    }
}

/// Apply ONE replacement to `content` — the shared matcher behind both `file_edit` and
/// `multi_edit` (pure, no IO). Mirrors the original `file_edit` decision tree exactly:
///   exact count 0  → indent_tolerant_blocks: 1 ⇒ apply, 0 ⇒ Err(not found), >1 ⇒ Err(ambiguous)
///   exact count >1 → Err(not unique) unless replace_all
///   exact count 1  → replacen / (replace_all ⇒ replace)
/// `old` MUST be non-empty (create-new is the caller's concern). `label` names the target in
/// errors (a path for file_edit; "edit #N (path)" for multi_edit).
fn apply_one_edit(content: &str, old: &str, new: &str, replace_all: bool, label: &str) -> Result<EditApplied> {
    if old.is_empty() {
        bail!("empty old_string is only valid for creating a new file (file_edit), not mid-edit");
    }
    let count = content.matches(old).count();
    if count == 0 {
        // Exact miss → ONE whitespace/indentation-tolerant block match is safe to apply (the #1 real
        // failure: the model gets leading indentation slightly wrong). >1 tolerant match is ambiguous
        // and we refuse rather than risk corrupting the file.
        let ranges = indent_tolerant_blocks(content, old);
        match ranges.len() {
            0 => bail!("old_string not found in {label} (even ignoring indentation) — re-read the file; it may have changed"),
            1 => {
                let (bs, be) = ranges[0];
                let before = content[bs..be].to_string();
                let updated = format!("{}{}{}", &content[..bs], new, &content[be..]);
                Ok(EditApplied { content: updated, before, after: new.to_string(), count: 1, tolerant: true })
            }
            n => bail!("old_string (ignoring indentation) matches {n} blocks in {label}; add more surrounding context to disambiguate"),
        }
    } else {
        if count > 1 && !replace_all {
            bail!("old_string is not unique in {label} ({count} matches); add context or set replace_all");
        }
        let updated = if replace_all { content.replace(old, new) } else { content.replacen(old, new, 1) };
        Ok(EditApplied { content: updated, before: old.to_string(), after: new.to_string(), count, tolerant: false })
    }
}

/// Find full-line blocks in `content` that match `old` IGNORING per-line leading/trailing
/// whitespace. Returns the byte range [start, end) of each matching block (line-aligned: from the
/// first matched line's start through the last matched line's end, excluding the trailing newline).
/// Used only as a fallback when exact matching fails.
fn indent_tolerant_blocks(content: &str, old: &str) -> Vec<(usize, usize)> {
    let old_norm: Vec<&str> = old.lines().map(|l| l.trim()).collect();
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

    let norm: Vec<&str> = spans.iter().map(|&(a, b)| content[a..b].trim()).collect();
    let mut out = Vec::new();
    if norm.len() < k {
        return out;
    }
    for i in 0..=(norm.len() - k) {
        if (0..k).all(|j| norm[i + j] == old_norm[j]) {
            out.push((spans[i].0, spans[i + k - 1].1));
        }
    }
    out
}

/// A compact `before → after` preview of an edit (each side clipped so a big block can't flood
/// the result). Gives the model cheap verifiability of what actually changed.
fn diff_preview(before: &str, after: &str) -> String {
    format!("--- before\n{}\n+++ after\n{}", clip_block(before), clip_block(after))
}

fn clip_block(s: &str) -> String {
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= 12 {
        return s.trim_end_matches('\n').to_string();
    }
    let head = lines[..6].join("\n");
    let tail = lines[lines.len() - 4..].join("\n");
    format!("{head}\n…[{} more lines]…\n{tail}", lines.len() - 10)
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
         indentation-tolerant retry). For a SINGLE edit use file_edit. Read the file first. Confined \
         to the working directory."
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
    fn execute(&self, args: &Value) -> Result<String> {
        let path = str_arg(args, "path")?;
        let edits = args
            .get("edits")
            .and_then(|v| v.as_array())
            .filter(|a| !a.is_empty())
            .context("multi_edit requires a non-empty 'edits' array")?;
        let target = confine(&self.root, path, true)?;
        let original = std::fs::read_to_string(&target)
            .with_context(|| format!("reading {}", target.display()))?;

        // Compute the whole result in memory; write ONCE at the end. Any edit error returns before
        // the write is reached → atomic ("nothing written"), no temp file / rollback needed. Each
        // edit re-searches the EVOLVING buffer, so offsets can never go stale (string search, not
        // byte offsets) and a later edit may target text an earlier one produced.
        let mut buf = original.clone();
        let mut summaries: Vec<String> = Vec::with_capacity(edits.len());
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
            let replace_all = e.get("replace_all").and_then(|v| v.as_bool()).unwrap_or(false);
            let applied = apply_one_edit(&buf, old, new, replace_all, &format!("edit #{n} ({path})"))?;
            let detail = if applied.tolerant {
                "1 replacement, indentation-tolerant".to_string()
            } else if replace_all {
                format!("{} replacement(s), replace_all", applied.count)
            } else {
                format!("{} replacement(s)", applied.count)
            };
            summaries.push(format!("  #{n}: {detail}"));
            buf = applied.content;
        }

        std::fs::write(&target, &buf).with_context(|| format!("writing {}", target.display()))?;
        Ok(format!(
            "edited {path} ({} edits applied)\n{}\n{}",
            edits.len(),
            summaries.join("\n"),
            diff_preview(&original, &buf)
        ))
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
         grep here); for long-running commands (dev servers, watchers) use the process tool — this \
         has a 120s timeout. Destructive — the user is asked to confirm."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "cwd": {"type": "string", "description": "optional subdir of the working dir"}
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
        cmd.current_dir(&dir).stdout(Stdio::piped()).stderr(Stdio::piped());
        let mut child = cmd.spawn().with_context(|| format!("spawning shell for `{command}`"))?;

        // Drain the pipes on threads so a chatty command can't deadlock on a full buffer,
        // while we poll with a wall-clock timeout (kill the child if it overruns).
        let out_pipe = child.stdout.take();
        let err_pipe = child.stderr.take();
        let oh = std::thread::spawn(move || drain(out_pipe));
        let eh = std::thread::spawn(move || drain(err_pipe));

        let timeout = Duration::from_secs(SHELL_TIMEOUT_SECS);
        let start = Instant::now();
        let mut cancelled = false;
        let status = loop {
            match child.try_wait()? {
                Some(st) => break Some(st),
                None => {
                    // User pressed Esc (cooperative cancel) — kill the child now instead of blocking
                    // the whole turn up to the timeout. This is what makes Esc responsive during a
                    // long command (the confirmed "can't cancel while a tool runs" bug).
                    if crate::ui::tui::cancel_requested() {
                        let _ = child.kill();
                        let _ = child.wait();
                        cancelled = true;
                        break None;
                    }
                    if start.elapsed() >= timeout {
                        let _ = child.kill();
                        let _ = child.wait();
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(40));
                }
            }
        };
        let stdout = oh.join().unwrap_or_default();
        let stderr = eh.join().unwrap_or_default();

        match status {
            None if cancelled => Ok("error: command cancelled by the user (Esc)".to_string()),
            None => {
                // Surface stderr too (the success branch already does) so a killed build's diagnostics
                // aren't lost to the model — they're often the most useful part of a timeout.
                let mut s = format!("error: command timed out after {SHELL_TIMEOUT_SECS}s (killed)\n{stdout}");
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
            Some(sk) => Ok(crate::skills::render_loaded(&sk)),
            None => {
                let avail: Vec<String> = crate::skills::list().into_iter().map(|s| s.name).collect();
                Ok(format!("(no skill named '{name}'; available: {})", avail.join(", ")))
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
                "body": {"type": "string", "description": "the steps / procedure (markdown)"}
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
        let description = args.get("description").and_then(|v| v.as_str()).unwrap_or("");
        let when = args.get("when").and_then(|v| v.as_str()).unwrap_or("");
        let path = crate::skills::save(name, description, when, body)?;
        Ok(format!("saved skill '{name}' → {}", path.display()))
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
        let activate = args.get("activate").and_then(|v| v.as_bool()).unwrap_or(true);
        let path = crate::persona::save(name, role, voice, body)?;
        if activate {
            crate::persona::set_active(name)?;
            Ok(format!(
                "created persona '{name}' → {} and switched to it (takes full effect from the \
                 user's next message).",
                path.display()
            ))
        } else {
            Ok(format!("created persona '{name}' → {} (not active; switch with /persona).", path.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("ng-agent-tool-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
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
    fn confine_rejects_escape() {
        let root = temp_root("confine");
        assert!(confine(&root, "../etc/passwd", false).is_err());
        // a normal in-tree path resolves
        std::fs::write(root.join("ok.txt"), "hi").unwrap();
        assert!(confine(&root, "ok.txt", true).is_ok());
    }

    #[test]
    fn file_read_reads_and_ranges() {
        let root = temp_root("read");
        std::fs::write(root.join("f.txt"), "l1\nl2\nl3\nl4").unwrap();
        let t = FileRead::new(root.clone());
        let all = t.execute(&serde_json::json!({"path":"f.txt"})).unwrap();
        assert_eq!(all, "l1\nl2\nl3\nl4");
        let mid = t.execute(&serde_json::json!({"path":"f.txt","start":2,"end":3})).unwrap();
        assert_eq!(mid, "l2\nl3");
    }

    #[test]
    fn file_read_rejects_escape() {
        let root = temp_root("read-escape");
        let t = FileRead::new(root);
        assert!(t.execute(&serde_json::json!({"path":"../../secret"})).is_err());
    }

    #[test]
    fn file_glob_lists_matches() {
        let root = temp_root("glob");
        std::fs::create_dir_all(root.join("src/sub")).unwrap();
        std::fs::write(root.join("src/a.rs"), "").unwrap();
        std::fs::write(root.join("src/sub/b.rs"), "").unwrap();
        std::fs::write(root.join("src/c.ts"), "").unwrap();
        let t = FileGlob::new(root);
        let out = t.execute(&serde_json::json!({"pattern":"src/**/*.rs"})).unwrap();
        assert!(out.contains("src/a.rs"));
        assert!(out.contains("src/sub/b.rs"));
        assert!(!out.contains("c.ts"));
    }

    #[test]
    fn file_edit_replaces_uniquely() {
        let root = temp_root("edit");
        std::fs::write(root.join("f.txt"), "hello world").unwrap();
        let t = FileEdit::new(root.clone());
        let r = t.execute(&serde_json::json!({"path":"f.txt","old_string":"world","new_string":"rust"})).unwrap();
        assert!(r.contains("edited"));
        assert_eq!(std::fs::read_to_string(root.join("f.txt")).unwrap(), "hello rust");
    }

    #[test]
    fn file_read_number_prefixes_lines() {
        let root = temp_root("read-num");
        std::fs::write(root.join("f.txt"), "alpha\nbeta\ngamma").unwrap();
        let t = FileRead::new(root);
        let out = t.execute(&serde_json::json!({"path":"f.txt","number":true})).unwrap();
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
        assert!(out.contains("over the 4-line read budget"), "loud marker: {out}");
        assert!(out.contains("L1\r\nL2\r\n"), "head slice is byte-exact incl CRLF");
        assert!(out.contains("L5"), "tail slice present");
        assert!(!out.contains("L3"), "the omitted middle is not shown");
        assert!(out.contains("lines omitted: 3-4"), "names the omitted range: {out}");
    }

    #[test]
    fn budget_view_over_bytes_one_long_line() {
        let c = "x".repeat(5000);
        let out = budget_view(&c, "min.js", 0, 2048); // byte cap only; one giant line
        assert!(out.starts_with("[file_read:"), "marker first");
        assert!(out.contains("KB read budget"));
        assert!(out.contains("bytes omitted"));
        assert!(out.len() < c.len(), "result is bounded below the original size");
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
        let big: String = (1..=2500).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        std::fs::write(root.join("big.txt"), &big).unwrap();
        let t = FileRead::new(root);
        let out = t.execute(&serde_json::json!({"path":"big.txt"})).unwrap();
        assert!(out.contains("over the 2000-line read budget"), "marker present");
        assert!(out.contains("line1\n"), "head present");
        assert!(out.contains("line2500"), "tail present");
        assert!(!out.contains("line1300\n"), "the omitted middle is not shown");
    }

    #[test]
    fn file_read_explicit_range_skips_budget() {
        // An explicit start/end on the SAME large file returns the exact range — never re-bounded.
        let root = temp_root("read-budget-range");
        let big: String = (1..=2500).map(|i| format!("line{i}")).collect::<Vec<_>>().join("\n");
        std::fs::write(root.join("big.txt"), &big).unwrap();
        let t = FileRead::new(root);
        let out = t.execute(&serde_json::json!({"path":"big.txt","start":5,"end":7})).unwrap();
        assert_eq!(out, "line5\nline6\nline7");
        assert!(!out.contains("read budget"), "range reads are never budgeted");
    }

    #[test]
    fn file_edit_indentation_tolerant_fallback() {
        let root = temp_root("edit-indent");
        // File uses 4-space indentation; the model's old_string uses 2 spaces (a real mismatch).
        std::fs::write(root.join("f.rs"), "fn main() {\n    let x = 1;\n    foo();\n}\n").unwrap();
        let t = FileEdit::new(root.clone());
        let r = t
            .execute(&serde_json::json!({
                "path": "f.rs",
                "old_string": "  let x = 1;\n  foo();",      // wrong indentation, right content
                "new_string": "    let x = 2;\n    bar();"
            }))
            .unwrap();
        assert!(r.contains("indentation-tolerant"), "got: {r}");
        assert!(r.contains("before"), "should include a preview");
        let after = std::fs::read_to_string(root.join("f.rs")).unwrap();
        assert_eq!(after, "fn main() {\n    let x = 2;\n    bar();\n}\n");
    }

    #[test]
    fn file_edit_ambiguous_tolerant_match_refuses() {
        let root = temp_root("edit-indent-dup");
        std::fs::write(root.join("f.txt"), "  a\nb\n    a\nb\n").unwrap();
        let t = FileEdit::new(root);
        // "a\nb" matches two blocks once indentation is ignored → refuse, don't corrupt.
        let r = t.execute(&serde_json::json!({"path":"f.txt","old_string":"a\nb","new_string":"X"}));
        assert!(r.is_err(), "ambiguous tolerant match must refuse");
    }

    #[test]
    fn file_edit_rejects_nonunique_without_replace_all() {
        let root = temp_root("edit-dup");
        std::fs::write(root.join("f.txt"), "a a a").unwrap();
        let t = FileEdit::new(root);
        assert!(t.execute(&serde_json::json!({"path":"f.txt","old_string":"a","new_string":"b"})).is_err());
    }

    #[test]
    fn file_edit_creates_new_when_old_empty() {
        let root = temp_root("edit-new");
        let t = FileEdit::new(root.clone());
        let r = t.execute(&serde_json::json!({"path":"new.txt","old_string":"","new_string":"content"})).unwrap();
        assert!(r.contains("created"));
        assert_eq!(std::fs::read_to_string(root.join("new.txt")).unwrap(), "content");
    }

    #[test]
    fn file_edit_is_destructive() {
        assert!(FileEdit::new(PathBuf::from(".")).is_destructive());
        assert!(ShellRun::new(PathBuf::from(".")).is_destructive());
        assert!(!FileRead::new(PathBuf::from(".")).is_destructive());
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
        assert_eq!(std::fs::read_to_string(root.join("f.txt")).unwrap(), "A beta G");
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
        assert_eq!(std::fs::read_to_string(root.join("f.txt")).unwrap(), "three");
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
        assert!(err.contains("edit #2"), "error names the failing edit index: {err}");
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
        assert_eq!(std::fs::read_to_string(root.join("f.txt")).unwrap(), "Z Z Z | Y");
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
        std::fs::write(root.join("f.rs"), "fn main() {\n    let x = 1;\n    foo();\n}\n").unwrap();
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
        assert!(t.execute(&serde_json::json!({"path": "f.txt", "edits": []})).is_err());
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
        assert_eq!(std::fs::read_to_string(root.join("f.txt")).unwrap(), "hi", "nothing written");
    }

    #[test]
    fn shell_run_executes() {
        let root = temp_root("shell");
        let t = ShellRun::new(root);
        let out = t.execute(&serde_json::json!({"command":"echo hello-ng"})).unwrap();
        assert!(out.contains("hello-ng"));
        assert!(out.starts_with("exit 0"));
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
        assert!(got.contains('\u{fffd}'), "bad bytes degrade to the replacement char, not loss");
    }

    #[test]
    fn persona_create_writes_and_activates() {
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-persona-tool-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);

        let t = PersonaCreate;
        assert!(t.is_destructive(), "writes to ~/.nextgen → approval-gated");

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
        assert_eq!(crate::core::cli_config::load().persona.as_deref(), Some("Sherlock"));
        let p = crate::persona::load("sherlock").expect("persona card written");
        assert_eq!(p.role, "a sharp consulting detective");

        // activate=false → authored for later, active persona unchanged
        let out2 = t
            .execute(&serde_json::json!({"name":"Watson","body":"A loyal chronicler.","activate":false}))
            .unwrap();
        assert!(out2.contains("not active"));
        assert_eq!(crate::core::cli_config::load().persona.as_deref(), Some("Sherlock"), "still Sherlock");

        // missing required body → error (no half-formed card)
        assert!(t.execute(&serde_json::json!({"name":"Empty"})).is_err());

        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
