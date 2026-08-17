//! Background process pool (the `process` tool). `shell_run` is FOREGROUND with a wall-clock kill
//! (120s by default) — useless for a `npm run dev`, a watcher, or a long build. `process` spawns a
//! command in the BACKGROUND, returns a `proc_<n>` handle immediately, and lets the agent
//! poll/log/wait/kill it later. Pure-Rust over `std::process` + drain threads (the same lossy-decode +
//! `chcp 65001` posture as `shell_run`); NO Python reader-threads, NO docker/ssh/modal backends —
//! local only.
//!
//! Safety: the command runs CONFINED to the cwd root (same `confine` jail as `shell_run`), and the
//! hard `cmd_guard` floor is applied to `action=start` in `agent::execute_one` BEFORE approval — so a
//! background `rm -rf /` is refused exactly like a foreground one.
//!
//! SCOPING: every handle belongs to the [`crate::core::exec_ctx::ExecutionContext::resource_scope`]
//! that created it, and the pool is only visible within that scope. The tool is granted to `coder`
//! and `tester` sub-agents (a timed-out `shell_run` tells them to come here), and those children run
//! concurrently on one shared worktree — a global view would let one child `kill` a sibling's dev
//! server, or read a build log it never started. A foreign handle reports the same "no such process"
//! as an unknown one, so a sibling's existence does not leak either. [`kill_all`] stays scope-blind:
//! process exit must reap everything.
//!
//! Lifetime: every process is spawned into a [`crate::core::proctree`] containment (Windows job
//! object / Unix process group), so `kill` reaps the whole tree and — on Windows — an Aizen crash
//! reaps it too, because the kernel closes the job handle. A dev server started here cannot outlive
//! the CLI holding its port.

use crate::agent::builtin::confine;
use crate::agent::tools::{Tool, WorkspaceEffect};
use anyhow::{bail, Context, Result};
use once_cell::sync::Lazy;
use serde_json::Value;
use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// Max concurrent/retained processes; a new `start` over the cap prunes the oldest FINISHED one.
/// Deliberately GLOBAL rather than per-scope: it bounds real OS resources, and a fan-out of children
/// each entitled to its own 16 slots is exactly the runaway this guards.
const MAX_PROCESSES: usize = 16;
/// Rolling merged stdout+stderr buffer per process (bytes); older output is dropped from the front.
const OUTPUT_CAP: usize = 200 * 1024;

/// A bounded byte buffer: appends keep only the most-recent `cap` bytes (a coarse ring).
///
/// `start_offset` is the ABSOLUTE position of `bytes[0]` in the process's whole output stream, so it
/// keeps counting up as the front is dropped. That is what makes an incremental read possible: a
/// caller holding a cursor can be handed only the bytes it has not seen, and can be told exactly how
/// many were evicted before it came back — instead of re-reading the full 200 KiB ring every poll.
struct RingBuf {
    bytes: Vec<u8>,
    cap: usize,
    start_offset: u64,
}

/// One incremental read of a [`RingBuf`], as produced by [`RingBuf::since`].
struct LogDelta {
    /// The new bytes, lossily decoded (same posture as `shell_run`: keep the ASCII structure rather
    /// than dropping a whole non-UTF-8 log).
    text: String,
    /// Cursor to pass back next time.
    next_cursor: u64,
    /// Bytes that existed after the caller's cursor but were already evicted from the ring.
    dropped_before: u64,
    /// The cursor pointed past the end of the stream (a stale handle, or a caller's arithmetic slip).
    clamped: bool,
}

impl RingBuf {
    fn new(cap: usize) -> Self {
        RingBuf {
            bytes: Vec::new(),
            cap,
            start_offset: 0,
        }
    }
    fn push(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        if self.bytes.len() > self.cap {
            let overflow = self.bytes.len() - self.cap;
            self.bytes.drain(..overflow);
            self.start_offset += overflow as u64;
        }
    }
    /// Absolute offset one past the last retained byte — the cursor a caller resumes from.
    fn end_offset(&self) -> u64 {
        self.start_offset + self.bytes.len() as u64
    }
    /// The whole retained buffer (the no-cursor read).
    fn text(&self) -> String {
        let body = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.start_offset > 0 {
            format!("…[{} earlier bytes dropped]…\n{body}", self.start_offset)
        } else {
            body
        }
    }
    /// Only the bytes at or after `cursor`. A cursor below the retained window reports how much was
    /// lost rather than silently presenting a gap as continuous output; a cursor past the end is
    /// clamped and flagged rather than being an error (a process can be re-read after it exits).
    fn since(&self, cursor: u64) -> LogDelta {
        let end = self.end_offset();
        if cursor > end {
            return LogDelta {
                text: String::new(),
                next_cursor: end,
                dropped_before: 0,
                clamped: true,
            };
        }
        let dropped_before = self.start_offset.saturating_sub(cursor);
        let from = cursor.max(self.start_offset);
        let idx = (from - self.start_offset) as usize;
        LogDelta {
            text: String::from_utf8_lossy(&self.bytes[idx..]).into_owned(),
            next_cursor: end,
            dropped_before,
            clamped: false,
        }
    }
}

/// Set the exit flag and wake every [`Process`] `wait` blocked on this entry. Called from the monitor
/// thread when the child exits, and from [`kill_tree`] so a kill wakes its waiter at once instead of
/// letting it sit out the remaining timeout.
fn signal_done(notify: &(Mutex<bool>, Condvar)) {
    let mut flag = notify.0.lock().unwrap_or_else(|e| e.into_inner());
    *flag = true;
    notify.1.notify_all();
}

struct ProcEntry {
    command: String,
    pid: u32,
    started: Instant,
    /// Execution scope that started this process; only that scope can see or touch the handle.
    scope: String,
    out: Arc<Mutex<RingBuf>>,
    exit: Arc<Mutex<Option<i32>>>,
    done: Arc<AtomicBool>,
    /// Exit notification. The `done` atomic stays for cheap non-blocking reads (`status`, `list`);
    /// this pair is what lets `wait` BLOCK until exit instead of polling on a sleep.
    notify: Arc<(Mutex<bool>, Condvar)>,
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    finished_at: Arc<Mutex<Option<Instant>>>,
    /// Job object / process group holding the whole tree. Kept for the entry's lifetime: on Windows
    /// this is also crash insurance — if Aizen dies without running `kill_all`, the OS closes the job
    /// handle and the kernel reaps every descendant, so a `npm run dev` cannot outlive the CLI.
    containment: Arc<crate::core::proctree::Containment>,
    /// Sandbox bookkeeping: finishes the audit line on exit/kill and keeps the run's private temp
    /// directory alive for as long as the process may use it.
    sandbox: Arc<Mutex<crate::sandbox::runner::SpawnGuard>>,
}

impl ProcEntry {
    fn status_label(&self) -> String {
        if self.done.load(Ordering::Relaxed) {
            match *self.exit.lock().unwrap_or_else(|e| e.into_inner()) {
                Some(code) => format!("exited({code})"),
                None => "exited(?)".to_string(),
            }
        } else {
            "running".to_string()
        }
    }
}

static REGISTRY: Lazy<Mutex<HashMap<String, ProcEntry>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// The execution scope of the CALLING tool body. Read per call, not per tool instance: the executor
/// seeds the context into a thread-local INSIDE the `spawn_blocking` closure, so one registered tool
/// serves the top-level turn and every delegated child, each seeing only its own handles.
fn caller_scope() -> String {
    crate::core::exec_ctx::current()
        .map(|c| c.resource_scope())
        .unwrap_or_else(|| "default".to_string())
}

/// Resolve a handle the caller OWNS. A handle from another scope is reported exactly like an unknown
/// id — a sibling dispatch must not learn that another child's process exists.
fn lookup<'a>(reg: &'a HashMap<String, ProcEntry>, id: &str, scope: &str) -> Result<&'a ProcEntry> {
    match reg.get(id) {
        Some(e) if e.scope == scope => Ok(e),
        _ => bail!("no such process '{id}'"),
    }
}

fn elapsed_label(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

/// Render an incremental read: a `next_cursor` the caller feeds back, then only the new bytes. Any
/// eviction or clamp is stated inline, so an incremental view is never mistaken for a complete one.
fn render_delta(id: &str, status: &str, cursor: u64, d: &LogDelta) -> String {
    let mut head = format!("{id} [{status}] next_cursor={}", d.next_cursor);
    if d.clamped {
        head.push_str(&format!(
            " (cursor {cursor} is past the end of this output — nothing new)"
        ));
    }
    if d.dropped_before > 0 {
        head.push_str(&format!(
            " ({} bytes after your cursor were dropped from the retained buffer)",
            d.dropped_before
        ));
    }
    let body = d.text.trim_end();
    if body.is_empty() {
        format!("{head}\nno new output since cursor {cursor}")
    } else {
        format!("{head}\n{body}")
    }
}

/// Spawn `command` in the background, confined to `root` (optionally a `cwd` subdir), owned by
/// `scope`. Returns the new `proc_<n>` id.
fn start(
    root: &PathBuf,
    command: &str,
    cwd: Option<&str>,
    scope: &str,
    network: bool,
) -> Result<String> {
    let dir = match cwd {
        Some(c) => confine(root, c, true)?,
        None => root.clone(),
    };

    // Prune a finished slot if we're at the cap; refuse if everything is still running. Scope-blind
    // on purpose: the cap counts live OS processes, not per-agent entitlements. A finished entry is
    // reclaimable whoever started it — its output has already been reported to its own scope.
    {
        let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        if reg.len() >= MAX_PROCESSES {
            let victim = reg
                .iter()
                .filter(|(_, e)| e.done.load(Ordering::Relaxed))
                .min_by_key(|(_, e)| e.finished_at.lock().map(|g| *g).unwrap_or(None))
                .map(|(k, _)| k.clone());
            match victim {
                Some(k) => {
                    reg.remove(&k);
                }
                None => bail!(
                    "process pool full ({MAX_PROCESSES} running) — kill one first (process action=kill)"
                ),
            }
        }
    }

    // One construction path with `shell_run`: the sandbox runner wraps the shell, scrubs the
    // environment and applies policy — going background must never sidestep the sandbox.
    let mut sbx = crate::sandbox::runner::prepare_std(
        crate::sandbox::request::SandboxRequest::shell(
            crate::sandbox::CommandOrigin::ProcessStart,
            command,
            dir.clone(),
            root.clone(),
        )
        .network(network)
        .background(true),
    )?;
    sbx.command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Contain the tree at spawn. A background command is the WORST case for the orphan problem this
    // guards: `process start` is what runs dev servers and watchers, i.e. exactly the processes that
    // spawn real children behind the `cmd.exe`/`sh` wrapper and keep holding a port after a kill.
    let mut child = match sbx.command.spawn() {
        Ok(c) => c,
        Err(e) => {
            sbx.finish(crate::sandbox::runner::Outcome::SpawnFailed);
            return Err(e).with_context(|| format!("spawning background `{command}`"));
        }
    };
    let containment = Arc::new(sbx.contain(&child));
    let sandbox = Arc::new(Mutex::new(sbx.into_guard()));
    let pid = child.id();

    let out = Arc::new(Mutex::new(RingBuf::new(OUTPUT_CAP)));
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let stdin = child.stdin.take();
    spawn_drain(out_pipe, Arc::clone(&out));
    spawn_drain(err_pipe, Arc::clone(&out));

    let exit = Arc::new(Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));
    let notify = Arc::new((Mutex::new(false), Condvar::new()));
    let finished_at = Arc::new(Mutex::new(None));
    let child = Arc::new(Mutex::new(child));

    // Monitor: reap the child when it exits, recording the code + done flag (no busy CPU), then wake
    // any `wait`. `done` is stored BEFORE the notify so a woken waiter always observes the exit.
    {
        let child = Arc::clone(&child);
        let exit = Arc::clone(&exit);
        let done = Arc::clone(&done);
        let notify = Arc::clone(&notify);
        let finished_at = Arc::clone(&finished_at);
        let sandbox = Arc::clone(&sandbox);
        std::thread::spawn(move || loop {
            if done.load(Ordering::Relaxed) {
                signal_done(&notify);
                break;
            }
            let st = { child.lock().unwrap_or_else(|e| e.into_inner()).try_wait() };
            match st {
                Ok(Some(status)) => {
                    *exit.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(status.code().unwrap_or(-1));
                    *finished_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                    done.store(true, Ordering::Relaxed);
                    sandbox
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .finish(crate::sandbox::runner::Outcome::Exit(status.code()));
                    signal_done(&notify);
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(60)),
                Err(_) => {
                    done.store(true, Ordering::Relaxed);
                    signal_done(&notify);
                    break;
                }
            }
        });
    }

    let id = format!("proc_{}", NEXT_ID.fetch_add(1, Ordering::Relaxed));
    let entry = ProcEntry {
        command: command.to_string(),
        pid,
        started: Instant::now(),
        scope: scope.to_string(),
        out,
        exit,
        done,
        notify,
        child,
        stdin: Arc::new(Mutex::new(stdin)),
        finished_at,
        containment,
        sandbox,
    };
    REGISTRY
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(id.clone(), entry);
    Ok(id)
}

/// Read a pipe in chunks on a background thread, appending raw bytes to the shared ring buffer.
fn spawn_drain<R: std::io::Read + Send + 'static>(pipe: Option<R>, out: Arc<Mutex<RingBuf>>) {
    if let Some(mut p) = pipe {
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            loop {
                match p.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => out
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push(&buf[..n]),
                }
            }
        });
    }
}

/// Best-effort kill of a process AND its children (a bg shell usually has a real child).
///
/// Uses the job object / process group captured at spawn rather than the old `taskkill /F /T /PID`.
/// `taskkill /T` walks the live parent→child chain, so it only reaches descendants whose ancestors are
/// *still running*: once the `cmd.exe` wrapper exits (or a launcher double-forks, as npm/python
/// wrappers do), the chain is broken and the real server survives — holding its port and the pipe. The
/// kernel's job membership has no such gap, and needs no extra process spawn to do the killing.
fn kill_tree(entry: &ProcEntry) {
    let mut child = entry.child.lock().unwrap_or_else(|e| e.into_inner());
    crate::core::proctree::kill_tree(&mut child, &entry.containment);
    entry.done.store(true, Ordering::Relaxed);
    entry
        .sandbox
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .finish(crate::sandbox::runner::Outcome::Exit(None));
    if entry
        .finished_at
        .lock()
        .map(|g| g.is_none())
        .unwrap_or(false)
    {
        *entry.finished_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }
    // Wake a `wait` immediately rather than leaving it to sit out its remaining timeout.
    signal_done(&entry.notify);
}

/// Kill every still-running background process and clear the pool. Called on REPL + daemon exit so
/// dev servers / watchers the agent started via `process start` don't outlive the CLI (orphaned
/// ports + CPU). Deliberately SCOPE-BLIND — process exit must reap every scope, not just whichever
/// one happens to be pinned on the calling thread. Best-effort and idempotent.
pub fn kill_all() {
    let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    for entry in reg.values() {
        if !entry.done.load(Ordering::Relaxed) {
            kill_tree(entry);
        }
    }
    reg.clear();
}

/// `process` — manage long-running background commands (start/list/log/status/wait/kill/write).
pub struct Process {
    root: PathBuf,
}
impl Process {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for Process {
    fn name(&self) -> &str {
        "process"
    }
    fn description(&self) -> &str {
        "Run and manage LONG-RUNNING background commands (dev servers, watchers, long builds) that \
         shell_run's cap would kill. action=start returns a proc_<n> handle; then wait (blocks \
         until exit), log (cursor=<next_cursor> → only new output), status/kill/write. Use \
         shell_run for quick commands. Handles are private to this agent."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {"type": "string", "enum": ["start", "list", "log", "status", "wait", "kill", "write"]},
                "command": {"type": "string", "description": "the command (action=start)"},
                "cwd": {"type": "string", "description": "optional working dir (subdir, ../ or absolute) (action=start)"},
                "network": {"type": "boolean", "description": "request network access (action=start; default false; approval-gated). Any socket — even binding a port — needs it."},
                "id": {"type": "string", "description": "a proc_<n> handle (all actions except start/list)"},
                "cursor": {"type": "integer", "description": "resume point from a previous next_cursor; returns only newer output (action=log/wait)"},
                "timeout_secs": {"type": "integer", "description": "max seconds to block (action=wait; default 30)"},
                "input": {"type": "string", "description": "text for the process stdin (action=write)"},
                "enter": {"type": "boolean", "description": "append newline (action=write; default true)"}
            },
            "required": ["action"]
        })
    }
    fn is_destructive(&self) -> bool {
        true // action=start runs arbitrary commands; kill/write affect a live process
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, args: &Value) -> WorkspaceEffect {
        match args.get("action").and_then(|v| v.as_str()) {
            Some("start") | Some("write") => WorkspaceEffect::OpaqueWorkspace,
            _ => WorkspaceEffect::None,
        }
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .context("missing `action`")?;
        let scope = caller_scope();
        let cursor = args.get("cursor").and_then(|v| v.as_u64());
        match action {
            "start" => {
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .context("start needs `command`")?;
                let cwd = args.get("cwd").and_then(|v| v.as_str());
                let network = args
                    .get("network")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let id = start(&self.root, command, cwd, &scope, network)?;
                let pid = REGISTRY
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&id)
                    .map(|e| e.pid)
                    .unwrap_or(0);
                Ok(format!(
                    "{id} started (pid {pid}): {command}\nIt runs in the background. For a build or \
                     test, block on it with process(action=wait, id={id}, timeout_secs=…). For \
                     progress on something long-lived, poll process(action=log, id={id}, \
                     cursor=<next_cursor>) — with a cursor you get only the new output instead of \
                     the whole log again. Stop it with process(action=kill, id={id})."
                ))
            }
            "list" => {
                let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                let mut ids: Vec<&String> = reg
                    .iter()
                    .filter(|(_, e)| e.scope == scope)
                    .map(|(k, _)| k)
                    .collect();
                if ids.is_empty() {
                    return Ok("no background processes".to_string());
                }
                ids.sort();
                let mut s = String::from("background processes:\n");
                for id in ids {
                    let e = &reg[id];
                    s.push_str(&format!(
                        "  {id}  {:<11}  {:>6}  {}\n",
                        e.status_label(),
                        elapsed_label(e.started.elapsed()),
                        e.command
                    ));
                }
                Ok(s.trim_end().to_string())
            }
            "log" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .context("log needs `id`")?;
                let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                let e = lookup(&reg, id, &scope)?;
                let buf = e.out.lock().unwrap_or_else(|e| e.into_inner());
                match cursor {
                    Some(c) => Ok(render_delta(id, &e.status_label(), c, &buf.since(c))),
                    None => Ok(format!(
                        "{id} [{}] next_cursor={}\n{}",
                        e.status_label(),
                        buf.end_offset(),
                        buf.text().trim_end()
                    )),
                }
            }
            "status" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .context("status needs `id`")?;
                let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                let e = lookup(&reg, id, &scope)?;
                Ok(format!(
                    "{id}: {} (elapsed {})",
                    e.status_label(),
                    elapsed_label(e.started.elapsed())
                ))
            }
            "wait" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .context("wait needs `id`")?
                    .to_string();
                let timeout = args
                    .get("timeout_secs")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(30);
                // Snapshot the notification handle, then BLOCK on it without holding the registry
                // lock. A condvar rather than a sleep-poll: a `cargo check` that finishes in 800ms
                // returns in 800ms, and a kill wakes the waiter at once (see `kill_tree`).
                let (done, notify) = {
                    let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                    let e = lookup(&reg, &id, &scope)?;
                    (Arc::clone(&e.done), Arc::clone(&e.notify))
                };
                let deadline = Instant::now() + Duration::from_secs(timeout);
                {
                    let mut flag = notify.0.lock().unwrap_or_else(|e| e.into_inner());
                    while !*flag {
                        let now = Instant::now();
                        if now >= deadline {
                            break;
                        }
                        let (guard, _) = notify
                            .1
                            .wait_timeout(flag, deadline - now)
                            .unwrap_or_else(|e| e.into_inner());
                        flag = guard;
                    }
                }
                let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                let e = lookup(&reg, &id, &scope)?;
                let buf = e.out.lock().unwrap_or_else(|e| e.into_inner());
                let exited = done.load(Ordering::Relaxed);
                // Cursor support on BOTH branches: a wait that times out is the natural point to
                // resume from, and re-sending the whole buffer each round is the cost this fixes.
                if let Some(c) = cursor {
                    let delta = buf.since(c);
                    let status = if exited {
                        e.status_label()
                    } else {
                        format!("still running after {timeout}s, not killed")
                    };
                    return Ok(render_delta(&id, &status, c, &delta));
                }
                let out = buf.text();
                if exited {
                    Ok(format!(
                        "{id} {} next_cursor={}\n{}",
                        e.status_label(),
                        buf.end_offset(),
                        out.trim_end()
                    ))
                } else {
                    Ok(format!(
                        "{id} still running after {timeout}s (not killed). next_cursor={} — pass it \
                         as `cursor` next time to get only new output. Latest output:\n{}",
                        buf.end_offset(),
                        out.trim_end()
                    ))
                }
            }
            "kill" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .context("kill needs `id`")?;
                let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                let e = lookup(&reg, id, &scope)?;
                kill_tree(e);
                Ok(format!("{id} killed"))
            }
            "write" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .context("write needs `id`")?;
                let mut input = args
                    .get("input")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if args.get("enter").and_then(|v| v.as_bool()).unwrap_or(true) {
                    input.push('\n');
                }
                let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                let e = lookup(&reg, id, &scope)?;
                let mut guard = e.stdin.lock().unwrap_or_else(|e| e.into_inner());
                let stdin = guard.as_mut().context("process stdin is closed")?;
                stdin
                    .write_all(input.as_bytes())
                    .context("writing to process stdin")?;
                stdin.flush().ok();
                Ok(format!("wrote {} bytes to {id} stdin", input.len()))
            }
            other => bail!("unknown action '{other}' (use start/list/log/status/wait/kill/write)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pool and `kill_all` are process-global, so these tests cannot run concurrently with each
    /// other: one test's `kill_all` would reap another's live handle. Every test takes this first.
    static TEST_SERIAL: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        TEST_SERIAL.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn root() -> PathBuf {
        std::env::current_dir().unwrap().canonicalize().unwrap()
    }

    /// Run `f` with `scope` pinned as the execution scope, exactly as the executor does for a tool
    /// body inside its `spawn_blocking` closure.
    fn scoped<T>(scope: &str, f: impl FnOnce() -> T) -> T {
        let ctx = crate::core::exec_ctx::ExecutionContext::default().with_resource_scope(scope);
        crate::core::exec_ctx::with_current(ctx, f)
    }

    fn forget(id: &str) {
        REGISTRY
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(id);
    }

    #[test]
    fn start_log_wait_lifecycle() {
        let _s = serial();
        let t = Process::new(root());
        let started = t
            .execute(&serde_json::json!({"action":"start","command":"echo hello-bg"}))
            .unwrap();
        let id = started.split_whitespace().next().unwrap().to_string();
        assert!(id.starts_with("proc_"));

        // Wait for it to finish, then the log must contain the output.
        let waited = t
            .execute(&serde_json::json!({"action":"wait","id":id,"timeout_secs":10}))
            .unwrap();
        assert!(waited.contains("hello-bg"), "got: {waited}");
        assert!(
            waited.contains("exited("),
            "should be exited; got: {waited}"
        );

        let listed = t.execute(&serde_json::json!({"action":"list"})).unwrap();
        assert!(listed.contains(&id));

        forget(&id);
    }

    #[test]
    fn unknown_id_errors() {
        let _s = serial();
        let t = Process::new(root());
        assert!(t
            .execute(&serde_json::json!({"action":"log","id":"proc_999999"}))
            .is_err());
        assert!(t.execute(&serde_json::json!({"action":"status"})).is_err()); // missing id
        assert!(t.execute(&serde_json::json!({"action":"bogus"})).is_err());
    }

    #[test]
    fn wait_returns_as_soon_as_the_process_exits() {
        let _s = serial();
        let t = Process::new(root());
        let started = t
            .execute(&serde_json::json!({"action":"start","command":"echo quick"}))
            .unwrap();
        let id = started.split_whitespace().next().unwrap().to_string();
        // A generous timeout the call must NOT sit out: the condvar wakes on exit, so this returns
        // in well under a second. A regression to a long sleep-poll would blow the bound.
        let began = Instant::now();
        let waited = t
            .execute(&serde_json::json!({"action":"wait","id":id,"timeout_secs":30}))
            .unwrap();
        assert!(waited.contains("exited("), "got: {waited}");
        assert!(
            began.elapsed() < Duration::from_secs(10),
            "wait blocked far longer than the process ran: {:?}",
            began.elapsed()
        );
        forget(&id);
    }

    #[test]
    fn log_with_a_cursor_returns_only_new_output() {
        let _s = serial();
        let t = Process::new(root());
        let started = t
            .execute(&serde_json::json!({"action":"start","command":"echo first-line"}))
            .unwrap();
        let id = started.split_whitespace().next().unwrap().to_string();
        let first = t
            .execute(&serde_json::json!({"action":"wait","id":id,"timeout_secs":10}))
            .unwrap();
        assert!(first.contains("first-line"), "got: {first}");

        // Feed the advertised cursor back: the process is done, so there is nothing new and the
        // earlier output must NOT be repeated.
        let cursor: u64 = first
            .split("next_cursor=")
            .nth(1)
            .and_then(|s| s.split_whitespace().next())
            .and_then(|s| s.parse().ok())
            .unwrap_or_else(|| panic!("no next_cursor in: {first}"));
        let second = t
            .execute(&serde_json::json!({"action":"log","id":id,"cursor":cursor}))
            .unwrap();
        assert!(
            !second.contains("first-line"),
            "cursor read repeated old output: {second}"
        );
        assert!(second.contains("no new output"), "got: {second}");
        forget(&id);
    }

    #[test]
    fn handles_are_private_to_their_execution_scope() {
        let _s = serial();
        let t = Process::new(root());
        let started = scoped("iso-owner", || {
            t.execute(&serde_json::json!({"action":"start","command":"echo owned"}))
                .unwrap()
        });
        let id = started.split_whitespace().next().unwrap().to_string();

        // A sibling scope can neither see nor touch it — and gets the same message as for an id
        // that never existed, so the handle's existence does not leak.
        scoped("iso-sibling", || {
            let listed = t.execute(&serde_json::json!({"action":"list"})).unwrap();
            assert_eq!(listed, "no background processes", "sibling saw: {listed}");
            for action in ["log", "status", "wait", "kill"] {
                let err = t
                    .execute(&serde_json::json!({"action":action,"id":id,"timeout_secs":1}))
                    .expect_err("sibling scope must not resolve a foreign handle");
                assert!(
                    err.to_string().contains("no such process"),
                    "{action}: {err}"
                );
            }
        });

        // The owning scope still has it.
        scoped("iso-owner", || {
            let listed = t.execute(&serde_json::json!({"action":"list"})).unwrap();
            assert!(listed.contains(&id), "owner lost its handle: {listed}");
            assert!(t
                .execute(&serde_json::json!({"action":"log","id":id}))
                .is_ok());
        });
        forget(&id);
    }

    #[test]
    fn kill_all_reaps_every_scope() {
        let _s = serial();
        let t = Process::new(root());
        let a = scoped("reap-a", || {
            t.execute(&serde_json::json!({"action":"start","command":"echo a"}))
                .unwrap()
        });
        let b = scoped("reap-b", || {
            t.execute(&serde_json::json!({"action":"start","command":"echo b"}))
                .unwrap()
        });
        let (ia, ib) = (
            a.split_whitespace().next().unwrap().to_string(),
            b.split_whitespace().next().unwrap().to_string(),
        );
        kill_all();
        let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        assert!(
            !reg.contains_key(&ia) && !reg.contains_key(&ib),
            "kill_all must clear handles from every scope"
        );
    }

    // ── RingBuf offsets ──────────────────────────────────────────────────────

    #[test]
    fn ring_offsets_track_absolute_positions() {
        let mut r = RingBuf::new(16);
        assert_eq!(r.end_offset(), 0);
        r.push(b"abcd");
        assert_eq!((r.start_offset, r.end_offset()), (0, 4));

        // A fresh cursor gets everything and lands at the end.
        let d = r.since(0);
        assert_eq!(d.text, "abcd");
        assert_eq!(d.next_cursor, 4);
        assert_eq!(d.dropped_before, 0);
        assert!(!d.clamped);

        // Only the appended bytes come back on the next read.
        r.push(b"efgh");
        let d = r.since(4);
        assert_eq!(d.text, "efgh");
        assert_eq!(d.next_cursor, 8);
    }

    #[test]
    fn ring_reports_bytes_dropped_before_a_stale_cursor() {
        let mut r = RingBuf::new(8);
        r.push(b"0123456789ABCD"); // 14 bytes into an 8-byte ring → first 6 evicted
        assert_eq!(r.start_offset, 6);
        assert_eq!(r.end_offset(), 14);
        let d = r.since(2);
        assert_eq!(d.dropped_before, 4, "bytes 2..6 are gone");
        assert_eq!(d.text, "6789ABCD", "only what is still retained");
        assert_eq!(d.next_cursor, 14);
        // The whole-buffer read says so too, so a no-cursor caller isn't misled either.
        assert!(r.text().contains("6 earlier bytes dropped"), "{}", r.text());
    }

    #[test]
    fn ring_handles_no_new_output_and_a_future_cursor() {
        let mut r = RingBuf::new(16);
        r.push(b"abc");
        // Cursor exactly at the end: nothing new, cursor unchanged.
        let d = r.since(3);
        assert!(d.text.is_empty());
        assert_eq!(d.next_cursor, 3);
        assert!(!d.clamped);
        // Beyond the end: clamped and flagged rather than an error or a panic.
        let d = r.since(999);
        assert!(d.text.is_empty());
        assert_eq!(d.next_cursor, 3);
        assert!(d.clamped);
    }
}
