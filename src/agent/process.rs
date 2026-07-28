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
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Max concurrent/retained processes; a new `start` over the cap prunes the oldest FINISHED one.
const MAX_PROCESSES: usize = 16;
/// Rolling merged stdout+stderr buffer per process (bytes); older output is dropped from the front.
const OUTPUT_CAP: usize = 200 * 1024;

/// A bounded byte buffer: appends keep only the most-recent `cap` bytes (a coarse ring).
struct RingBuf {
    bytes: Vec<u8>,
    cap: usize,
    dropped: bool,
}
impl RingBuf {
    fn new(cap: usize) -> Self {
        RingBuf {
            bytes: Vec::new(),
            cap,
            dropped: false,
        }
    }
    fn push(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
        if self.bytes.len() > self.cap {
            let overflow = self.bytes.len() - self.cap;
            self.bytes.drain(..overflow);
            self.dropped = true;
        }
    }
    fn text(&self) -> String {
        let body = String::from_utf8_lossy(&self.bytes).into_owned();
        if self.dropped {
            format!("…[earlier output dropped]…\n{body}")
        } else {
            body
        }
    }
}

struct ProcEntry {
    command: String,
    pid: u32,
    started: Instant,
    out: Arc<Mutex<RingBuf>>,
    exit: Arc<Mutex<Option<i32>>>,
    done: Arc<AtomicBool>,
    child: Arc<Mutex<Child>>,
    stdin: Arc<Mutex<Option<ChildStdin>>>,
    finished_at: Arc<Mutex<Option<Instant>>>,
    /// Job object / process group holding the whole tree. Kept for the entry's lifetime: on Windows
    /// this is also crash insurance — if Aizen dies without running `kill_all`, the OS closes the job
    /// handle and the kernel reaps every descendant, so a `npm run dev` cannot outlive the CLI.
    containment: Arc<crate::core::proctree::Containment>,
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

fn elapsed_label(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}m{:02}s", s / 60, s % 60)
    }
}

/// Spawn `command` in the background, confined to `root` (optionally a `cwd` subdir). Returns the
/// new `proc_<n>` id.
fn start(root: &PathBuf, command: &str, cwd: Option<&str>) -> Result<String> {
    let dir = match cwd {
        Some(c) => confine(root, c, true)?,
        None => root.clone(),
    };

    // Prune a finished slot if we're at the cap; refuse if everything is still running.
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

    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(format!("chcp 65001>nul & {command}"));
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    cmd.current_dir(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Contain the tree at spawn. A background command is the WORST case for the orphan problem this
    // guards: `process start` is what runs dev servers and watchers, i.e. exactly the processes that
    // spawn real children behind the `cmd.exe`/`sh` wrapper and keep holding a port after a kill.
    crate::core::proctree::prepare(&mut cmd);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning background `{command}`"))?;
    let containment = Arc::new(crate::core::proctree::contain(&child));
    let pid = child.id();

    let out = Arc::new(Mutex::new(RingBuf::new(OUTPUT_CAP)));
    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let stdin = child.stdin.take();
    spawn_drain(out_pipe, Arc::clone(&out));
    spawn_drain(err_pipe, Arc::clone(&out));

    let exit = Arc::new(Mutex::new(None));
    let done = Arc::new(AtomicBool::new(false));
    let finished_at = Arc::new(Mutex::new(None));
    let child = Arc::new(Mutex::new(child));

    // Monitor: reap the child when it exits, recording the code + done flag (no busy CPU).
    {
        let child = Arc::clone(&child);
        let exit = Arc::clone(&exit);
        let done = Arc::clone(&done);
        let finished_at = Arc::clone(&finished_at);
        std::thread::spawn(move || loop {
            if done.load(Ordering::Relaxed) {
                break;
            }
            let st = { child.lock().unwrap_or_else(|e| e.into_inner()).try_wait() };
            match st {
                Ok(Some(status)) => {
                    *exit.lock().unwrap_or_else(|e| e.into_inner()) =
                        Some(status.code().unwrap_or(-1));
                    *finished_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
                    done.store(true, Ordering::Relaxed);
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(60)),
                Err(_) => {
                    done.store(true, Ordering::Relaxed);
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
        out,
        exit,
        done,
        child,
        stdin: Arc::new(Mutex::new(stdin)),
        finished_at,
        containment,
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
    if entry
        .finished_at
        .lock()
        .map(|g| g.is_none())
        .unwrap_or(false)
    {
        *entry.finished_at.lock().unwrap_or_else(|e| e.into_inner()) = Some(Instant::now());
    }
}

/// Kill every still-running background process and clear the pool. Called on REPL + daemon exit so
/// dev servers / watchers the agent started via `process start` don't outlive the CLI (orphaned
/// ports + CPU). Best-effort and idempotent — a no-op when the pool is empty.
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
         shell_run's foreground wall-clock cap would kill. action=start spawns a command and returns a \
         proc_<n> handle immediately; then action=log/status/wait/kill/write manage it. Use shell_run \
         (not this) for quick commands that finish in seconds. Confined to the working dir."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "action": {"type": "string", "enum": ["start", "list", "log", "status", "wait", "kill", "write"]},
                "command": {"type": "string", "description": "the command (action=start)"},
                "cwd": {"type": "string", "description": "optional working dir for the process (a subdir, or a ../ or absolute path elsewhere) (action=start)"},
                "id": {"type": "string", "description": "a proc_<n> handle (all actions except start/list)"},
                "timeout_secs": {"type": "integer", "description": "max seconds to block (action=wait; default 30)"},
                "input": {"type": "string", "description": "text to send to the process stdin (action=write)"},
                "enter": {"type": "boolean", "description": "append a newline after input (action=write; default true)"}
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
        match action {
            "start" => {
                let command = args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .context("start needs `command`")?;
                let cwd = args.get("cwd").and_then(|v| v.as_str());
                let id = start(&self.root, command, cwd)?;
                let pid = REGISTRY
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get(&id)
                    .map(|e| e.pid)
                    .unwrap_or(0);
                Ok(format!(
                    "{id} started (pid {pid}): {command}\nIt runs in the background. Read output with \
                     process(action=log, id={id}); stop it with process(action=kill, id={id})."
                ))
            }
            "list" => {
                let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                if reg.is_empty() {
                    return Ok("no background processes".to_string());
                }
                let mut ids: Vec<&String> = reg.keys().collect();
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
                let e = reg
                    .get(id)
                    .with_context(|| format!("no such process '{id}'"))?;
                let out = e.out.lock().unwrap_or_else(|e| e.into_inner()).text();
                Ok(format!("{id} [{}]\n{}", e.status_label(), out.trim_end()))
            }
            "status" => {
                let id = args
                    .get("id")
                    .and_then(|v| v.as_str())
                    .context("status needs `id`")?;
                let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                let e = reg
                    .get(id)
                    .with_context(|| format!("no such process '{id}'"))?;
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
                // Snapshot the done-flag handle, then poll WITHOUT holding the registry lock.
                let done = {
                    let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                    let e = reg
                        .get(&id)
                        .with_context(|| format!("no such process '{id}'"))?;
                    Arc::clone(&e.done)
                };
                let start = Instant::now();
                while !done.load(Ordering::Relaxed) && start.elapsed().as_secs() < timeout {
                    std::thread::sleep(Duration::from_millis(100));
                }
                let reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
                let e = reg
                    .get(&id)
                    .with_context(|| format!("no such process '{id}'"))?;
                let out = e.out.lock().unwrap_or_else(|e| e.into_inner()).text();
                if done.load(Ordering::Relaxed) {
                    Ok(format!("{id} {}\n{}", e.status_label(), out.trim_end()))
                } else {
                    Ok(format!(
                        "{id} still running after {timeout}s (not killed). Latest output:\n{}",
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
                let e = reg
                    .get(id)
                    .with_context(|| format!("no such process '{id}'"))?;
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
                let e = reg
                    .get(id)
                    .with_context(|| format!("no such process '{id}'"))?;
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

    fn root() -> PathBuf {
        std::env::current_dir().unwrap().canonicalize().unwrap()
    }

    #[test]
    fn start_log_wait_lifecycle() {
        let t = Process::new(root());
        // A short command that prints + exits.
        let cmd = if cfg!(windows) {
            "echo hello-bg"
        } else {
            "echo hello-bg"
        };
        let started = t
            .execute(&serde_json::json!({"action":"start","command":cmd}))
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

        // Cleanup so the pool doesn't leak across tests.
        let mut reg = REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
        reg.remove(&id);
    }

    #[test]
    fn unknown_id_errors() {
        let t = Process::new(root());
        assert!(t
            .execute(&serde_json::json!({"action":"log","id":"proc_999999"}))
            .is_err());
        assert!(t.execute(&serde_json::json!({"action":"status"})).is_err()); // missing id
        assert!(t.execute(&serde_json::json!({"action":"bogus"})).is_err());
    }
}
