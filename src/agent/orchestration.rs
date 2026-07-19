//! Live multi-agent orchestration status — the surface behind `/workflows`.
//!
//! Claude Code has `/workflows` to watch fan-out progress. Aizen already ran `task` + `workflow`
//! children with only transcript trace lines (`wf_trace`); this module keeps a process-global
//! registry so the user can open a status panel mid-turn and see who is running, who finished,
//! and how many sub-agent slots are free.
//!
//! Design notes:
//! - Pure in-process state (no disk, no network). Cleared only by process exit.
//! - RAII [`Track`] marks the entry finished on drop if the caller forgot — panic/early-return safe.
//! - Recent history is capped so a long REPL session doesn't grow unbounded.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

/// Cap on finished entries retained for `/workflows` history.
const HISTORY_CAP: usize = 16;
/// Cap on concurrent live entries (workflow + children + tasks). Soft — over-cap still records.
const LIVE_SOFT_CAP: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Top-level `task` tool dispatch (one sub-agent).
    Task,
    /// Parent `workflow` fan-out / verify call.
    Workflow,
    /// One child inside a workflow.
    WorkflowChild,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Running,
    Synthesizing,
    Done,
    Failed,
}

#[derive(Debug, Clone)]
struct Entry {
    id: u64,
    kind: Kind,
    /// Workflow name, or task role/agent label.
    name: String,
    /// Extra label (role, agent slug, child id).
    label: String,
    phase: Phase,
    /// Free-form detail ("3 step(s)", "error: …").
    detail: String,
    started: Instant,
    finished: Option<Instant>,
    parent: Option<u64>,
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn store() -> &'static Mutex<Store> {
    static S: OnceLock<Mutex<Store>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(Store::default()))
}

#[derive(Default)]
struct Store {
    live: Vec<Entry>,
    history: Vec<Entry>,
}

impl Store {
    fn push_live(&mut self, e: Entry) {
        if self.live.len() >= LIVE_SOFT_CAP {
            // Drop the oldest finished-looking live row if any; else the oldest.
            if let Some(i) = self.live.iter().position(|x| matches!(x.phase, Phase::Done | Phase::Failed)) {
                let old = self.live.remove(i);
                self.push_history(old);
            } else if !self.live.is_empty() {
                let old = self.live.remove(0);
                self.push_history(old);
            }
        }
        self.live.push(e);
    }

    fn push_history(&mut self, e: Entry) {
        self.history.push(e);
        if self.history.len() > HISTORY_CAP {
            let n = self.history.len() - HISTORY_CAP;
            self.history.drain(0..n);
        }
    }

    fn finish(&mut self, id: u64, phase: Phase, detail: impl Into<String>) {
        let detail = detail.into();
        if let Some(pos) = self.live.iter().position(|e| e.id == id) {
            let mut e = self.live.remove(pos);
            e.phase = phase;
            if !detail.is_empty() {
                e.detail = detail;
            }
            e.finished = Some(Instant::now());
            self.push_history(e);
        }
    }

    fn set_phase(&mut self, id: u64, phase: Phase, detail: impl Into<String>) {
        let detail = detail.into();
        if let Some(e) = self.live.iter_mut().find(|e| e.id == id) {
            e.phase = phase;
            if !detail.is_empty() {
                e.detail = detail;
            }
        }
    }
}

/// RAII handle for one tracked orchestration unit. Dropping without [`Track::finish`] marks it
/// `Failed` with detail `"aborted"` so a panic mid-fan-out doesn't leave a ghost "running" row.
pub struct Track {
    id: u64,
    finished: bool,
}

impl Track {
    pub fn id(&self) -> u64 {
        self.id
    }

    /// Mark successful completion (or a terminal non-error status like `max-iters` still "done-ish").
    pub fn finish_ok(mut self, detail: impl Into<String>) {
        store().lock().unwrap_or_else(|e| e.into_inner()).finish(self.id, Phase::Done, detail);
        self.finished = true;
    }

    /// Mark failure / error.
    pub fn finish_err(mut self, detail: impl Into<String>) {
        store().lock().unwrap_or_else(|e| e.into_inner()).finish(self.id, Phase::Failed, detail);
        self.finished = true;
    }

    /// Update phase without finishing (e.g. workflow → synthesizing).
    pub fn set_phase(&self, phase: Phase, detail: impl Into<String>) {
        store()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_phase(self.id, phase, detail);
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        if !self.finished {
            store()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .finish(self.id, Phase::Failed, "aborted");
        }
    }
}

fn start(kind: Kind, name: impl Into<String>, label: impl Into<String>, parent: Option<u64>) -> Track {
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let e = Entry {
        id,
        kind,
        name: name.into(),
        label: label.into(),
        phase: Phase::Running,
        detail: String::new(),
        started: Instant::now(),
        finished: None,
        parent,
    };
    store().lock().unwrap_or_else(|e| e.into_inner()).push_live(e);
    Track { id, finished: false }
}

/// Begin tracking a top-level `task` dispatch.
pub fn start_task(label: impl Into<String>) -> Track {
    let label = label.into();
    start(Kind::Task, label.clone(), label, None)
}

/// Begin tracking a `workflow` parent (fanout / verify).
pub fn start_workflow(name: impl Into<String>, n_tasks: usize) -> Track {
    let name = name.into();
    start(Kind::Workflow, name, format!("{n_tasks} task(s)"), None)
}

/// Begin tracking one child inside a workflow.
pub fn start_workflow_child(parent: Option<u64>, task_id: impl Into<String>, label: impl Into<String>) -> Track {
    start(Kind::WorkflowChild, task_id.into(), label.into(), parent)
}

/// How many sub-agent slots are currently held (process-global gate).
pub fn gate_active() -> usize {
    crate::agent::task_tool::active_subagents()
}

/// Configured concurrent sub-agent cap.
pub fn gate_cap() -> usize {
    crate::agent::task_tool::max_parallel_subagents_pub()
}

/// Number of live (not-yet-finished) orchestration entries.
pub fn live_count() -> usize {
    store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .live
        .len()
}

fn fmt_elapsed(started: Instant, finished: Option<Instant>) -> String {
    let end = finished.unwrap_or_else(Instant::now);
    let secs = end.saturating_duration_since(started).as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

fn phase_mark(p: Phase) -> &'static str {
    match p {
        Phase::Running => "⋯",
        Phase::Synthesizing => "✦",
        Phase::Done => "✓",
        Phase::Failed => "✗",
    }
}

fn kind_tag(k: Kind) -> &'static str {
    match k {
        Kind::Task => "task",
        Kind::Workflow => "workflow",
        Kind::WorkflowChild => "child",
    }
}

/// Human-readable multi-agent status for `/workflows` (and pure-print overlays).
pub fn format_status() -> String {
    let g = store().lock().unwrap_or_else(|e| e.into_inner());
    let active = gate_active();
    let cap = gate_cap();
    let mut out = String::new();
    out.push_str(&format!(
        "Multi-agent  ·  slots {active}/{cap}  ·  live {}\n",
        g.live.len()
    ));

    if g.live.is_empty() && g.history.is_empty() {
        out.push_str(
            "\n  (no task/workflow activity this process yet)\n\
             \n  The model launches multi-agent work via the `task` and `workflow` tools\n\
             (or `aizen workflow <spec.json>`). Ultimate mode prefers workflows for fan-out.\n\
             Open /workflows again while a fan-out is running to watch live children.",
        );
        return out;
    }

    if !g.live.is_empty() {
        out.push_str("\n● running\n");
        for e in &g.live {
            out.push_str(&format_row(e));
        }
    } else {
        out.push_str("\n○ nothing running right now\n");
    }

    if !g.history.is_empty() {
        out.push_str("\n· recent\n");
        // Newest last in history vec → show newest first.
        for e in g.history.iter().rev().take(HISTORY_CAP) {
            out.push_str(&format_row(e));
        }
    }

    out.push_str(
        "\n\nTips: `task` = one sub-agent · `workflow` = fan-out ≤5 + synthesize/verify\n\
         CLI: `aizen workflow <spec.json>` · specialists: `aizen agents list`",
    );
    out
}

fn format_row(e: &Entry) -> String {
    let mark = phase_mark(e.phase);
    let tag = kind_tag(e.kind);
    let elapsed = fmt_elapsed(e.started, e.finished);
    let label = if e.label.is_empty() || e.label == e.name {
        String::new()
    } else {
        format!(" ({})", e.label)
    };
    let detail = if e.detail.is_empty() {
        String::new()
    } else {
        format!(" — {}", e.detail)
    };
    let parent = e
        .parent
        .map(|p| format!(" ←#{p}"))
        .unwrap_or_default();
    format!("  {mark} [{tag}] {}{label}  · {elapsed}{detail}{parent}\n", e.name)
}

/// Compact one-liner for the HUD / status bar when agents are in flight.
pub fn hud_chip() -> Option<String> {
    let n = live_count();
    if n == 0 {
        return None;
    }
    let active = gate_active();
    let cap = gate_cap();
    Some(format!("agents {n} · slots {active}/{cap}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn track_lifecycle_and_format() {
        // Isolate: this process may already have history from other tests; just assert our rows appear.
        let w = start_workflow("review", 2);
        let wid = w.id();
        let c1 = start_workflow_child(Some(wid), "bugs", "reviewer");
        let c2 = start_workflow_child(Some(wid), "impl", "coder");
        assert!(live_count() >= 3);
        c1.finish_ok("done [3 step(s)]");
        c2.finish_err("error");
        w.set_phase(Phase::Synthesizing, "merging");
        w.finish_ok("synthesized");
        let s = format_status();
        assert!(s.contains("Multi-agent"), "{s}");
        assert!(s.contains("review") || s.contains("bugs") || s.contains("impl"), "{s}");
        assert!(s.contains("slots"), "{s}");
    }

    #[test]
    fn drop_marks_aborted() {
        let id = {
            let t = start_task("planner");
            t.id()
        }; // drop without finish
        let s = format_status();
        // History should mention aborted (or the live list shouldn't still show running for that id).
        let g = store().lock().unwrap();
        assert!(
            g.live.iter().all(|e| e.id != id),
            "dropped track must leave live"
        );
        assert!(
            g.history.iter().any(|e| e.id == id && e.phase == Phase::Failed),
            "drop → Failed in history; status was:\n{s}"
        );
    }

    #[test]
    fn empty_status_is_helpful() {
        // Can't guarantee empty process-wide, but format_status must always be non-empty & mention multi-agent.
        let s = format_status();
        assert!(s.contains("Multi-agent"));
        assert!(s.contains("slots"));
    }
}
