//! In-session task list (the `todo_write` tool) — makes multi-step work visibly tracked, the way
//! Claude Code's todo list is one of its most legible features. Pure in-memory, zero infra: a
//! process-global list the model REPLACES each call (send-the-whole-list semantics), rendered as a
//! checklist into the scroll region + summarised in the status bar. Reset on `/clear`.
//!
//! P0 harness persistence: optional `confidence` / `hill_climbable` fields + incomplete helpers
//! used by the agent loop's todo-poke / confidence-gate / hill-climb paths.

use crate::agent::tools::Tool;
use anyhow::Result;
use console::style;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Todo {
    pub content: String,
    #[serde(default = "default_status")]
    pub status: Status,
    /// Honest 0–100 confidence (P0.2). Omit on trivial tasks. Used by the confidence-spike gate.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<u8>,
    /// How quantifiable/iterable this goal is, 0–100 (P0.3). Below the gate → reframe nudge.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hill_climbable: Option<u8>,
}

fn default_status() -> Status {
    Status::Pending
}

impl Todo {
    /// Construct a todo without optional harness fields (tests + internal seeds).
    pub fn new(content: impl Into<String>, status: Status) -> Self {
        Self {
            content: content.into(),
            status,
            confidence: None,
            hill_climbable: None,
        }
    }
}

/// The session's task list. Process-global (one REPL = one session); `/clear` wipes it.
static TODOS: Lazy<Mutex<Vec<Todo>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// Replace the whole list (TodoWrite semantics: the model sends the full list every call).
pub fn set(items: Vec<Todo>) {
    *TODOS.lock().unwrap_or_else(|e| e.into_inner()) = items;
}

pub fn snapshot() -> Vec<Todo> {
    TODOS.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Wipe the list (called on `/clear` / a fresh conversation).
pub fn clear() {
    TODOS.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

/// True when any item is still pending or in_progress (empty list → false).
pub fn has_incomplete() -> bool {
    snapshot().iter().any(|t| t.status != Status::Done)
}

/// Format incomplete items as `[>]` / `[ ]` lines for the todo-poke inject. `None` if none open.
pub fn incomplete_summary(max_chars: usize) -> Option<String> {
    let items = snapshot();
    let open: Vec<&Todo> = items.iter().filter(|t| t.status != Status::Done).collect();
    if open.is_empty() {
        return None;
    }
    let mut text = String::new();
    for t in open {
        let mark = match t.status {
            Status::Done => "[x]", // filtered out; defensive
            Status::InProgress => "[>]",
            Status::Pending => "[ ]",
        };
        let line = format!("{mark} {}\n", t.content);
        if text.chars().count() + line.chars().count() > max_chars {
            text.push_str("…\n");
            break;
        }
        text.push_str(&line);
    }
    Some(text.trim_end().to_string())
}

/// `☑ done/total` for the status bar (None when there are no todos).
pub fn status_summary() -> Option<String> {
    let items = snapshot();
    if items.is_empty() {
        return None;
    }
    let done = items.iter().filter(|t| t.status == Status::Done).count();
    Some(format!("☑ {done}/{}", items.len()))
}

/// A glyph for a status: done ✓, in-progress ▸, pending ○.
fn glyph(s: Status) -> &'static str {
    match s {
        Status::Done => "✓",
        Status::InProgress => "▸",
        Status::Pending => "○",
    }
}

/// Render the list as a colored checklist block (one line per item). Empty list → empty string.
pub fn render_block(items: &[Todo]) -> String {
    if items.is_empty() {
        return String::new();
    }
    let mut out = String::new();
    out.push_str(&style("todos:").color256(crate::ui::splash::ACCENT).to_string());
    out.push('\n');
    for t in items {
        let g = glyph(t.status);
        let line = match t.status {
            // Design: green ✓ + struck, muted text · moonlight ▸ + bright active row · faint ○ + muted.
            Status::Done => {
                format!(" {} {}", crate::ui::theme::ok(g), style(&t.content).dim().strikethrough())
            }
            Status::InProgress => {
                format!(" {} {}", style(g).color256(crate::ui::splash::ACCENT).bold(), style(&t.content).bold())
            }
            Status::Pending => format!(" {} {}", crate::ui::theme::faint(g), crate::ui::theme::muted(&t.content)),
        };
        out.push_str(&line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// A terse one-line summary fed back to the MODEL (so it knows the list took effect without
/// re-echoing the whole thing into context).
fn model_ack(items: &[Todo]) -> String {
    if items.is_empty() {
        return "todo list cleared".to_string();
    }
    let done = items.iter().filter(|t| t.status == Status::Done).count();
    let doing = items.iter().filter(|t| t.status == Status::InProgress).count();
    format!("todo list updated: {} item(s), {done} done, {doing} in progress", items.len())
}

/// Clamp optional 0–100 fields after serde (accepts up to u8::MAX from JSON).
fn normalize_todos(mut items: Vec<Todo>) -> Vec<Todo> {
    for t in &mut items {
        if let Some(c) = t.confidence {
            t.confidence = Some(c.min(100));
        }
        if let Some(h) = t.hill_climbable {
            t.hill_climbable = Some(h.min(100));
        }
    }
    items
}

/// Parse a `{"todos": [...]}` args object into a `Vec<Todo>` — shared by the top-level `todo_write`
/// and the sub-agent-scoped `ScopedTodo`, so both accept exactly the same shape.
fn parse_todos(args: &serde_json::Value) -> Result<Vec<Todo>> {
    match args.get("todos") {
        Some(v) => {
            let items: Vec<Todo> = serde_json::from_value(v.clone())
                .map_err(|e| anyhow::anyhow!("invalid `todos` (expect [{{content, status}}]): {e}"))?;
            Ok(normalize_todos(items))
        }
        None => anyhow::bail!("missing `todos` array"),
    }
}

/// The `todos` array JSON Schema — identical for `todo_write` and `ScopedTodo`.
fn todos_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "todos": {
                "type": "array",
                "description": "The full task list (replaces the previous one).",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "content": {"type": "string", "description": "the task, imperative + concise"},
                        "status": {"type": "string", "enum": ["pending", "in_progress", "done"]},
                        "confidence": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": 100,
                            "description": "0–100 honest confidence at assign and at done (optional; omit on trivial tasks)"
                        },
                        "hill_climbable": {
                            "type": "integer",
                            "minimum": 0,
                            "maximum": 100,
                            "description": "0–100 how quantifiable/iterable this goal is (optional; below ~90 may trigger a reframe nudge)"
                        }
                    },
                    "required": ["content", "status"]
                }
            }
        },
        "required": ["todos"]
    })
}

/// `todo_write` — the model sends its full current task list; we replace + show it.
pub struct TodoWrite;

impl Tool for TodoWrite {
    fn name(&self) -> &str {
        "todo_write"
    }
    fn description(&self) -> &str {
        "Track a multi-step task as a visible checklist. Send the COMPLETE current list every call \
         (it REPLACES the previous one). Use it to plan multi-file / hard-to-undo work and to mark \
         progress: set ONE item to in_progress at a time, flip it to done before starting the next. \
         Not for trivial one-step tasks. Optional confidence (0–100) at assign/done and \
         hill_climbable (0–100) for quantifiable goals. The list is shown to the user and resets on \
         /clear. Leaving items incomplete will block Done (harness poke)."
    }
    fn parameters(&self) -> serde_json::Value {
        todos_schema()
    }
    // Mutates shared session state → keep it on the serial path (not a parallel read-only batch).
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &serde_json::Value) -> Result<String> {
        let items = parse_todos(args)?;
        set(items.clone());
        // Show the checklist to the USER as an in-place plan panel (retained updates the same box;
        // classic re-prints it). `status`: 0 = pending, 1 = in-progress, 2 = done — matched by the
        // renderer's ✓/▸/○ glyphs. An empty list removes the panel.
        let rows: Vec<(u8, String)> = items
            .iter()
            .map(|t| {
                let s = match t.status {
                    Status::Done => 2u8,
                    Status::InProgress => 1,
                    Status::Pending => 0,
                };
                (s, t.content.clone())
            })
            .collect();
        crate::ui::tui::plan_update(&rows);
        Ok(model_ack(&items))
    }
}

/// `todo_write` for a SUB-AGENT (W17). A sub-agent runs its own multi-step plan inside its own
/// loop, but the top-level `TodoWrite` is unavailable to it (it writes the process-global `TODOS`
/// and renders into the USER's scroll region — a sub-agent must not clobber either, and concurrent
/// read-only sub-agents would race on one global list). This variant is fully SELF-CONTAINED: it
/// holds its OWN list in a per-instance `Mutex` (fresh with each sub-agent registry), touches no
/// global state, and prints nothing to the user's UI — the only effect is the ack returned to the
/// sub-agent, which is exactly the recitation signal that keeps a long plan from drifting. The
/// list dies with the sub-agent, which is correct: its plan is scratch work, not the user's.
pub struct ScopedTodo {
    items: Mutex<Vec<Todo>>,
}

impl ScopedTodo {
    pub fn new() -> Self {
        Self {
            items: Mutex::new(Vec::new()),
        }
    }
}

impl Default for ScopedTodo {
    fn default() -> Self {
        Self::new()
    }
}

impl Tool for ScopedTodo {
    fn name(&self) -> &str {
        "todo_write"
    }
    fn description(&self) -> &str {
        "Track your OWN multi-step plan as a checklist while you work. Send the COMPLETE current \
         list every call (it REPLACES the previous one). Set ONE item to in_progress at a time and \
         flip it to done before starting the next. Use it to keep a multi-step task on track; skip \
         it for trivial one- or two-step work. Optional confidence / hill_climbable fields allowed. \
         This is your private scratch plan — it is not shown to anyone and is discarded when you \
         return your result."
    }
    fn parameters(&self) -> serde_json::Value {
        todos_schema()
    }
    // Mutates this tool's own list → keep it serial (a sub-agent runs single-threaded anyway).
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &serde_json::Value) -> Result<String> {
        let items = parse_todos(args)?;
        *self.items.lock().unwrap_or_else(|e| e.into_inner()) = items.clone();
        // No UI emission: a sub-agent's plan must not leak into the user's scroll region. The ack
        // (the recitation signal) is the whole point.
        Ok(model_ack(&items))
    }
}

/// Serializes access to the process-global TODOS list across concurrent tests / loop_eval
/// scenarios (also used by `agent::tests` for the recitation-reminder loop test).
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    // The list is a process-global, so the stateful tests must not run concurrently.
    use super::TEST_LOCK;

    #[test]
    fn set_and_summary_roundtrip() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set(vec![
            Todo::new("a", Status::Done),
            Todo::new("b", Status::InProgress),
            Todo::new("c", Status::Pending),
        ]);
        assert_eq!(status_summary().as_deref(), Some("☑ 1/3"));
        assert_eq!(snapshot().len(), 3);
        assert!(has_incomplete());
        clear();
        assert!(status_summary().is_none());
        assert!(snapshot().is_empty());
        assert!(!has_incomplete());
    }

    #[test]
    fn incomplete_summary_lists_open_only() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        set(vec![
            Todo::new("done-one", Status::Done),
            Todo::new("doing", Status::InProgress),
            Todo::new("later", Status::Pending),
        ]);
        let s = incomplete_summary(600).expect("open items");
        assert!(s.contains("[>] doing"), "{s}");
        assert!(s.contains("[ ] later"), "{s}");
        assert!(!s.contains("done-one"), "{s}");
        set(vec![Todo::new("all-done", Status::Done)]);
        assert!(incomplete_summary(600).is_none());
        clear();
    }

    #[test]
    fn execute_replaces_list() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let t = TodoWrite;
        t.execute(&serde_json::json!({"todos":[{"content":"x","status":"pending"}]})).unwrap();
        assert_eq!(snapshot().len(), 1);
        // A second call REPLACES (does not append).
        t.execute(&serde_json::json!({"todos":[
            {"content":"y","status":"in_progress"},
            {"content":"z","status":"done"}
        ]}))
        .unwrap();
        let s = snapshot();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].content, "y");
        assert_eq!(status_summary().as_deref(), Some("☑ 1/2"));
        clear();
    }

    #[test]
    fn execute_accepts_confidence_and_hill_climbable() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let t = TodoWrite;
        t.execute(&serde_json::json!({"todos":[{
            "content":"optimize parser",
            "status":"in_progress",
            "confidence": 40,
            "hill_climbable": 85
        }]})).unwrap();
        let s = snapshot();
        assert_eq!(s[0].confidence, Some(40));
        assert_eq!(s[0].hill_climbable, Some(85));
        // Clamp >100.
        t.execute(&serde_json::json!({"todos":[{
            "content":"x","status":"done","confidence": 200, "hill_climbable": 150
        }]})).unwrap();
        let s = snapshot();
        assert_eq!(s[0].confidence, Some(100));
        assert_eq!(s[0].hill_climbable, Some(100));
        clear();
    }

    #[test]
    fn render_block_is_empty_when_no_todos() {
        assert!(render_block(&[]).is_empty());
        let b = render_block(&[Todo::new("build", Status::InProgress)]);
        assert!(b.contains("build"));
        assert!(b.contains("todos:"));
    }

    #[test]
    fn bad_args_error() {
        let t = TodoWrite;
        assert!(t.execute(&serde_json::json!({})).is_err());
        assert!(t.execute(&serde_json::json!({"todos": "nope"})).is_err());
    }

    #[test]
    fn scoped_todo_keeps_its_own_list_and_never_touches_the_global() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Seed the process-global list (the user's top-level plan).
        set(vec![Todo::new("user-task", Status::InProgress)]);

        // A sub-agent's ScopedTodo writes its OWN plan.
        let sub = ScopedTodo::new();
        let ack = sub
            .execute(&serde_json::json!({"todos":[
                {"content":"sub-step-1","status":"in_progress"},
                {"content":"sub-step-2","status":"pending"}
            ]}))
            .unwrap();
        assert!(ack.contains("2 item"), "ack reflects the sub-agent's own list: {ack}");

        // The global list is UNTOUCHED — no leak between the sub-agent and the user's plan.
        let global = snapshot();
        assert_eq!(global.len(), 1, "global list unchanged by the sub-agent");
        assert_eq!(global[0].content, "user-task");

        // Two independent sub-agents don't share state (per-instance, not global).
        let other = ScopedTodo::new();
        let other_ack = other
            .execute(&serde_json::json!({"todos":[{"content":"x","status":"done"}]}))
            .unwrap();
        assert!(other_ack.contains("1 item"), "second sub-agent has its own list");
        // The first sub-agent's list is still its own 2 items.
        assert_eq!(sub.items.lock().unwrap().len(), 2);

        clear();
    }

    #[test]
    fn scoped_todo_rejects_bad_args() {
        let sub = ScopedTodo::new();
        assert!(sub.execute(&serde_json::json!({})).is_err());
        assert!(sub.execute(&serde_json::json!({"todos": 5})).is_err());
    }

    #[test]
    fn scoped_todo_and_todo_write_share_a_schema() {
        // Both accept the identical shape — a sub-agent that learned todo_write at top level uses
        // it unchanged.
        assert_eq!(TodoWrite.parameters(), ScopedTodo::new().parameters());
        assert_eq!(TodoWrite.name(), ScopedTodo::new().name());
    }
}
