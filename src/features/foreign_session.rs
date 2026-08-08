//! Import conversations recorded by OTHER CLIs (Claude Code, Codex) and resume them inside aizen.
//!
//! Each CLI keeps its own append-only JSONL transcript on disk. None of them share a schema, and
//! none of them match aizen's `Vec<Message>` wire shape — but they all record the same *thing* (a
//! turn-by-turn chat with tool calls + results), so a line-by-line reader can rebuild the spine and
//! hand it to the same thread-switch path `/resume` uses.
//!
//! The hard part is `assert_valid_history` (`src/agent/mod.rs`): every `tool` result must be
//! preceded by an assistant `tool_call` with the same id, and every open `tool_call` must be closed
//! before the next assistant turn. A foreign transcript that was compacted mid-call, or whose tail
//! was trimmed by the source CLI, violates that invariant — and a provider returns 400 the moment
//! it sees an orphan. [`repair_history`] closes both gaps so the imported transcript is always
//! sendable, no matter how the source CLI left it.
//!
//! What is deliberately NOT carried over:
//! - `thinking` / `reasoning` blocks. aizen strips chain-of-thought from its OWN model's output
//!   (`reasoning_content` is read only to be suppressed from display, and `<think>` tags are
//!   filtered out of `content`). Keeping them here would fill the context with the one thing
//!   aizen always discards, and there is no field on `Message` that holds them honestly.
//! - `system` / `developer` turns. The source CLI's harness prompt is not aizen's; leaving it in
//!   would be grafted onto the dynamic lane and then immediately replaced by
//!   `splice_prompt_lanes`, or worse — read as the stable lane and replayed verbatim. The current
//!   project's own prompt bundle is seeded fresh on import, exactly as on `/resume`.
//! - Sidechain / sub-agent transcripts (Claude `isSidechain:true`) and Codex `event_msg` /
//!   `turn_context` bookkeeping lines.

use crate::core::types::{FunctionCall, Message, ToolCall};

/// Which foreign CLI a transcript came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cli {
    Claude,
    Codex,
}

impl Cli {
    fn tag(self) -> &'static str {
        match self {
            Cli::Claude => "claude",
            Cli::Codex => "codex",
        }
    }
}

/// One discoverable foreign transcript, with just enough metadata for the picker — the transcript
/// itself is parsed lazily on selection so a 58 MB file never has to be read to list it.
#[derive(Debug, Clone)]
pub struct ForeignSession {
    pub cli: Cli,
    pub path: std::path::PathBuf,
    pub cwd: String,
    pub mtime_ms: Option<u64>,
    pub turns: usize,
    /// First human-typed prompt in the file — the row's subject line — truncated for one row.
    /// Harness envelope (command caveats, `<environment_context>`, hook output) is skipped rather
    /// than captioned, so this is empty only when the transcript genuinely has no human turn.
    pub first_prompt: String,
}

impl ForeignSession {
    /// Width of the fixed left gutter: `{age:>3}  {tag:<6}` + one trailing space.
    const GUTTER: usize = 3 + 2 + 6 + 1;

    /// Picker row: a narrow fixed gutter, then the subject.
    ///
    /// ```text
    ///  1m   claude  commit đi
    /// 58m   claude  clone repos này về
    /// 62d   codex   update the chunk boundaries
    /// ```
    ///
    /// `max_width` is the terminal width. Every row is CLIPPED to it, which is the difference between
    /// a list you can read and one you can't: dialoguer wraps an over-long item onto a second line, so
    /// one long subject shifts every row below it and the column alignment — the thing that makes 240
    /// rows scannable — collapses for the whole page.
    ///
    /// The gutter is padded so the subjects start at the same column and the eye can run straight
    /// down them. Ages are compact for the same reason: `19 hour(s) ago` and `1 min ago` differ by six
    /// characters, and that difference lands in front of the only column that matters.
    pub fn row(&self, age: impl Fn(Option<u64>) -> String, max_width: usize) -> String {
        let subject = if self.first_prompt.is_empty() {
            "(no subject)"
        } else {
            &self.first_prompt
        };
        let mut tail = subject.to_string();
        // A subdir launch is worth naming; `discover` blanks cwd when it IS the project root, so this
        // marks the genuinely unusual row instead of every row.
        if !self.cwd.is_empty() {
            if let Some(leaf) = std::path::Path::new(&self.cwd)
                .file_name()
                .and_then(|n| n.to_str())
            {
                tail.push_str(&format!("  ({leaf})"));
            }
        }
        let room = max_width.saturating_sub(Self::GUTTER);
        if room >= 8 && tail.chars().count() > room {
            tail = tail
                .chars()
                .take(room.saturating_sub(1))
                .collect::<String>()
                + "…";
        }
        format!("{:>3}  {:<6} {}", age(self.mtime_ms), self.cli.tag(), tail)
    }
}

/// Resolve the foreign CLIs' home roots. `AIZEN_FOREIGN_HOME` overrides BOTH (how tests pin a temp
/// tree); otherwise the user's home (`USERPROFILE`/`HOME`) — the same base `config::aizen_home`
/// derives `.aizen` from, so a redirected home redirects the foreign roots too.
fn foreign_roots() -> [std::path::PathBuf; 2] {
    if let Ok(v) = std::env::var("AIZEN_FOREIGN_HOME") {
        let v = v.trim();
        if !v.is_empty() {
            let base = std::path::PathBuf::from(v);
            return [base.join(".claude"), base.join(".codex")];
        }
    }
    let home = std::env::var("USERPROFILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("HOME").ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| ".".to_string());
    let base = std::path::PathBuf::from(home);
    [base.join(".claude"), base.join(".codex")]
}

/// Canonicalize a path for project matching WITHOUT requiring it to exist. Claude/Codex record the
/// cwd they were launched from verbatim — `C:\…` vs `c:\…`, with or without a trailing separator —
/// so two records of the SAME checkout compare unequal under a naive string match. Canonicalize
/// when the dir exists (collapses `\\?\`, casing on case-insensitive FSes), and fall back to a
/// manual lowercase + separator-normalize when it doesn't (a moved checkout still deserves to be
/// matched against its old recorded cwd).
fn match_key(p: &std::path::Path) -> String {
    if let Ok(c) = std::fs::canonicalize(p) {
        return norm_key(&c);
    }
    norm_key(p)
}

/// Lowercase + forward-slash + strip trailing slash + strip a `\\?\` verbatim prefix. The final
/// key is comparison-only; it is never shown and never written back.
fn norm_key(p: &std::path::Path) -> String {
    let mut s = p.to_string_lossy().to_string();
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        s = rest.to_string();
    }
    s = s.replace('\\', "/");
    while s.ends_with('/') {
        s.pop();
    }
    s.to_lowercase()
}

/// Does `recorded` cwd belong to `project_root`? True when it IS the root or lives UNDER it —
/// launching the foreign CLI from a SUBDIR of the repo still counts as "this project".
///
/// The reverse direction — a recorded cwd that is an ANCESTOR of the root — looks symmetric but is
/// deliberately NOT a match. `config::project_root()` already resolves to the checkout's toplevel
/// (git `--show-toplevel`, or the OUTERMOST VCS/manifest marker when git is unusable), so a cwd
/// shallower than it lies outside the project boundary by construction; the only dirs it can name
/// are marker-less parents. Honoring it made every transcript ever recorded in such a parent match
/// every project beneath it — on this developer's machine a `~` launch matched all eight — which is
/// the same collapse-everything-into-one-zone failure `project_root`'s home-bound walk exists to
/// prevent. And the cost is not just a noisy picker: importing an over-matched transcript stamps
/// THIS project's provenance onto a conversation from another one, and that stamp cannot be undone.
fn cwd_matches(recorded: &str, project_root: &std::path::Path) -> bool {
    let rec = norm_key(std::path::Path::new(recorded));
    let proj = match_key(project_root);
    rec == proj || rec.starts_with(&format!("{proj}/"))
}

/// Scan both foreign roots for transcripts whose recorded cwd belongs to `project_root`, newest
/// first. Unreadable / non-JSONL files are skipped silently — a corrupt foreign file is not ours to
/// diagnose, and surfacing it would drown the picker in noise from every project the user ever
/// opened in another CLI.
pub fn discover(project_root: &std::path::Path) -> Vec<ForeignSession> {
    let mut out = Vec::new();
    let [claude_root, codex_root] = foreign_roots();
    discover_claude(&claude_root, project_root, &mut out);
    discover_codex(&codex_root, project_root, &mut out);

    let now_ms = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let proj = match_key(project_root);
    for s in &mut out {
        // Clamp future stamps (clock skew from a VM resume) so one skewed file can't pin the top.
        s.mtime_ms = s.mtime_ms.map(|ms| ms.min(now_ms));
        // A cwd that IS the project root tells the user nothing — every row would carry the same
        // note. Blank it here (one comparison, once) so `row` can print it unconditionally and only
        // a genuine subdir launch shows up as one.
        if norm_key(std::path::Path::new(&s.cwd)) == proj {
            s.cwd.clear();
        }
    }
    // Newest first; ties break by path so the sort is total and stable.
    out.sort_by(|a, b| {
        b.mtime_ms
            .cmp(&a.mtime_ms)
            .then_with(|| a.path.cmp(&b.path))
    });
    out
}

fn discover_claude(
    root: &std::path::Path,
    project_root: &std::path::Path,
    out: &mut Vec<ForeignSession>,
) {
    let projects = match std::fs::read_dir(root.join("projects")) {
        Ok(rd) => rd,
        Err(_) => return, // Claude CLI not used on this machine — fine.
    };
    for dir in projects.flatten() {
        let dir_path = dir.path();
        if !dir_path.is_dir() {
            continue;
        }
        let entries = match std::fs::read_dir(&dir_path) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
                continue;
            }
            if let Some(s) = read_claude_row(&path, project_root) {
                out.push(s);
            }
        }
    }
}

/// Read a Claude CLI transcript just enough to decide + describe it: the recorded cwd (any line
/// carries it), the first human prompt, and a turn count. Returns `None` when the file's cwd never
/// matches `project_root` or the file is unreadable. The full parse happens later, on selection.
fn read_claude_row(
    path: &std::path::Path,
    project_root: &std::path::Path,
) -> Option<ForeignSession> {
    let bytes = std::fs::read(path).ok()?;
    let mut cwd: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut turns = 0usize;
    let mut matched = false;
    for line in bytes.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_slice(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        // Sidechain transcripts are a sub-agent's internal log, not the user's conversation.
        if v.get("isSidechain").and_then(|b| b.as_bool()) == Some(true) {
            continue;
        }
        if first_prompt.is_none() && v.get("type").and_then(|t| t.as_str()) == Some("user") {
            // Keep scanning past envelope-only turns: `/compact`, hook output, and command caveats
            // all arrive as `user` lines, so the FIRST one is routinely not a prompt at all.
            if let Some(text) = claude_human_text(&v).as_deref().and_then(prompt_subject) {
                first_prompt = Some(truncate(&text, 60));
            }
        }
        if let Some(c) = v.get("cwd").and_then(|c| c.as_str()) {
            if cwd.is_none() {
                cwd = Some(c.to_string());
            }
            if !matched && cwd_matches(c, project_root) {
                matched = true;
            }
        }
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if ty == "user" || ty == "assistant" {
            // Count a turn only when it carries real content — a user line that is purely a
            // tool_result envelope is bookkeeping, not a conversational turn.
            if ty == "assistant" || claude_human_text(&v).is_some() {
                turns += 1;
            }
        }
    }
    if !matched {
        return None;
    }
    Some(ForeignSession {
        cli: Cli::Claude,
        path: path.to_path_buf(),
        cwd: cwd.unwrap_or_default(),
        mtime_ms: file_mtime_ms(path),
        turns,
        first_prompt: first_prompt.unwrap_or_default(),
    })
}

/// Pull the human-typed text out of a Claude `user` line. Claude serializes `message.content` as
/// either a plain string (typed prompt) or a parts array (text + tool_result + attachments). A
/// user line whose only part is a `tool_result` is a tool envelope, not a prompt → `None`.
fn claude_human_text(v: &serde_json::Value) -> Option<String> {
    let msg = v.get("message")?;
    let content = msg.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        for part in arr {
            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    return Some(t.to_string());
                }
            }
        }
    }
    None
}

fn discover_codex(
    root: &std::path::Path,
    project_root: &std::path::Path,
    out: &mut Vec<ForeignSession>,
) {
    let sessions = match std::fs::read_dir(root.join("sessions")) {
        Ok(rd) => rd,
        Err(_) => return,
    };
    // Codex nests by date: sessions/YYYY/MM/DD/rollout-*.jsonl. Walk the whole tree.
    let mut files = Vec::new();
    collect_jsonl(root.join("sessions"), &mut files);
    let _ = sessions; // read_dir above was just a presence check; collect_jsonl does the real walk.
    for path in files {
        if let Some(s) = read_codex_row(&path, project_root) {
            out.push(s);
        }
    }
}

fn collect_jsonl(dir: std::path::PathBuf, out: &mut Vec<std::path::PathBuf>) {
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return;
    };
    for e in rd.flatten() {
        let path = e.path();
        if path.is_dir() {
            collect_jsonl(path, out);
        } else if path.extension().and_then(|x| x.to_str()) == Some("jsonl") {
            out.push(path);
        }
    }
}

/// Read a Codex transcript's header + first prompt. Codex writes `cwd` once, in the
/// `session_meta` line near the top, so this stops early once it has the meta + a prompt (or hits
/// the byte cap below) — it never reads a 58 MB file to the end just to list it.
fn read_codex_row(
    path: &std::path::Path,
    project_root: &std::path::Path,
) -> Option<ForeignSession> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(f);
    let mut cwd: Option<String> = None;
    let mut first_prompt: Option<String> = None;
    let mut turns = 0usize;
    let mut bytes_read: usize = 0;
    let mut buf = String::new();
    // Cap the listing scan: the meta + first user prompt live in the first few KB. Reading further
    // only inflates the turn count, which the picker shows rounded — not worth parsing megabytes.
    const SCAN_CAP: usize = 64 * 1024;
    while bytes_read < SCAN_CAP {
        buf.clear();
        let n = match reader.read_line(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };
        bytes_read += n;
        let line = buf.trim_end();
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) == Some("session_meta") {
            if let Some(c) = v
                .get("payload")
                .and_then(|p| p.get("cwd"))
                .and_then(|c| c.as_str())
            {
                cwd = Some(c.to_string());
            }
        }
        // A Codex user message is a response_item with payload.type "message" + role "user".
        if first_prompt.is_none()
            && v.get("type").and_then(|t| t.as_str()) == Some("response_item")
            && v.get("payload")
                .and_then(|p| p.get("type"))
                .and_then(|t| t.as_str())
                == Some("message")
            && v.get("payload")
                .and_then(|p| p.get("role"))
                .and_then(|r| r.as_str())
                == Some("user")
        {
            // Codex leads every session with an `<environment_context>` user item, so the first
            // user message is ALWAYS envelope — skip past it to the real prompt.
            if let Some(text) = codex_message_text(v.get("payload").unwrap_or(&v))
                .as_deref()
                .and_then(prompt_subject)
            {
                first_prompt = Some(truncate(&text, 60));
            }
        }
        // Turn count: assistant OR user message parts (tool calls are separate response_items).
        if v.get("type").and_then(|t| t.as_str()) == Some("response_item") {
            let pty = v
                .get("payload")
                .and_then(|p| p.get("type"))
                .and_then(|t| t.as_str());
            if pty == Some("message") {
                let role = v
                    .get("payload")
                    .and_then(|p| p.get("role"))
                    .and_then(|r| r.as_str());
                if role == Some("user") || role == Some("assistant") {
                    turns += 1;
                }
            }
        }
    }
    let matched = cwd.as_deref().is_some_and(|c| cwd_matches(c, project_root));
    if !matched {
        return None;
    }
    Some(ForeignSession {
        cli: Cli::Codex,
        path: path.to_path_buf(),
        cwd: cwd.unwrap_or_default(),
        mtime_ms: file_mtime_ms(path),
        turns,
        first_prompt: first_prompt.unwrap_or_default(),
    })
}

/// Concatenate the `input_text`/`text` parts of a Codex `message` payload into one string.
fn codex_message_text(payload: &serde_json::Value) -> Option<String> {
    let content = payload.get("content")?.as_array()?;
    let mut out = String::new();
    for part in content {
        let ty = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let txt = if ty == "input_text" || ty == "text" || ty == "output_text" {
            part.get("text").and_then(|t| t.as_str())
        } else {
            None
        };
        if let Some(t) = txt {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(t);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Tags the foreign CLIs wrap around text THEY injected into a user turn. None of it was typed by
/// the person, so none of it is a subject line: a picker full of
/// `<local-command-caveat>Caveat: The messages below were gener…` tells you nothing about which
/// conversation you're looking at, and every row looks identical.
const ENVELOPE_TAGS: &[&str] = &[
    "local-command-caveat",
    "local-command-stdout",
    "local-command-stderr",
    "command-name",
    "command-message",
    "command-args",
    "environment_context",
    "system-reminder",
    "user-prompt-submit-hook",
    "instructions",
];

/// The human-typed subject of a prompt, or `None` when the text is pure harness envelope.
///
/// Strips each `<tag>…</tag>` block (and an unclosed `<tag>` opener, which is what a truncated scan
/// leaves behind), then any leftover standalone `<…>` line. Whatever survives is what the person
/// actually wrote; if nothing survives, the caller keeps scanning for the next user line rather than
/// captioning the row with a machine's words.
fn prompt_subject(text: &str) -> Option<String> {
    let mut s = text.to_string();
    for tag in ENVELOPE_TAGS {
        let open = format!("<{tag}>");
        let close = format!("</{tag}>");
        loop {
            let Some(start) = s.find(&open) else { break };
            let end = match s[start..].find(&close) {
                Some(rel) => start + rel + close.len(),
                // Unclosed opener: the block runs to the end of what we have.
                None => s.len(),
            };
            s.replace_range(start..end, " ");
        }
    }
    // A line that is nothing but one tag (e.g. a bare `<cwd>…</cwd>` left by a nested envelope).
    let kept: Vec<&str> = s
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !(l.starts_with('<') && l.ends_with('>')))
        .collect();
    let joined = kept.join(" ");
    let joined = joined.trim();
    if joined.is_empty() {
        None
    } else {
        Some(joined.to_string())
    }
}

fn file_mtime_ms(path: &std::path::Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()
        .and_then(|md| md.modified().ok())
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.trim();
    // Collapse newlines so a multi-line prompt fits one picker row.
    let flat: String = s
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() <= max {
        return flat.to_string();
    }
    let kept: String = flat.chars().take(max.saturating_sub(1)).collect();
    format!("{kept}…")
}

// ── full parse (on selection) ────────────────────────────────────────────────

/// Parse a foreign transcript into aizen's `Vec<Message>` and repair it so it satisfies
/// `assert_valid_history`. This is what the picker hands to the thread-switch path.
pub fn load(session: &ForeignSession) -> Result<Vec<Message>, String> {
    let mut msgs = match session.cli {
        Cli::Claude => parse_claude(&session.path),
        Cli::Codex => parse_codex(&session.path),
    }?;
    repair_history(&mut msgs);
    Ok(msgs)
}

/// Full Claude CLI parse → aizen messages. Reads every line; drops sidechain, system/meta
/// bookkeeping, `thinking` parts, and the source CLI's `developer`/`system` harness turns.
fn parse_claude(path: &std::path::Path) -> Result<Vec<Message>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let mut out: Vec<Message> = Vec::new();
    // A Claude transcript is NOT a linear log. On resume/fork the CLI re-appends the entries it
    // replayed, so the same turn can appear many times over — one real file here had 72,959 lines
    // carrying only 49,128 distinct entries, with a single tool_result written four times. Each
    // entry carries a stable `uuid`, so the first occurrence wins and later copies are skipped.
    // Without this, one tool_use id gets several results and `assert_valid_history` rejects the
    // import (a provider returns 400 on a duplicate result).
    let mut seen_uuid: std::collections::HashSet<String> = std::collections::HashSet::new();
    for line in bytes.split(|b| *b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_slice(line) {
            Ok(v) => v,
            Err(_) => continue, // a single malformed line doesn't void the rest of the transcript
        };
        if v.get("isSidechain").and_then(|b| b.as_bool()) == Some(true) {
            continue;
        }
        // Entries without a uuid can't be deduped; keep them (they're rare and never tool traffic).
        if let Some(uuid) = v.get("uuid").and_then(|u| u.as_str()) {
            if !seen_uuid.insert(uuid.to_string()) {
                continue;
            }
        }
        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let msg = match v.get("message") {
            Some(m) => m,
            None => continue,
        };
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");
        match ty {
            "user" => {
                // A user turn may be a real prompt (string or text-parts) or a tool_result envelope.
                if let Some(result) = claude_tool_result(msg) {
                    out.push(result);
                } else if let Some(text) = claude_human_text_v(msg) {
                    out.push(Message::user(text));
                }
            }
            "assistant" if role == "assistant" => {
                if let Some(m) = claude_assistant(msg) {
                    out.push(m);
                }
            }
            _ => {} // mode/permission/snapshot/queue/attachment/summary — not conversation.
        }
    }
    Ok(out)
}

/// Claude `message.content` as a plain human string (string OR text-part array), ignoring
/// tool_result parts. Distinct from [`claude_human_text`] only in that this is the full-text
/// version (no truncation) used during real parse.
fn claude_human_text_v(msg: &serde_json::Value) -> Option<String> {
    let content = msg.get("content")?;
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        let mut out = String::new();
        for part in arr {
            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

/// If a Claude user `message` is a tool_result envelope, build the aizen `tool` message from it.
fn claude_tool_result(msg: &serde_json::Value) -> Option<Message> {
    let arr = msg.get("content")?.as_array()?;
    let mut result = None;
    for part in arr {
        if part.get("type").and_then(|t| t.as_str()) == Some("tool_result") {
            let id = part.get("tool_use_id").and_then(|i| i.as_str())?;
            let is_error = part
                .get("is_error")
                .and_then(|b| b.as_bool())
                .unwrap_or(false);
            let body = part
                .get("content")
                .and_then(|c| claude_result_body(c))
                .unwrap_or_default();
            let body = if is_error && !body.is_empty() {
                format!("Error: {body}")
            } else if is_error {
                "(tool error)".to_string()
            } else {
                body
            };
            // First tool_result part wins; a well-formed Claude line has exactly one.
            result = Some(Message::tool_result(id, body));
            break;
        }
    }
    result
}

/// Claude nests tool_result content as either a string or an array of text parts.
fn claude_result_body(content: &serde_json::Value) -> Option<String> {
    if let Some(s) = content.as_str() {
        return Some(s.to_string());
    }
    if let Some(arr) = content.as_array() {
        let mut out = String::new();
        for part in arr {
            if part.get("type").and_then(|t| t.as_str()) == Some("text") {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
        }
        if !out.is_empty() {
            return Some(out);
        }
    }
    None
}

/// Build an aizen assistant message from a Claude assistant turn: concatenate `text` parts (skip
/// `thinking`) and collect `tool_use` parts into `tool_calls`. The `tool_use.id` is preserved
/// verbatim so the matching `tool_result` re-links by id.
fn claude_assistant(msg: &serde_json::Value) -> Option<Message> {
    let arr = msg.get("content")?.as_array()?;
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for part in arr {
        match part.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(t);
                }
            }
            Some("tool_use") => {
                let id = part
                    .get("id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = part
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                // `input` is a JSON object; aizen's `FunctionCall.arguments` is the STRINGIFIED
                // object (the wire footgun documented on the field). Stringify to match.
                let input = part.get("input").cloned().unwrap_or(serde_json::json!({}));
                let arguments = serde_json::to_string(&input).unwrap_or_else(|_| "{}".to_string());
                tool_calls.push(ToolCall {
                    id,
                    kind: "function".to_string(),
                    function: FunctionCall { name, arguments },
                });
            }
            _ => {} // thinking, redacted_thinking, etc. — deliberately dropped.
        }
    }
    if text.is_empty() && tool_calls.is_empty() {
        return None;
    }
    Some(Message {
        role: "assistant".to_string(),
        content: if text.is_empty() { None } else { Some(text) },
        tool_calls,
        tool_call_id: None,
        images: Vec::new(),
        cache_control: None,
    })
}

/// Full Codex parse → aizen messages. Codex emits `response_item` lines whose payload is a
/// `message`, `function_call`, or `function_call_output` (plus `custom_tool_call*` and
/// `reasoning`). We rebuild the assistant→tool_call→tool_result spine in order.
fn parse_codex(path: &std::path::Path) -> Result<Vec<Message>, String> {
    use std::io::{BufRead, BufReader};
    let f = std::fs::File::open(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let reader = BufReader::new(f);
    let mut out: Vec<Message> = Vec::new();
    // Pending function calls keyed by call_id, so an output arriving later can be emitted as the
    // tool_result right after the (already-pushed) assistant tool_call. Codex separates the call
    // and its output into two response_items, possibly with reasoning between them.
    let mut pending: std::collections::HashMap<String, ()> = std::collections::HashMap::new();
    let mut buf = String::new();
    for line in reader.lines() {
        buf.clear();
        let line = match line {
            Ok(l) => l,
            Err(_) => continue,
        };
        if line.trim().is_empty() {
            continue;
        }
        let v: serde_json::Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v.get("type").and_then(|t| t.as_str()) != Some("response_item") {
            continue; // session_meta / event_msg / turn_context / compacted — not conversation.
        }
        let payload = match v.get("payload") {
            Some(p) => p,
            None => continue,
        };
        let pty = payload.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match pty {
            "message" => {
                let role = payload.get("role").and_then(|r| r.as_str()).unwrap_or("");
                match role {
                    "user" => {
                        if let Some(text) = codex_message_text(payload) {
                            out.push(Message::user(text));
                        }
                    }
                    "assistant" => {
                        if let Some(text) = codex_message_text(payload) {
                            out.push(Message::assistant(text));
                        }
                    }
                    _ => {} // developer/system harness — dropped (aizen seeds its own prompt).
                }
            }
            "function_call" | "custom_tool_call" => {
                let id = payload
                    .get("call_id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let name = payload
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("")
                    .to_string();
                // Codex `arguments` is already a stringified JSON object (like aizen's wire shape).
                let arguments = payload
                    .get("arguments")
                    .and_then(|a| a.as_str())
                    .unwrap_or("{}")
                    .to_string();
                if !id.is_empty() {
                    pending.insert(id.clone(), ());
                }
                // Attach the call to the trailing assistant message if it has no calls yet (the
                // common case: an assistant message, then its function_calls); otherwise emit a
                // standalone assistant tool-call turn — same shape `assistant_tool_calls` builds.
                let call = ToolCall {
                    id,
                    kind: "function".to_string(),
                    function: FunctionCall { name, arguments },
                };
                attach_or_push_call(&mut out, call);
            }
            "function_call_output" | "custom_tool_call_output" => {
                let id = payload
                    .get("call_id")
                    .and_then(|i| i.as_str())
                    .unwrap_or("")
                    .to_string();
                let body = payload
                    .get("output")
                    .and_then(|o| o.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| {
                        // Codex sometimes nests output as an object; stringify it rather than drop.
                        payload
                            .get("output")
                            .map(|o| serde_json::to_string(o).unwrap_or_default())
                            .unwrap_or_default()
                    });
                if !id.is_empty() {
                    pending.remove(&id);
                    out.push(Message::tool_result(id, body));
                }
            }
            _ => {} // reasoning and anything else — dropped.
        }
    }
    let _ = pending; // any still-pending calls are closed by repair_history.
    Ok(out)
}

/// Attach a tool call to the trailing assistant message when it can take one (assistant role, no
/// calls yet, no tool_call_id); otherwise push a fresh assistant tool-call turn. Keeps the
/// assistant→call grouping Codex intended without ever producing an invalid intermediate shape.
fn attach_or_push_call(out: &mut Vec<Message>, call: ToolCall) {
    if let Some(last) = out.last_mut() {
        if last.role == "assistant"
            && last.tool_call_id.is_none()
            && last.tool_calls.is_empty()
            && last.content.is_none()
        {
            last.tool_calls.push(call);
            return;
        }
    }
    out.push(Message {
        role: "assistant".to_string(),
        content: None,
        tool_calls: vec![call],
        tool_call_id: None,
        images: Vec::new(),
        cache_control: None,
    });
}

// ── repair: make the imported transcript satisfy `assert_valid_history` ───────

/// Synthetic placeholder inserted for a tool_call whose result the source CLI never recorded (the
/// transcript was trimmed/compacted mid-call). Honest about the gap rather than fabricated content.
const MISSING_RESULT: &str = "(no result recorded — imported from a foreign CLI transcript)";

/// Close every open `tool_call` and drop every orphan `tool` result so the imported transcript
/// satisfies the invariant every aizen request relies on:
///
/// 1. A `tool` message's `tool_call_id` must reference a `tool_call` on an EARLIER assistant turn.
///    A foreign transcript compacted from the front can leave a tool_result whose call was cut —
///    the provider returns 400 on the orphan. Such results are DROPPED (not re-paired): the call
///    they answered is gone, so there is nothing to attach them to.
/// 2. An assistant `tool_call` must be closed by a `tool` result before the next assistant turn.
///    A foreign transcript trimmed at the tail can leave the last call unanswered — same 400.
///    Such calls get a synthetic `tool_result` ([`MISSING_RESULT`]) so the pair is complete.
///
/// Runs in one left-to-right pass with a set of declared call ids, mirroring `assert_valid_history`
/// exactly — what passes this passes that.
pub fn repair_history(msgs: &mut Vec<Message>) {
    use std::collections::HashSet;

    // Pass 1 — drop orphan tool results (no preceding matching tool_call). Build the declared-id
    // set as we go so a result only matches a call that came BEFORE it, never one later.
    let mut declared: HashSet<String> = HashSet::new();
    let mut keep = Vec::with_capacity(msgs.len());
    for m in msgs.drain(..) {
        if m.role == "tool" {
            let Some(id) = m.tool_call_id.as_deref() else {
                continue; // a tool message with no id is structurally invalid — drop it.
            };
            if !declared.contains(id) {
                continue; // orphan: its call was never declared (cut by compaction). drop.
            }
            keep.push(m);
        } else {
            for tc in &m.tool_calls {
                declared.insert(tc.id.clone());
            }
            keep.push(m);
        }
    }
    *msgs = keep;

    // Pass 2 — close every declared call that never got a result, inserting a synthetic tool_result
    // right before the next assistant turn (or at the end). Grouped by trailing-assistant so we
    // don't interleave a synthetic result into the middle of one assistant's calls.
    let mut seen_results: HashSet<String> = HashSet::new();
    for m in msgs.iter() {
        if m.role == "tool" {
            if let Some(id) = m.tool_call_id.clone() {
                seen_results.insert(id);
            }
        }
    }
    let open: Vec<String> = declared
        .into_iter()
        .filter(|id| !seen_results.contains(id))
        .collect();
    if open.is_empty() {
        return;
    }
    // Append synthetic results at the tail. They are all for the LAST assistant's open calls;
    // inserting mid-history would be valid too, but the tail is where a trimmed transcript loses
    // them, so the tail is where they belong.
    for id in open {
        msgs.push(Message::tool_result(id, MISSING_RESULT));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    fn assert_valid(msgs: &[Message]) {
        // Mirror of `agent::mod.rs::assert_valid_history` — the invariant repair must satisfy.
        let mut declared: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for m in msgs {
            for tc in &m.tool_calls {
                declared.insert(tc.id.as_str());
            }
            if m.role == "tool" {
                let id = m
                    .tool_call_id
                    .as_deref()
                    .expect("tool msg has tool_call_id");
                assert!(
                    declared.contains(id),
                    "orphan tool result '{id}' after repair"
                );
            }
        }
        // No two results for the same call id (repair must not double-close).
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for m in msgs {
            if m.role == "tool" {
                let id = m.tool_call_id.as_deref().unwrap();
                assert!(seen.insert(id), "duplicate tool result for '{id}'");
            }
        }
    }

    /// A temp tree unique to ONE test, so two tests never write the same transcript path.
    fn tmp_home(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("aizen-foreign-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir); // a re-run must not see the last run's files
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// Pin `AIZEN_FOREIGN_HOME` for the duration of a `discover` test.
    ///
    /// The var is process-global, so the lock must be held for the WHOLE set-use-unset window —
    /// acquiring it after `set_var` (or dropping it before `remove_var`) lets a sibling test clear
    /// the var mid-assertion, which is exactly the flake this replaces. Restoring on `Drop` also
    /// keeps a panicking test from leaking the override into whatever runs next.
    struct ForeignHome {
        _guard: std::sync::MutexGuard<'static, ()>,
    }

    impl ForeignHome {
        fn pin(dir: &std::path::Path) -> Self {
            let guard = crate::core::config::TEST_HOME_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            std::env::set_var("AIZEN_FOREIGN_HOME", dir);
            Self { _guard: guard }
        }
    }

    impl Drop for ForeignHome {
        fn drop(&mut self) {
            std::env::remove_var("AIZEN_FOREIGN_HOME");
        }
    }

    /// The largest `.jsonl` under the REAL `~/.claude/projects`, or `None` on a machine that has
    /// never run Claude Code. Resolves the home from `USERPROFILE`/`HOME` directly rather than
    /// through [`foreign_roots`]: the caller has already pinned `AIZEN_FOREIGN_HOME` to a temp tree,
    /// and this is the one place that deliberately wants the real one.
    fn largest_real_claude_transcript() -> Option<std::path::PathBuf> {
        let home = std::env::var("USERPROFILE")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| std::env::var("HOME").ok().filter(|s| !s.trim().is_empty()))?;
        let projects = std::path::PathBuf::from(home)
            .join(".claude")
            .join("projects");
        if !projects.is_dir() {
            return None;
        }
        let mut found = Vec::new();
        collect_jsonl(projects, &mut found);
        found
            .into_iter()
            .filter_map(|p| std::fs::metadata(&p).ok().map(|m| (m.len(), p)))
            .max_by_key(|(len, _)| *len)
            .map(|(_, p)| p)
    }

    fn write_jsonl(path: &std::path::Path, lines: &[&str]) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let body = lines.join("\n");
        std::fs::write(path, body).unwrap();
    }

    // ── parse_claude ──────────────────────────────────────────────────────────
    // These call the parser with an explicit path, so they need no env override at all.

    #[test]
    fn claude_string_content_user_and_assistant_text() {
        let home = tmp_home("claude-string");
        let f = home.join(".claude/projects/x/s.jsonl");
        write_jsonl(
            &f,
            &[
                r#"{"type":"user","message":{"role":"user","content":"hello"},"cwd":"/repo"}"#,
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"hi there"}]}}"#,
            ],
        );
        let msgs = parse_claude(&f).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[0].content.as_deref(), Some("hello"));
        assert_eq!(msgs[1].role, "assistant");
        assert_eq!(msgs[1].content.as_deref(), Some("hi there"));
    }

    #[test]
    fn claude_tool_use_and_result_relink_by_id() {
        let home = tmp_home("claude-tooluse");
        let f = home.join(".claude/projects/x/s.jsonl");
        write_jsonl(
            &f,
            &[
                r#"{"type":"user","message":{"role":"user","content":"do it"},"cwd":"/repo"}"#,
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"ok"},{"type":"tool_use","id":"call_1","name":"bash","input":{"cmd":"ls"}}]}}"#,
                r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call_1","content":"file.txt"}]}}"#,
            ],
        );
        let mut msgs = parse_claude(&f).unwrap();
        repair_history(&mut msgs);
        assert_valid(&msgs);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[1].tool_calls.len(), 1);
        assert_eq!(msgs[1].tool_calls[0].id, "call_1");
        assert_eq!(msgs[1].tool_calls[0].function.name, "bash");
        // arguments must be the STRINGIFIED object, not a nested object.
        assert_eq!(msgs[1].tool_calls[0].function.arguments, r#"{"cmd":"ls"}"#);
        assert_eq!(msgs[2].role, "tool");
        assert_eq!(msgs[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(msgs[2].content.as_deref(), Some("file.txt"));
    }

    #[test]
    fn claude_thinking_and_sidechain_are_dropped() {
        let home = tmp_home("claude-thinking");
        let f = home.join(".claude/projects/x/s.jsonl");
        write_jsonl(
            &f,
            &[
                r#"{"type":"user","message":{"role":"user","content":"hi"},"cwd":"/repo"}"#,
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"secret plan"},{"type":"text","text":"answer"}]}}"#,
                r#"{"type":"user","isSidechain":true,"message":{"role":"user","content":"subagent noise"}}"#,
            ],
        );
        let msgs = parse_claude(&f).unwrap();
        assert_eq!(
            msgs.len(),
            2,
            "thinking kept its text; sidechain line dropped"
        );
        assert_eq!(msgs[1].content.as_deref(), Some("answer"));
        assert!(!msgs
            .iter()
            .any(|m| m.content.as_deref() == Some("subagent noise")));
    }

    // ── parse_codex ───────────────────────────────────────────────────────────

    #[test]
    fn codex_function_call_and_output_relink() {
        let home = tmp_home("codex-call");
        let f = home.join(".codex/sessions/2026/01/01/r.jsonl");
        write_jsonl(
            &f,
            &[
                r#"{"type":"session_meta","payload":{"cwd":"/repo"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"run it"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"sure"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"function_call","call_id":"c1","name":"shell","arguments":"{\"cmd\":\"ls\"}"}}"#,
                r#"{"type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"out"}}"#,
            ],
        );
        let mut msgs = parse_codex(&f).unwrap();
        repair_history(&mut msgs);
        assert_valid(&msgs);
        // user, assistant(text), assistant(tool_call), tool(result)
        assert_eq!(msgs.len(), 4);
        assert_eq!(msgs[2].tool_calls[0].id, "c1");
        assert_eq!(msgs[2].tool_calls[0].function.arguments, r#"{"cmd":"ls"}"#);
        assert_eq!(msgs[3].role, "tool");
        assert_eq!(msgs[3].tool_call_id.as_deref(), Some("c1"));
    }

    #[test]
    fn codex_developer_and_reasoning_dropped() {
        let home = tmp_home("codex-dev");
        let f = home.join(".codex/sessions/2026/01/01/r.jsonl");
        write_jsonl(
            &f,
            &[
                r#"{"type":"session_meta","payload":{"cwd":"/repo"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"developer","content":[{"type":"input_text","text":"harness prompt"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"reasoning","content":[{"type":"text","text":"secret"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#,
            ],
        );
        let msgs = parse_codex(&f).unwrap();
        assert_eq!(msgs.len(), 1, "developer + reasoning dropped, user kept");
        assert_eq!(msgs[0].content.as_deref(), Some("hi"));
    }

    // ── repair_history ────────────────────────────────────────────────────────

    #[test]
    fn repair_drops_orphan_tool_result() {
        let mut msgs = vec![
            // tool_result with no preceding call — compaction cut the call.
            Message::tool_result("ghost", "orphan body"),
            Message::user("hi"),
        ];
        repair_history(&mut msgs);
        assert_valid(&msgs);
        assert!(!msgs
            .iter()
            .any(|m| m.tool_call_id.as_deref() == Some("ghost")));
    }

    #[test]
    fn repair_closes_open_tool_call_with_synthetic_result() {
        let mut msgs = vec![
            Message::user("do it"),
            Message {
                role: "assistant".to_string(),
                content: None,
                tool_calls: vec![ToolCall {
                    id: "open1".to_string(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "bash".to_string(),
                        arguments: "{}".to_string(),
                    },
                }],
                tool_call_id: None,
                images: Vec::new(),
                cache_control: None,
            },
        ];
        repair_history(&mut msgs);
        assert_valid(&msgs);
        let last = msgs.last().unwrap();
        assert_eq!(last.role, "tool");
        assert_eq!(last.tool_call_id.as_deref(), Some("open1"));
        assert_eq!(last.content.as_deref(), Some(MISSING_RESULT));
    }

    #[test]
    fn repair_leaves_already_valid_history_untouched() {
        let mut msgs = vec![
            Message::user("do it"),
            Message {
                role: "assistant".to_string(),
                content: None,
                tool_calls: vec![ToolCall {
                    id: "c".to_string(),
                    kind: "function".to_string(),
                    function: FunctionCall {
                        name: "bash".to_string(),
                        arguments: "{}".to_string(),
                    },
                }],
                tool_call_id: None,
                images: Vec::new(),
                cache_control: None,
            },
            Message::tool_result("c", "done"),
        ];
        let before = msgs.clone();
        repair_history(&mut msgs);
        assert_valid(&msgs);
        assert_eq!(msgs.len(), before.len());
        assert_eq!(
            msgs[2].content.as_deref(),
            Some("done"),
            "real result kept, no synthetic added"
        );
    }

    // ── discover ──────────────────────────────────────────────────────────────

    #[test]
    fn discover_filters_by_project_cwd_case_insensitive() {
        let home = tmp_home("discover");
        let _env = ForeignHome::pin(&home);
        // Recorded cwd "C:\Repo" must match a project root queried as "c:\repo" (Windows casing).
        let claude = home.join(".claude/projects/x/match.jsonl");
        write_jsonl(
            &claude,
            &[r#"{"type":"user","message":{"role":"user","content":"hi"},"cwd":"C:\\Repo"}"#],
        );
        let other = home.join(".claude/projects/x/other.jsonl");
        write_jsonl(
            &other,
            &[r#"{"type":"user","message":{"role":"user","content":"hi"},"cwd":"C:\\Elsewhere"}"#],
        );
        let codex = home.join(".codex/sessions/2026/01/01/c.jsonl");
        write_jsonl(
            &codex,
            &[
                r#"{"type":"session_meta","payload":{"cwd":"C:\\Repo\\sub"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}}"#,
            ],
        );
        let root = std::path::PathBuf::from(r"C:\Repo");
        let found = discover(&root);
        // match.jsonl (exact, diff case) + c.jsonl (subdir of repo). other.jsonl excluded.
        assert_eq!(
            found.len(),
            2,
            "got: {:?}",
            found.iter().map(|s| &s.path).collect::<Vec<_>>()
        );
        assert!(found
            .iter()
            .all(|s| s.cli == Cli::Claude || s.cli == Cli::Codex));
    }

    /// The project filter is deliberately ONE-directional, and both directions are pinned here
    /// because the symmetric version reads more natural and is wrong.
    ///
    /// Measured on the developer's real `~/.claude` + `~/.codex` before this was narrowed: 18 Claude
    /// transcripts would have been offered for the aizen checkout, of which 9 were genuine and 9 were
    /// ancestor over-matches (6 recorded in `~` itself, 3 in the parent `mini_project/` — a folder
    /// that also holds nextgen, bot_banhang_tele and three aizen_* siblings). Codex added two more
    /// from `mini_project` and one from `~`.
    #[test]
    fn cwd_filter_accepts_subdirs_and_rejects_ancestors() {
        let root = std::path::Path::new(r"C:\Users\me\proj\aizen");
        // Exact, including the drive-letter casing Claude and Codex disagree about.
        assert!(cwd_matches(r"C:\Users\me\proj\aizen", root));
        assert!(cwd_matches(r"c:\users\me\proj\aizen", root));
        assert!(
            cwd_matches(r"C:\Users\me\proj\aizen\", root),
            "trailing sep"
        );
        // Launched from a subdir of the checkout — still this project.
        assert!(cwd_matches(r"C:\Users\me\proj\aizen\crates\core", root));
        // A PARENT of the checkout is not this project. `project_root` already resolved to the
        // toplevel, so these can only be marker-less parents holding unrelated siblings.
        assert!(!cwd_matches(r"C:\Users\me\proj", root), "parent dir");
        assert!(!cwd_matches(r"C:\Users\me", root), "home dir");
        // A sibling whose path is a string PREFIX of the root must not match either — the guard is
        // the `/` in the prefix test, and dropping it would pull `aizen_be`/`aizen_web` in.
        assert!(!cwd_matches(r"C:\Users\me\proj\aizen_be", root));
        assert!(!cwd_matches(r"C:\Users\me\proj\aizen_web", root));
    }

    // ── picker row ────────────────────────────────────────────────────────────

    #[test]
    fn envelope_only_prompts_have_no_subject() {
        // Every one of these arrives as a `user` line but was written by the harness, not the person.
        assert_eq!(
            prompt_subject("<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>"),
            None
        );
        assert_eq!(
            prompt_subject("<environment_context>\n  <cwd>C:\\Users\\admin\\Desktop</cwd>\n</environment_context>"),
            None
        );
        // A truncated scan leaves an UNCLOSED opener; it must still be recognized as envelope.
        assert_eq!(
            prompt_subject("<local-command-caveat>Caveat: The messages below were gener"),
            None
        );
    }

    #[test]
    fn subject_survives_when_a_real_prompt_is_wrapped_in_envelope() {
        assert_eq!(
            prompt_subject(
                "<local-command-caveat>Caveat: blah</local-command-caveat>\nfix the auth handler"
            )
            .as_deref(),
            Some("fix the auth handler")
        );
        assert_eq!(
            prompt_subject("just a normal prompt").as_deref(),
            Some("just a normal prompt")
        );
    }

    #[test]
    fn first_prompt_skips_envelope_turns_and_finds_the_real_one() {
        let home = tmp_home("subject-scan");
        let f = home.join(".claude/projects/x/s.jsonl");
        write_jsonl(
            &f,
            &[
                // Turn 1 is pure caveat — the shape that made every row in the picker identical.
                r#"{"type":"user","message":{"role":"user","content":"<local-command-caveat>Caveat: The messages below were generated by the user while running local commands.</local-command-caveat>"},"cwd":"C:\\Repo"}"#,
                r#"{"type":"user","message":{"role":"user","content":"<command-name>/compact</command-name>"},"cwd":"C:\\Repo"}"#,
                r#"{"type":"user","message":{"role":"user","content":"rewrite the auth handler"},"cwd":"C:\\Repo"}"#,
            ],
        );
        let row = read_claude_row(&f, std::path::Path::new(r"C:\Repo")).unwrap();
        assert_eq!(row.first_prompt, "rewrite the auth handler");
    }

    #[test]
    fn codex_first_prompt_skips_the_environment_context_item() {
        let home = tmp_home("codex-subject");
        let f = home.join(".codex/sessions/2026/01/01/c.jsonl");
        write_jsonl(
            &f,
            &[
                r#"{"type":"session_meta","payload":{"cwd":"C:\\Repo"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"<environment_context>\n  <cwd>C:\\Repo</cwd>\n</environment_context>"}]}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"clone repos này về"}]}}"#,
            ],
        );
        let row = read_codex_row(&f, std::path::Path::new(r"C:\Repo")).unwrap();
        assert_eq!(row.first_prompt, "clone repos này về");
    }

    #[test]
    fn row_is_gutter_plus_subject_and_drops_the_root_cwd_note() {
        // A transcript launched from the project root: the note would repeat on every row, so
        // `discover` blanks it and the row is age + tag + subject only.
        let at_root = ForeignSession {
            cli: Cli::Claude,
            path: std::path::PathBuf::from("/x/s.jsonl"),
            cwd: String::new(),
            mtime_ms: None,
            turns: 4143,
            first_prompt: "rewrite the auth handler".into(),
        };
        let r = at_root.row(|_| "1m".to_string(), 100);
        assert_eq!(r, " 1m  claude rewrite the auth handler");
        assert!(
            !r.contains("turns"),
            "turn count is noise in a 240-row list: {r}"
        );
        // A SUBDIR launch still earns its note — that is information the subject can't carry.
        let in_sub = ForeignSession {
            cwd: "/repo/crates/core".into(),
            ..at_root.clone()
        };
        assert!(in_sub.row(|_| "1m".to_string(), 100).contains("(core)"));
    }

    #[test]
    fn rows_line_up_and_never_exceed_the_given_width() {
        // Two rows whose ages differ in length must still start their subject at the same column:
        // that shared column is what lets 240 rows be scanned vertically.
        let mk = |age: &'static str, subject: &str| {
            ForeignSession {
                cli: Cli::Codex,
                path: std::path::PathBuf::from("/x/s.jsonl"),
                cwd: String::new(),
                mtime_ms: None,
                turns: 0,
                first_prompt: subject.to_string(),
            }
            .row(move |_| age.to_string(), 40)
        };
        let a = mk("1m", "alpha");
        let b = mk("62d", "beta");
        assert_eq!(
            a.find("alpha"),
            b.find("beta"),
            "subjects must share a column:\n{a}\n{b}"
        );
        // Over-long subjects are clipped, not wrapped — a wrapped item shifts every row below it.
        let long = mk("1m", &"x".repeat(200));
        assert!(long.chars().count() <= 40, "len {}", long.chars().count());
        assert!(long.ends_with('…'), "{long}");
        // Multi-byte subjects clip by CHARS, not bytes (a byte slice here would panic mid-codepoint).
        let viet = mk("1m", &"đường".repeat(50));
        assert!(viet.chars().count() <= 40);
    }

    #[test]
    fn discover_blanks_cwd_only_when_it_is_the_project_root() {
        let home = tmp_home("cwd-blank");
        let _env = ForeignHome::pin(&home);
        write_jsonl(
            &home.join(".claude/projects/x/root.jsonl"),
            &[r#"{"type":"user","message":{"role":"user","content":"a"},"cwd":"C:\\Repo"}"#],
        );
        write_jsonl(
            &home.join(".claude/projects/x/sub.jsonl"),
            &[r#"{"type":"user","message":{"role":"user","content":"b"},"cwd":"C:\\Repo\\sub"}"#],
        );
        let found = discover(std::path::Path::new(r"C:\Repo"));
        assert_eq!(found.len(), 2);
        let root = found
            .iter()
            .find(|s| s.path.ends_with("root.jsonl"))
            .unwrap();
        let sub = found
            .iter()
            .find(|s| s.path.ends_with("sub.jsonl"))
            .unwrap();
        assert!(root.cwd.is_empty(), "root cwd should be blanked");
        assert!(!sub.cwd.is_empty(), "subdir cwd carries information");
    }

    #[test]
    fn load_returns_valid_history_for_both_clis() {
        let home = tmp_home("load");
        let claude = home.join(".claude/projects/x/s.jsonl");
        write_jsonl(
            &claude,
            &[
                r#"{"type":"user","message":{"role":"user","content":"hi"},"cwd":"/repo"}"#,
                r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"a","name":"bash","input":{}}]}}"#,
                // NOTE: no tool_result — trimmed tail. repair must close it.
            ],
        );
        let sess = ForeignSession {
            cli: Cli::Claude,
            path: claude,
            cwd: "/repo".into(),
            mtime_ms: None,
            turns: 2,
            first_prompt: "hi".into(),
        };
        let msgs = load(&sess).unwrap();
        assert_valid(&msgs);
        assert!(msgs
            .iter()
            .any(|m| m.role == "tool" && m.tool_call_id.as_deref() == Some("a")));
    }

    /// Claude re-appends already-recorded entries when a session is resumed or forked, so the same
    /// turn can appear several times in one file. Deduping by the per-entry `uuid` is what keeps a
    /// tool_use from collecting several results — which `assert_valid_history` rejects outright.
    #[test]
    fn claude_replayed_duplicate_entries_are_deduped_by_uuid() {
        let path = tmp_home("dupe").join("dupe.jsonl");
        let user = r#"{"uuid":"u1","type":"user","message":{"role":"user","content":"go"}}"#;
        let call = r#"{"uuid":"u2","type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"bash","input":{}}]}}"#;
        let result = r#"{"uuid":"u3","type":"user","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":"ok"}]}}"#;
        // The whole trio written twice, exactly as a resumed session records it.
        write_jsonl(&path, &[user, call, result, user, call, result]);
        let msgs = parse_claude(&path).unwrap();
        assert_valid(&msgs); // would trip "duplicate tool result for 't1'" without dedupe
        assert_eq!(
            msgs.len(),
            3,
            "replayed copies must not be re-imported: {msgs:?}"
        );
        assert_eq!(msgs.iter().filter(|m| m.role == "tool").count(), 1);
    }

    /// Integration smoke: load the largest real transcript on disk for this project and verify
    /// repair_history satisfies the invariant every aizen request relies on. This is the closest
    /// unit-test can get to hand-testing with your actual data. Skipped when no such file exists
    /// (a fresh machine or someone who never used Claude Code).
    #[test]
    fn smoke_load_real_claude_transcript_validates() {
        let home = tmp_home("smoke");
        let _env = ForeignHome::pin(&home);
        // Pick the LARGEST real Claude transcript under the developer's own `~/.claude/projects`,
        // whatever it happens to be called — biggest file means most turns, most tool calls, and the
        // best odds of containing the trimmed-tail and compaction gaps `repair_history` exists for.
        // Discovered rather than hardcoded so this runs on any machine that has ever used Claude
        // Code, and skips cleanly (not fails) on one that hasn't — CI included.
        let Some(src) = largest_real_claude_transcript() else {
            eprintln!("SKIP: no real Claude transcript found under ~/.claude/projects");
            return;
        };
        let dst = home.join(".claude/projects/x/real.jsonl");
        std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
        std::fs::copy(&src, &dst).unwrap();
        let sess = ForeignSession {
            cli: Cli::Claude,
            path: dst,
            cwd: String::new(),
            mtime_ms: None,
            turns: 0,
            first_prompt: String::new(),
        };
        let msgs = load(&sess).unwrap();
        assert_valid(&msgs);
        // Also confirm repair actually did something: there ARE tool calls that were closed by synthetic results.
        let mut declared: HashSet<String> = HashSet::new();
        for m in &msgs {
            for tc in &m.tool_calls {
                declared.insert(tc.id.clone());
            }
        }
        let seen_results: HashSet<&str> = msgs
            .iter()
            .filter(|m| m.role == "tool")
            .filter_map(|m| m.tool_call_id.as_deref())
            .collect();
        let open_after_repair = declared
            .iter()
            .filter(|id| !seen_results.contains(id.as_str()))
            .count();
        assert_eq!(
            open_after_repair, 0,
            "all declared calls must be closed after repair"
        );
    }
}
