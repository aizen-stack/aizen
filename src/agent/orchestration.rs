//! Live multi-agent orchestration status — the surface behind `/workflows`.
//!
//! Claude Code has `/workflows` to watch fan-out progress. Aizen already ran `task` + `workflow`
//! children with only transcript trace lines (`wf_trace`); this module keeps a process-global
//! registry so the user can open a status panel mid-turn and see who is running, who finished,
//! and how many sub-agent slots are free.
//!
//! Design notes:
//! - In-process state is the fast UI path; each run also publishes a redacted, atomic manifest under
//!   `~/.aizen/orchestration/runs/` so another process can show live activity.
//! - RAII [`Track`] marks the entry finished on drop if the caller forgot — panic/early-return safe.
//! - Recent history is capped so a long REPL session doesn't grow unbounded.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
    started_unix: u64,
    finished_unix: Option<u64>,
    parent: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RunManifest {
    schema: u32,
    run_id: String,
    pid: u32,
    kind: String,
    name: String,
    label: String,
    phase: String,
    detail: String,
    parent: Option<u64>,
    started_unix: u64,
    finished_unix: Option<u64>,
    updated_unix: u64,
}

const RUN_SCHEMA: u32 = 1;

fn manifest_root() -> PathBuf {
    crate::core::config::nextgen_home()
        .join("orchestration")
        .join("runs")
}

fn manifest_path(id: u64) -> PathBuf {
    manifest_root().join(format!("{}-{id}.json", std::process::id()))
}

fn manifest_lock(id: u64) -> PathBuf {
    crate::core::workspace_txn::store_lock("orchestration-run", &id.to_string())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn kind_name(kind: Kind) -> &'static str {
    match kind {
        Kind::Task => "task",
        Kind::Workflow => "workflow",
        Kind::WorkflowChild => "child",
    }
}

fn phase_name(phase: Phase) -> &'static str {
    match phase {
        Phase::Running => "running",
        Phase::Synthesizing => "synthesizing",
        Phase::Done => "done",
        Phase::Failed => "failed",
    }
}

fn safe_text(value: &str, max: usize) -> String {
    value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max)
        .collect()
}

fn persist_manifest(e: &Entry) {
    let path = manifest_path(e.id);
    let bytes = match serde_json::to_vec_pretty(&RunManifest {
        schema: RUN_SCHEMA,
        run_id: e.id.to_string(),
        pid: std::process::id(),
        kind: kind_name(e.kind).to_string(),
        name: safe_text(&e.name, 120),
        label: safe_text(&e.label, 120),
        phase: phase_name(e.phase).to_string(),
        detail: safe_text(&e.detail, 240),
        parent: e.parent,
        started_unix: e.started_unix,
        finished_unix: e.finished_unix,
        updated_unix: unix_now(),
    }) {
        Ok(bytes) => bytes,
        Err(_) => return,
    };
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &manifest_lock(e.id),
        Duration::from_secs(2),
    )
    .ok();
    if _lock.is_none() {
        return;
    }
    let _ = fs::create_dir_all(manifest_root());
    let _ = crate::core::persist::atomic_write_owner_only(&path, &bytes);
}

/// Best-effort removal of this run's manifest (called when the run reaches a terminal state — the
/// remote view only surfaces running/synthesizing rows, so a finished file is just dead weight).
fn remove_manifest(id: u64) {
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &manifest_lock(id),
        Duration::from_secs(1),
    )
    .ok();
    let _ = crate::core::persist::remove_if_exists(&manifest_path(id));
}

/// Manifests older than this since their last update are treated as crashed-process debris and swept
/// during a load. Generous so a long legitimate fan-out is never pruned out from under itself.
const MANIFEST_STALE_SECS: u64 = 6 * 3600;

fn load_remote_manifests() -> Vec<RunManifest> {
    let Ok(entries) = fs::read_dir(manifest_root()) else {
        return Vec::new();
    };
    let now = unix_now();
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let manifest = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<RunManifest>(&bytes).ok())
            .filter(|m| m.schema == RUN_SCHEMA);
        let Some(m) = manifest else {
            // Unparseable/foreign schema debris — sweep it so the dir can't grow unbounded.
            let _ = crate::core::persist::remove_if_exists(&path);
            continue;
        };
        // A crashed process leaves a "running" manifest forever; the owning process deletes its own
        // file on finish. Sweep anything that hasn't been touched within the stale window.
        if now.saturating_sub(m.updated_unix) > MANIFEST_STALE_SECS {
            let _ = crate::core::persist::remove_if_exists(&path);
            continue;
        }
        out.push(m);
    }
    out
}

fn remote_row(m: &RunManifest) -> String {
    let elapsed = m
        .finished_unix
        .unwrap_or_else(unix_now)
        .saturating_sub(m.started_unix);
    let mark = match m.phase.as_str() {
        "running" => "⋯",
        "synthesizing" => "✦",
        "done" => "✓",
        "failed" => "✗",
        _ => "?",
    };
    let label = if m.label.is_empty() || m.label == m.name {
        String::new()
    } else {
        format!(" ({})", m.label)
    };
    let detail = if m.detail.is_empty() {
        String::new()
    } else {
        format!(" — {}", m.detail)
    };
    format!(
        "  {mark} [{}] {}{}  · {}s{} · pid {}\n",
        m.kind, m.name, label, elapsed, detail, m.pid
    )
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn next_id() -> u64 {
    let seq = NEXT_ID.fetch_add(1, Ordering::Relaxed) & 0xffff_ffff;
    ((std::process::id() as u64) << 32) | seq
}

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
            if let Some(i) = self
                .live
                .iter()
                .position(|x| matches!(x.phase, Phase::Done | Phase::Failed))
            {
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
            e.finished_unix = Some(unix_now());
            // Terminal state → drop the cross-process manifest (the remote view only surfaces
            // running/synthesizing rows, so a finished file is dead weight and would otherwise
            // linger until the stale sweep).
            remove_manifest(e.id);
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
            persist_manifest(e);
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
        store()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .finish(self.id, Phase::Done, detail);
        self.finished = true;
    }

    /// Mark failure / error.
    pub fn finish_err(mut self, detail: impl Into<String>) {
        store()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .finish(self.id, Phase::Failed, detail);
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
            store().lock().unwrap_or_else(|e| e.into_inner()).finish(
                self.id,
                Phase::Failed,
                "aborted",
            );
        }
    }
}

fn start(
    kind: Kind,
    name: impl Into<String>,
    label: impl Into<String>,
    parent: Option<u64>,
) -> Track {
    let id = next_id();
    let now = unix_now();
    let e = Entry {
        id,
        kind,
        name: name.into(),
        label: label.into(),
        phase: Phase::Running,
        detail: String::new(),
        started: Instant::now(),
        finished: None,
        started_unix: now,
        finished_unix: None,
        parent,
    };
    persist_manifest(&e);
    store()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_live(e);
    Track {
        id,
        finished: false,
    }
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
pub fn start_workflow_child(
    parent: Option<u64>,
    task_id: impl Into<String>,
    label: impl Into<String>,
) -> Track {
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
    store().lock().unwrap_or_else(|e| e.into_inner()).live.len()
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

    if g.live.is_empty() && g.history.is_empty() && load_remote_manifests().is_empty() {
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

    let remote = load_remote_manifests()
        .into_iter()
        .filter(|m| {
            m.pid != std::process::id() && matches!(m.phase.as_str(), "running" | "synthesizing")
        })
        .collect::<Vec<_>>();
    if !remote.is_empty() {
        out.push_str("\n◎ other process(es)\n");
        for m in &remote {
            out.push_str(&remote_row(m));
        }
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
    let parent = e.parent.map(|p| format!(" ←#{p}")).unwrap_or_default();
    format!(
        "  {mark} [{tag}] {}{label}  · {elapsed}{detail}{parent}\n",
        e.name
    )
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
        assert!(
            s.contains("review") || s.contains("bugs") || s.contains("impl"),
            "{s}"
        );
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
            g.history
                .iter()
                .any(|e| e.id == id && e.phase == Phase::Failed),
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
