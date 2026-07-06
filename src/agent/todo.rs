//! In-session task list (the `todo_write` tool) — makes multi-step work visibly tracked, the way
//! Claude Code's todo list is one of its most legible features. Pure in-memory, zero infra: a
//! process-global list the model REPLACES each call (send-the-whole-list semantics), rendered as a
//! checklist into the scroll region + summarised in the status bar. Reset on `/clear`.

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
}

fn default_status() -> Status {
    Status::Pending
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

/// Parse a `{"todos": [...]}` args object into a `Vec<Todo>` — shared by the top-level `todo_write`
/// and the sub-agent-scoped `ScopedTodo`, so both accept exactly the same shape.
fn parse_todos(args: &serde_json::Value) -> Result<Vec<Todo>> {
    match args.get("todos") {
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| anyhow::anyhow!("invalid `todos` (expect [{{content, status}}]): {e}")),
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
                        "status": {"type": "string", "enum": ["pending", "in_progress", "done"]}
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
         (it REPLACES the previous one). Use it to plan 3+ step work and to mark progress: set ONE \
         item to in_progress at a time, flip it to done before starting the next. Not for trivial \
         one-step tasks. The list is shown to the user and resets on /clear."
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
        // Show the checklist to the USER (scroll region above the pinned box, or stderr).
        let block = render_block(&items);
        if !block.is_empty() {
            if crate::ui::tui::active() {
                crate::ui::tui::emit_line(&block);
            } else {
                eprintln!("{block}");
            }
        }
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
        Self { items: Mutex::new(Vec::new()) }
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
         flip it to done before starting the next. Use it to keep a 3+ step task on track; skip it \
         for trivial one- or two-step work. This is your private scratch plan — it is not shown to \
         anyone and is discarded when you return your result."
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

/// Serializes tests that touch the process-global TODOS list (also used by `agent::tests` for the
/// recitation-reminder loop test).
#[cfg(test)]
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
            Todo { content: "a".into(), status: Status::Done },
            Todo { content: "b".into(), status: Status::InProgress },
            Todo { content: "c".into(), status: Status::Pending },
        ]);
        assert_eq!(status_summary().as_deref(), Some("☑ 1/3"));
        assert_eq!(snapshot().len(), 3);
        clear();
        assert!(status_summary().is_none());
        assert!(snapshot().is_empty());
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
    fn render_block_is_empty_when_no_todos() {
        assert!(render_block(&[]).is_empty());
        let b = render_block(&[Todo { content: "build".into(), status: Status::InProgress }]);
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
        set(vec![Todo { content: "user-task".into(), status: Status::InProgress }]);

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
        let other_ack = other.execute(&serde_json::json!({"todos":[{"content":"x","status":"done"}]})).unwrap();
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
        // Both accept the identical `{todos:[{content,status}]}` shape — a sub-agent that learned
        // todo_write at top level uses it unchanged.
        assert_eq!(TodoWrite.parameters(), ScopedTodo::new().parameters());
        assert_eq!(TodoWrite.name(), ScopedTodo::new().name());
    }
}
