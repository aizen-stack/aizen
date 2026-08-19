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
    /// This run's OWN cancellation token, when the spawner armed one ([`Track::arm_stop`]).
    ///
    /// It is a `child()` of the turn token, never the turn token itself — that distinction is the
    /// whole point. Cancelling this stops one sub-agent; the turn, its parent workflow, and its
    /// siblings keep running. `None` for rows nobody offered a stop handle for.
    cancel: Option<crate::core::cancel::TurnCancel>,
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
    /// Who set this process running, when it was not a person at a terminal.
    ///
    /// Empty (and omitted from the file) for every ordinary run: a REPL turn, a one-shot `aizen`
    /// invocation, the daemon. It is filled only by [`set_origin`], and today the only caller is
    /// `aizen mcp serve`, which learns the name from the MCP handshake — so a fan-out that Claude
    /// Code asked for says `Claude Code` because Claude Code said so, not because anything guessed.
    ///
    /// Readers must treat it as a label, never as authority: it is a string a client chose for
    /// itself, so it may name a program that does not exist. It decides what a status view prints
    /// and nothing else.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    origin: String,
}

const RUN_SCHEMA: u32 = 1;

/* The origin, set at most once per process.
`OnceLock` rather than a mutex on purpose: a process serves one client for its whole life, so the
second caller is a bug rather than a change of mind, and a write that silently loses is a safer
failure than a run whose author changes halfway through a fan-out. */
static ORIGIN: OnceLock<String> = OnceLock::new();

/// Name whoever is driving this process, for every run it starts from here on.
///
/// Ignored if called twice — see [`ORIGIN`]. The name is trimmed and bounded because it arrives
/// over a wire from another program.
pub fn set_origin(who: &str) {
    let who = safe_text(who.trim(), 60);
    if who.is_empty() {
        return;
    }
    let _ = ORIGIN.set(who);
}

fn origin() -> String {
    ORIGIN.get().cloned().unwrap_or_default()
}

fn manifest_root() -> PathBuf {
    crate::core::config::aizen_home()
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
        origin: origin(),
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
        "  {mark} [{}] {}{}  · {}{} · pid {}\n",
        m.kind,
        m.name,
        label,
        fmt_secs(elapsed),
        detail,
        m.pid
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
            // Evict only a FINISHED row. Evicting a still-running one orphaned it: `finish` and
            // `set_phase` search `live` only, so the evicted run's eventual finish became a
            // silent no-op, its manifest was never removed, and other processes' `/workflows`
            // showed a phantom "running" row for the whole stale-sweep window. A fan-out at the
            // documented max (32 children + their parent) may briefly exceed the soft cap
            // instead — the cap is a display bound, not a correctness bound.
            if let Some(i) = self
                .live
                .iter()
                .position(|x| matches!(x.phase, Phase::Done | Phase::Failed))
            {
                let old = self.live.remove(i);
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

    /// Returns whether the row existed — the CALLER then drops the cross-process manifest,
    /// OUTSIDE the store lock. Manifest IO takes a 1–2s file lock; holding the store mutex
    /// across it stalled every reader (`/workflows`, the HUD chip, `cancel_matching`) for the
    /// duration of a contended write.
    fn finish(&mut self, id: u64, phase: Phase, detail: impl Into<String>) -> bool {
        let detail = detail.into();
        if let Some(pos) = self.live.iter().position(|e| e.id == id) {
            let mut e = self.live.remove(pos);
            e.phase = phase;
            if !detail.is_empty() {
                e.detail = detail;
            }
            e.finished = Some(Instant::now());
            e.finished_unix = Some(unix_now());
            self.push_history(e);
            return true;
        }
        false
    }

    /// Returns a snapshot of the updated row for the CALLER to persist — same reason as
    /// [`Store::finish`]: the manifest write must not happen under this lock.
    fn set_phase(&mut self, id: u64, phase: Phase, detail: impl Into<String>) -> Option<Entry> {
        let detail = detail.into();
        if let Some(e) = self.live.iter_mut().find(|e| e.id == id) {
            e.phase = phase;
            if !detail.is_empty() {
                e.detail = detail;
            }
            return Some(e.clone());
        }
        None
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
        let found =
            store()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .finish(self.id, Phase::Done, detail);
        if found {
            // Terminal state → drop the cross-process manifest, OUTSIDE the store lock (its file
            // lock can block for seconds; see `Store::finish`).
            remove_manifest(self.id);
        }
        self.finished = true;
    }

    /// Mark failure / error.
    pub fn finish_err(mut self, detail: impl Into<String>) {
        let found = store().lock().unwrap_or_else(|e| e.into_inner()).finish(
            self.id,
            Phase::Failed,
            detail,
        );
        if found {
            remove_manifest(self.id);
        }
        self.finished = true;
    }

    /// Update phase without finishing (e.g. workflow → synthesizing).
    pub fn set_phase(&self, phase: Phase, detail: impl Into<String>) {
        let snapshot = store()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .set_phase(self.id, phase, detail);
        if let Some(e) = snapshot {
            persist_manifest(&e); // outside the lock — see `Store::set_phase`
        }
    }

    /// Publish a stop handle for this run, so `/workflows stop <id>` can cancel it alone.
    ///
    /// The token MUST be a `TurnCancel::child()` of whatever the run inherited, never the inherited
    /// token itself — arming the turn token here would turn a request to stop one sub-agent into Esc.
    pub fn arm_stop(&self, token: crate::core::cancel::TurnCancel) {
        let mut g = store().lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = g.live.iter_mut().find(|e| e.id == self.id) {
            e.cancel = Some(token);
        }
    }
}

impl Drop for Track {
    fn drop(&mut self) {
        if !self.finished {
            let found = store().lock().unwrap_or_else(|e| e.into_inner()).finish(
                self.id,
                Phase::Failed,
                "aborted",
            );
            if found {
                remove_manifest(self.id);
            }
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
        cancel: None,
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

/// Format a run duration for a status row. Three tiers so the unit is always meaningful at a glance:
/// `42s`, `7m03s`, `2h05m`. Without the hour tier a long fan-out read `125m00s` — correct, but the
/// reader has to do the division to learn it has been going for two hours.
pub(crate) fn fmt_secs(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

fn fmt_elapsed(started: Instant, finished: Option<Instant>) -> String {
    let end = finished.unwrap_or_else(Instant::now);
    fmt_secs(end.saturating_duration_since(started).as_secs())
}

/// The short, typeable form of a run id. Ids embed the pid in their high 32 bits (so two aizen
/// windows never collide in a shared manifest dir), which makes the full value a 20-digit number
/// nobody will retype into a stop command. The low half is a per-process counter starting at 1.
fn short_handle(id: u64) -> String {
    format!("#{}", id & 0xffff_ffff)
}

/// Outcome of a [`cancel_matching`] request.
pub struct CancelReport {
    /// Live rows whose own token was cancelled.
    pub cancelled: usize,
    /// Live rows that matched but published no stop handle — reported separately so the caller can
    /// say "matched but can't be stopped" instead of the misleading "nothing matched".
    pub unstoppable: usize,
}

/// Cancel the live runs matching `needle`, which is either a short handle (`#3`, `3`) or a
/// case-insensitive substring of a row's name or label (`t1`, `reviewer`, `parser`).
///
/// Only the matched runs stop: each row's token is a child of the turn's, so the orchestrating turn
/// and every sibling keep going. This is the gap Esc could not fill — Esc cancels the whole turn.
pub fn cancel_matching(needle: &str) -> CancelReport {
    let needle = needle.trim();
    let as_num = needle.trim_start_matches('#').parse::<u64>().ok();
    let lower = needle.to_lowercase();
    let mut tokens = Vec::new();
    let mut unstoppable = 0usize;
    {
        let g = store().lock().unwrap_or_else(|e| e.into_inner());
        for e in g.live.iter().filter(|e| {
            match as_num {
                // A numeric needle is an EXACT id match (short or full form). Never substring: `#3`
                // must not also stop `#30`.
                Some(n) => e.id == n || (e.id & 0xffff_ffff) == n,
                None => {
                    !lower.is_empty()
                        && (e.name.to_lowercase().contains(&lower)
                            || e.label.to_lowercase().contains(&lower))
                }
            }
        }) {
            match &e.cancel {
                Some(t) => tokens.push(t.clone()),
                None => unstoppable += 1,
            }
        }
    }
    // Cancel outside the store lock: `cancel()` walks the token's own child tree and takes its locks,
    // and a stopped child's loop immediately reaches for the store to mark itself finished.
    let cancelled = tokens.len();
    for t in tokens {
        t.cancel();
    }
    CancelReport {
        cancelled,
        unstoppable,
    }
}

/// Names that open the status panel. One source of truth: the REPL's slash dispatch matches on this,
/// and the input thread uses it to recognise a mid-turn stop request before the command is queued.
pub fn is_status_command(name: &str) -> bool {
    matches!(name, "workflows" | "workflow" | "wf" | "agents-status")
}

/// Interpret a `/workflows` argument as a stop request and carry it out, returning the line to show.
/// `None` means it wasn't a stop request and the caller should open the panel instead.
///
/// Parsing AND execution live here together so the two call sites — the REPL's slash handler (idle)
/// and the input thread (mid-turn, where the submission queue is not being drained) — cannot drift
/// into reporting the same action differently.
pub fn try_stop_command(arg: &str) -> Option<String> {
    let mut words = arg.split_whitespace();
    let verb = words.next()?.to_ascii_lowercase();
    if !matches!(verb.as_str(), "stop" | "kill" | "cancel") {
        return None;
    }
    let Some(needle) = words.next() else {
        return Some("usage: /workflows stop <#id|name>  (ids are shown in the panel)".to_string());
    };
    let report = cancel_matching(needle);
    Some(if report.cancelled > 0 {
        format!(
            "✓ stop: requested for {} run(s) matching {needle:?} — siblings keep running",
            report.cancelled
        )
    } else if report.unstoppable > 0 {
        // Matched, but the spawner published no handle. Say that rather than "not found", which would
        // send the user hunting for a typo that isn't there.
        format!(
            "stop: {} matching run(s) have no stop handle (marked `no-stop`) — Esc cancels the whole turn",
            report.unstoppable
        )
    } else {
        format!("stop: no running row matches {needle:?} — open /workflows for live ids")
    })
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
        "\n\nStop ONE run: `/workflows stop #<id>` (or a name fragment) — Esc cancels the whole turn\n\
         Tips: `task` = one sub-agent · `workflow` = parallel fan-out + synthesize/verify\n\
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
        .map(|p| format!(" ←{}", short_handle(p)))
        .unwrap_or_default();
    // The handle leads the row: it is the argument to `/workflows stop`, and a stop command is only
    // usable if the thing you type is visible next to the thing you want to stop. Rows with no stop
    // handle armed are marked so the panel never advertises a control that would not work.
    let handle = short_handle(e.id);
    let stoppable = match (e.phase, e.cancel.is_some()) {
        (Phase::Running | Phase::Synthesizing, false) => " ·no-stop",
        _ => "",
    };
    format!(
        "  {mark} {handle}{stoppable} [{tag}] {}{label}  · {elapsed}{detail}{parent}\n",
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

    /* The origin field must be invisible to everything that existed before it.
    Two directions, both load-bearing: a manifest written by an older aizen (or by any ordinary
    run today) has no `origin` key and must still parse, and a run with no origin must still
    SERIALISE without the key — otherwise every `/workflows` row and every desktop office pane on
    an older reader gains a field it was not built to ignore, for a value that is always empty. */
    #[test]
    fn a_manifest_without_an_origin_round_trips_unchanged() {
        let old = r#"{"schema":1,"run_id":"7","pid":42,"kind":"task","name":"n","label":"l",
            "phase":"running","detail":"","parent":null,"started_unix":1,"finished_unix":null,
            "updated_unix":2}"#;
        let m: RunManifest = serde_json::from_str(old).expect("an older manifest still parses");
        assert!(m.origin.is_empty());
        let back = serde_json::to_string(&m).unwrap();
        assert!(
            !back.contains("origin"),
            "an ordinary run writes the same file it always did"
        );
    }

    #[test]
    fn a_manifest_carries_the_name_the_client_gave() {
        let new = r#"{"schema":1,"run_id":"7","pid":42,"kind":"workflow","name":"n","label":"l",
            "phase":"running","detail":"","parent":null,"started_unix":1,"finished_unix":null,
            "updated_unix":2,"origin":"Claude Code"}"#;
        let m: RunManifest = serde_json::from_str(new).unwrap();
        assert_eq!(m.origin, "Claude Code");
        assert!(serde_json::to_string(&m).unwrap().contains("Claude Code"));
    }

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
    fn elapsed_reaches_for_hours_before_the_minute_count_gets_absurd() {
        assert_eq!(fmt_secs(0), "0s");
        assert_eq!(fmt_secs(59), "59s");
        assert_eq!(fmt_secs(60), "1m00s");
        assert_eq!(fmt_secs(3599), "59m59s");
        // The reason this tier exists: a two-hour fan-out used to read `120m00s`.
        assert_eq!(fmt_secs(7200), "2h00m");
        assert_eq!(fmt_secs(7 * 3600 + 5 * 60 + 9), "7h05m");
    }

    #[test]
    fn a_row_shows_the_handle_you_would_type_to_stop_it() {
        let t = start_task("coder · rewrite the parser");
        let handle = short_handle(t.id());
        let s = format_status();
        assert!(
            s.contains(&handle),
            "the stop handle must be visible next to the row; got:\n{s}"
        );
        // No token armed yet → the panel must not advertise a stop that would fail.
        assert!(s.contains("·no-stop"), "unarmed row marked no-stop:\n{s}");
        t.arm_stop(crate::core::cancel::TurnCancel::new());
        t.finish_ok("done");
    }

    #[test]
    fn stop_by_handle_cancels_only_that_row() {
        let turn = crate::core::cancel::TurnCancel::new();
        let a = start_task("coder · task A");
        let b = start_task("coder · task B");
        let (tok_a, tok_b) = (turn.child(), turn.child());
        a.arm_stop(tok_a.clone());
        b.arm_stop(tok_b.clone());

        let report = cancel_matching(&short_handle(a.id()));

        assert_eq!(report.cancelled, 1, "exactly the matched row");
        assert_eq!(report.unstoppable, 0);
        assert!(tok_a.is_cancelled(), "target stops");
        assert!(!tok_b.is_cancelled(), "sibling keeps running");
        assert!(!turn.is_cancelled(), "the turn keeps running");
        a.finish_err("stopped");
        b.finish_ok("done");
    }

    #[test]
    fn a_numeric_handle_never_matches_by_prefix() {
        // `#3` must not also stop `#30`: ids are exact, only names are substring-matched.
        let t = start_task("coder · exact match check");
        let armed = crate::core::cancel::TurnCancel::new();
        t.arm_stop(armed.clone());
        let short = t.id() & 0xffff_ffff;
        let report = cancel_matching(&format!("{}", short * 10 + 7));
        assert_eq!(report.cancelled, 0, "a longer number is a different run");
        assert!(!armed.is_cancelled());
        t.finish_ok("done");
    }

    #[test]
    fn a_matched_row_with_no_handle_is_reported_separately() {
        // Otherwise "0 cancelled" reads as "you typed the id wrong".
        let t = start_task("planner · unarmed row");
        let report = cancel_matching(&short_handle(t.id()));
        assert_eq!(report.cancelled, 0);
        assert_eq!(report.unstoppable, 1);
        t.finish_ok("done");
    }

    #[test]
    fn only_a_stop_verb_is_intercepted_as_a_stop() {
        // The input thread routes on this while a turn runs: anything that is NOT a stop must fall
        // through to "open the panel", and a bare `/workflows` must never be read as a stop.
        assert!(try_stop_command("").is_none());
        assert!(try_stop_command("   ").is_none());
        assert!(try_stop_command("tree").is_none(), "unknown verb → panel");

        for verb in ["stop", "kill", "cancel", "STOP", "Kill"] {
            let note = try_stop_command(verb).expect("recognised as a stop request");
            assert!(note.starts_with("usage:"), "no target named: {note}");
        }
    }

    #[test]
    fn status_command_aliases_match_the_repl_dispatch() {
        // Both surfaces route on this one predicate; a drift here would make the mid-turn panel open
        // for `/workflows` but not for `/wf`.
        for name in ["workflows", "workflow", "wf", "agents-status"] {
            assert!(is_status_command(name), "{name} must be recognised");
        }
        assert!(!is_status_command("work"), "/work is a different command");
        assert!(!is_status_command("team"));
    }

    #[test]
    fn empty_status_is_helpful() {
        // Can't guarantee empty process-wide, but format_status must always be non-empty & mention multi-agent.
        let s = format_status();
        assert!(s.contains("Multi-agent"));
        assert!(s.contains("slots"));
    }
}
