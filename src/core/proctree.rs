//! Process-tree containment and tree-kill for every child Aizen spawns through a shell.
//!
//! The problem this exists to solve, measured on Windows 11 (26200): we spawn `cmd /C … & <command>`,
//! so the process we hold a handle to is the **wrapper**, not the real work. `Child::kill()` calls
//! `TerminateProcess` on that wrapper only — a grandchild (`cargo`, `node`, `pytest`, a dev server)
//! keeps running, orphaned. Worse, it inherited the write end of our stdout/stderr pipes, so a
//! subsequent `read_to_end` on those pipes **never sees EOF**: the reader thread blocks for as long
//! as the orphan lives, which for a wedged process means forever. A reproduction confirmed both
//! halves: after `kill()` the grandchild was still alive, and the drain-thread join was still blocked
//! 12s later, having been given a 45s sleeper to outlast.
//!
//! The fix is the standard one for a Windows process supervisor, already used here for language
//! servers (`agent::lsp::jobobject`): put the wrapper in a **Job Object** with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so terminating the job — or merely dropping the handle, or
//! crashing — reaps every descendant. Unix gets the equivalent via a **process group**: `setsid` at
//! spawn, then `killpg`.
//!
//! Everything here is best-effort by design. A failed job creation yields a [`Containment::None`]
//! that still kills the direct child, which is exactly the old behaviour — a platform that refuses
//! the API degrades, it does not break.

use std::process::{Child, Command};
use std::time::Duration;

/// A handle keeping a spawned tree contained. Hold it for as long as the child may live; dropping it
/// on Windows kills the whole tree (kill-on-close), which is also the crash-safety property.
pub enum Containment {
    /// Windows job object (kill-on-close). Owns the handle; closed exactly once on drop.
    #[cfg(windows)]
    Job(windows_job::Job),
    /// Unix process group led by the child (`setsid`), killed via `killpg`.
    #[cfg(unix)]
    Group(i32),
    /// Containment unavailable — [`kill_tree`] falls back to killing the direct child only.
    None,
}

impl std::fmt::Debug for Containment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            #[cfg(windows)]
            Self::Job(_) => f.write_str("Containment::Job"),
            #[cfg(unix)]
            Self::Group(pgid) => write!(f, "Containment::Group({pgid})"),
            Self::None => f.write_str("Containment::None"),
        }
    }
}

impl Containment {
    /// Did containment actually take? Callers use this only for diagnostics — behaviour degrades
    /// gracefully either way.
    pub fn is_contained(&self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Prepare `cmd` so the child it spawns can lead its own process group (Unix). On Windows this is a
/// no-op: containment is applied *after* spawn, by assigning the live process to a job.
pub fn prepare(cmd: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: `setsid` is async-signal-safe and is the documented way to detach a child into a
        // new session/group from the post-fork, pre-exec hook. It touches no allocator or lock.
        unsafe {
            cmd.pre_exec(|| {
                libc_setsid();
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// Contain a just-spawned child. Pair with [`prepare`] on the same `Command`.
pub fn contain(child: &Child) -> Containment {
    #[cfg(windows)]
    {
        return windows_job::contain(child);
    }
    #[cfg(unix)]
    {
        // `prepare` made the child a session leader, so its pid IS the process-group id.
        return Containment::Group(child.id() as i32);
    }
    #[allow(unreachable_code)]
    {
        let _ = child;
        Containment::None
    }
}

/// Kill the child **and every descendant**, then reap the direct child so it is not left a zombie.
///
/// Order matters: terminate the tree first, then `wait()`. Reaping first would leave the orphans
/// running with our pipe handles still open — the exact hang this module exists to prevent.
pub fn kill_tree(child: &mut Child, containment: &Containment) {
    terminate_tree(containment);
    let _ = child.kill();
    let _ = child.wait();
}

/// Kill every process in the containment WITHOUT touching a `Child` handle.
///
/// The async spawn path (verify gate) hands its child to `wait_with_output()`, which consumes it, so
/// there is no `Child` left to kill when the timeout fires — only `kill_on_drop`, which reaps the
/// wrapper alone. This kills the tree the wrapper left behind.
pub fn terminate_tree(containment: &Containment) {
    match containment {
        #[cfg(windows)]
        Containment::Job(job) => job.terminate(),
        #[cfg(unix)]
        Containment::Group(pgid) => {
            // SAFETY: a plain kill(2) on a process-group id we created; an invalid/exited group is
            // simply an ESRCH error we ignore.
            unsafe { libc_killpg(*pgid, 9) };
        }
        Containment::None => {}
    }
}

/// Join a pipe-drain thread with a deadline, returning `(text, timed_out)`.
///
/// `JoinHandle::join` has no timeout, so the wait moves onto a channel: the drain thread's result
/// arrives through `tx`, and a lapsed `recv_timeout` means the thread is still parked on a pipe that
/// nobody is going to close. We then abandon it — a detached thread blocked in `read` costs one stack
/// and exits if EOF ever arrives, which beats blocking the caller forever.
pub fn join_drain<T: Send + Default + 'static>(
    handle: std::thread::JoinHandle<T>,
    deadline: Duration,
) -> (T, bool) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(handle.join().unwrap_or_default());
    });
    match rx.recv_timeout(deadline) {
        Ok(value) => (value, false),
        Err(_) => (T::default(), true),
    }
}

/// Read a pipe to raw bytes — the byte-exact basis for both output shapes.
///
/// Deliberately `read_to_end` rather than `read_to_string`: the latter returns `Err` and leaves the
/// buffer EMPTY at the first invalid UTF-8 byte, so on a non-English Windows the OEM-codepage output
/// of `dir` and friends would be dropped wholesale. Callers that want text decode these bytes
/// lossily (see [`output_bounded`]), which keeps the structure and degrades only the odd byte.
fn read_raw<R: std::io::Read>(pipe: Option<R>) -> Vec<u8> {
    match pipe {
        Some(mut p) => {
            let mut bytes = Vec::new();
            let _ = p.read_to_end(&mut bytes);
            bytes
        }
        None => Vec::new(),
    }
}

/// What [`output_bounded`] came back with.
pub struct BoundedOutput {
    pub stdout: String,
    pub stderr: String,
    /// `None` when the command was killed at the deadline.
    pub code: Option<i32>,
    pub timed_out: bool,
    /// A drain thread was still blocked at the grace deadline, so the text above may be short.
    pub output_truncated: bool,
}

/// What [`output_bounded_bytes`] came back with — identical to [`BoundedOutput`] but BYTE-EXACT.
///
/// Required by callers whose stdout is binary and must survive verbatim: `git cat-file blob` hands
/// back file contents, and decoding those through `String::from_utf8_lossy` would replace every
/// invalid byte with U+FFFD — silently corrupting any non-UTF-8 file a checkpoint restores or diffs.
/// `-z`-separated git output has the same requirement (NUL-delimited, byte-exact paths).
pub struct BoundedBytes {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// `None` when the command was killed at the deadline.
    pub code: Option<i32>,
    pub timed_out: bool,
    /// A drain thread was still blocked at the grace deadline, so the output above may be short.
    pub output_truncated: bool,
}

impl BoundedBytes {
    /// Did the command exit successfully? (`false` for a timeout kill, which has no code — the
    /// distinction matters: a killed child must never read as success.)
    ///
    /// Currently exercised only by this module's tests; production callers rebuild a real
    /// `ExitStatus` from `code` instead (see `timemachine::run_git_bounded`).
    #[allow(dead_code)]
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// Run `cmd` to completion under a wall-clock deadline, with the whole process tree contained.
///
/// This is the safe replacement for `Command::output()`, which has **no** timeout: it blocks until
/// every pipe reaches EOF, and a grandchild that outlives its wrapper keeps the write end open, so a
/// single wedged command hangs the caller for good. Used by the interactive `!cmd` escape and by
/// custom-command `` !`cmd` `` expansion — both run on the REPL's own thread, where a hang freezes
/// the UI rather than just one tool call.
///
/// The caller owns `cmd` (working dir, env); stdio is configured here.
pub fn output_bounded(
    cmd: &mut Command,
    timeout: Duration,
    drain_grace: Duration,
) -> std::io::Result<BoundedOutput> {
    let raw = output_bounded_bytes(cmd, timeout, drain_grace)?;
    Ok(BoundedOutput {
        stdout: String::from_utf8_lossy(&raw.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&raw.stderr).into_owned(),
        code: raw.code,
        timed_out: raw.timed_out,
        output_truncated: raw.output_truncated,
    })
}

/// [`output_bounded`] without the lossy decode: stdout/stderr come back as raw bytes.
///
/// Use this whenever the child's stdout is binary or delimiter-exact (`git cat-file blob`, any `-z`
/// output). [`output_bounded`] is a thin lossy wrapper over this function, so both share one
/// containment/deadline implementation — a fix here reaches every bounded caller.
pub fn output_bounded_bytes(
    cmd: &mut Command,
    timeout: Duration,
    drain_grace: Duration,
) -> std::io::Result<BoundedBytes> {
    use std::process::Stdio;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    prepare(cmd);
    let mut child = cmd.spawn()?;
    let containment = contain(&child);

    let out_pipe = child.stdout.take();
    let err_pipe = child.stderr.take();
    let oh = std::thread::spawn(move || read_raw(out_pipe));
    let eh = std::thread::spawn(move || read_raw(err_pipe));

    let start = std::time::Instant::now();
    let mut timed_out = false;
    let code = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code(),
            Ok(None) => {
                if start.elapsed() >= timeout {
                    kill_tree(&mut child, &containment);
                    timed_out = true;
                    break None;
                }
                std::thread::sleep(Duration::from_millis(40));
            }
            Err(e) => return Err(e),
        }
    };

    let (stdout, out_cut) = join_drain(oh, drain_grace);
    let (stderr, err_cut) = join_drain(eh, drain_grace);
    Ok(BoundedBytes {
        stdout,
        stderr,
        code,
        timed_out,
        output_truncated: out_cut || err_cut,
    })
}

/// [`prepare`] for the async spawn path.
pub fn prepare_tokio(cmd: &mut tokio::process::Command) {
    #[cfg(unix)]
    {
        // SAFETY: as `prepare` — `setsid` is async-signal-safe and touches no allocator or lock.
        unsafe {
            cmd.pre_exec(|| {
                libc_setsid();
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    {
        let _ = cmd;
    }
}

/// [`contain`] for the async spawn path.
pub fn contain_tokio(child: &tokio::process::Child) -> Containment {
    #[cfg(windows)]
    {
        return windows_job::contain_tokio(child);
    }
    #[cfg(unix)]
    {
        // `prepare_tokio` made the child a session leader ⇒ its pid is the group id. A child already
        // reaped has no id; nothing to contain then.
        return match child.id() {
            Some(pid) => Containment::Group(pid as i32),
            None => Containment::None,
        };
    }
    #[allow(unreachable_code)]
    {
        let _ = child;
        Containment::None
    }
}

#[cfg(unix)]
extern "C" {
    #[link_name = "setsid"]
    fn c_setsid() -> i32;
    #[link_name = "killpg"]
    fn c_killpg(pgrp: i32, sig: i32) -> i32;
}

// Thin wrappers so the `unsafe` call sites above read as one operation each. Declared rather than
// pulled from `libc`: these two symbols are all we need, and the crate is not in the dependency
// tree (single static binary posture — see Cargo.toml).
#[cfg(unix)]
fn libc_setsid() {
    // SAFETY: no arguments, no memory touched; a failure (already a leader) is ignored by design.
    unsafe { c_setsid() };
}

#[cfg(unix)]
unsafe fn libc_killpg(pgrp: i32, sig: i32) {
    c_killpg(pgrp, sig);
}

#[cfg(windows)]
pub mod windows_job {
    //! Kill-on-close Job Object for a `std::process::Child`.
    //!
    //! This mirrors `agent::lsp::jobobject` (which contains *tokio* children for language servers).
    //! The duplication is deliberate and small: that module is typed against `tokio::process::Child`
    //! and lives behind the LSP feature surface, while shell/verify children are `std::process`.
    //! Unifying them would mean a generic over raw-handle extraction for a 30-line binding.

    use super::Containment;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
        SetInformationJobObject, TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// An owned kill-on-close job handle. Dropping it — or the process dying, even on a hard crash —
    /// terminates every process assigned to it.
    pub struct Job(HANDLE);

    // SAFETY: a job handle is a kernel object usable from any thread; we only ever close it once
    // (in Drop) and never hand out the raw value.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        fn kill_on_close() -> Option<Self> {
            // SAFETY: null attributes + null name are the documented "anonymous job" arguments; the
            // handle is checked before use and owned by `Job` (closed exactly once in Drop).
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return None;
            }
            let job = Job(handle);
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `info` is initialized and the size argument is exactly its size — the
            // (pointer, length) pair the API requires.
            let ok = unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    std::ptr::from_ref(&info).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            (ok != 0).then_some(job)
        }

        /// Terminate every process in the job now (rather than waiting for the handle to close).
        pub fn terminate(&self) {
            // SAFETY: `self.0` is a live handle we own. Exit code 1 marks an abnormal end.
            unsafe { TerminateJobObject(self.0, 1) };
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: single close of a handle we own. Kill-on-close fires here, so any tree still
            // running when the caller drops containment is reaped.
            unsafe { CloseHandle(self.0) };
        }
    }

    /// Assign a freshly-spawned child (and thus its descendants) to a new kill-on-close job.
    pub fn contain(child: &std::process::Child) -> Containment {
        assign(child.as_raw_handle() as HANDLE)
    }

    /// Same, for a `tokio::process::Child` (the async spawn path — the verify gate).
    pub fn contain_tokio(child: &tokio::process::Child) -> Containment {
        match child.raw_handle() {
            Some(h) => assign(h as HANDLE),
            None => Containment::None, // already reaped: nothing to contain
        }
    }

    fn assign(process: HANDLE) -> Containment {
        let Some(job) = Job::kill_on_close() else {
            return Containment::None;
        };
        // SAFETY: both handles are live — the job is owned locally, the process handle belongs to a
        // child the caller still owns.
        let ok = unsafe { AssignProcessToJobObject(job.0, process) != 0 };
        if ok {
            Containment::Job(job)
        } else {
            Containment::None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    /// The shell wrapper Aizen actually uses, so the test exercises the real spawn shape.
    fn shell(command: &str) -> Command {
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.arg("/C").arg(command);
            c
        } else {
            let mut c = Command::new("sh");
            c.arg("-c").arg(command);
            c
        };
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    #[test]
    fn containment_is_available_on_this_platform() {
        // Not a tautology: on Windows this asserts the Job Object APIs actually work here, which is
        // the whole basis of tree-kill. A platform without them degrades to killing the direct
        // child, so the assertion is scoped to the platforms we claim support for.
        let mut cmd = shell(if cfg!(windows) { "echo hi" } else { "echo hi" });
        prepare(&mut cmd);
        let mut child = cmd.spawn().expect("spawn shell");
        let c = contain(&child);
        assert!(
            c.is_contained(),
            "job/group containment must be available: {c:?}"
        );
        kill_tree(&mut child, &c);
    }

    /// A long-lived grandchild that keeps the inherited pipe open after the wrapper is killed.
    ///
    /// The choice is empirical, not stylistic. `cmd /C ping -n 40 …` — the obvious portable
    /// busy-wait — turned out to DIE with its wrapper on Windows 11 26200, so a test built on it
    /// passed whether or not tree-kill worked. A `powershell Start-Sleep` grandchild was measured
    /// surviving the wrapper kill *and* holding the pipe (reader blocked), which is the state this
    /// module exists to escape. Absolute path: PATH is not guaranteed to carry System32.
    fn sleeper_command() -> String {
        if cfg!(windows) {
            let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
            format!(
                r"{root}\System32\WindowsPowerShell\v1.0\powershell.exe -NoProfile -Command Start-Sleep -Seconds 40"
            )
        } else {
            // Background + wait, so `sh` does NOT exec-replace itself with `sleep`: we need a real
            // grandchild, otherwise killing the direct child would already end everything.
            "sleep 40 & wait".to_string()
        }
    }

    /// Read to EOF on a thread, reporting whether it finished within `wait`.
    fn reader_finished_within(
        rx: &std::sync::mpsc::Receiver<usize>,
        wait: std::time::Duration,
    ) -> bool {
        rx.recv_timeout(wait).is_ok()
    }

    #[test]
    fn kill_tree_closes_the_pipes_so_a_reader_sees_eof() {
        // The regression this module exists for: a grandchild outliving the `cmd.exe` wrapper holds
        // the inherited write end, so `read_to_end` never sees EOF and the drain thread blocks
        // forever.
        //
        // The test carries its own NEGATIVE CONTROL, because an earlier version of it silently
        // proved nothing: first we kill ONLY the direct child and require the reader to still be
        // blocked (⇒ a descendant really does hold the pipe — the precondition), and only then do we
        // terminate the tree and require the reader to complete. If containment ever regresses to
        // "kill the wrapper", the second half fails; if the fixture stops reproducing the hang, the
        // first half fails instead of quietly passing.
        use std::io::Read;
        let mut cmd = shell(&sleeper_command());
        prepare(&mut cmd);
        let mut child = cmd.spawn().expect("spawn shell");
        let containment = contain(&child);
        let mut out = child.stdout.take().expect("piped stdout");

        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            let _ = tx.send(buf.len());
        });

        // Let the grandchild come up and inherit the pipe.
        std::thread::sleep(std::time::Duration::from_millis(900));

        // Kill the direct child ONLY — the old behaviour.
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            !reader_finished_within(&rx, std::time::Duration::from_millis(1500)),
            "fixture no longer reproduces the hang: the pipe closed after killing just the wrapper, \
             so the tree-kill assertion below would prove nothing. Fix the fixture, not the assert."
        );

        // Now the real thing.
        terminate_tree(&containment);
        assert!(
            reader_finished_within(&rx, std::time::Duration::from_secs(15)),
            "pipe reader still blocked after tree-kill — a descendant kept the write end open"
        );
    }

    /// `output_bounded_bytes` must hand back stdout VERBATIM.
    ///
    /// The Time Machine reads file contents through `git cat-file blob`, so a lossy UTF-8 decode
    /// anywhere on that path silently rewrites every invalid byte to U+FFFD — corrupting any
    /// non-UTF-8 file a checkpoint captures, diffs, or restores. This pins the guarantee that made
    /// it safe to route the Time Machine's git calls through the bounded runner at all.
    #[test]
    fn bounded_bytes_preserves_invalid_utf8() {
        // 0xFF is never valid UTF-8. Getting a child to emit one, portably, is fiddlier than it
        // looks, and two earlier fixtures failed for reasons worth recording so nobody re-tries them:
        // a PowerShell `[Console]::OpenStandardOutput().Write(…)` one-liner is refused outright by
        // AMSI ("script contains malicious content"), and `cmd /C type "<path>"` mis-parses the
        // quotes Rust adds around an argument containing them.
        //
        // So: write the byte to a file, then read it back through the platform's cat with the
        // child's WORKING DIRECTORY set and the bare filename as its own argument. No path — and
        // therefore no quoting — ever reaches the command line, and the generated filename has no
        // spaces. `dir`/`file` are captured by value so each call gets an independent Command.
        let dir = std::env::temp_dir();
        let name = format!(
            "aizen-proctree-bytes-{}-{}.bin",
            std::process::id(),
            crate::core::persist::unique_sequence()
        );
        std::fs::write(dir.join(&name), [0x41u8, 0xFF, 0x42]).expect("write fixture");
        let emit = || -> Command {
            let mut c = if cfg!(windows) {
                // `type` is a cmd builtin, so cmd is unavoidable here — but with cwd set, the only
                // arguments are the literal words `type` and the filename.
                let mut c = Command::new("cmd");
                c.arg("/C").arg("type").arg(&name);
                c
            } else {
                let mut c = Command::new("cat");
                c.arg(&name);
                c
            };
            c.current_dir(&dir);
            c
        };
        let mut cmd = emit();
        let out = output_bounded_bytes(
            &mut cmd,
            std::time::Duration::from_secs(20),
            std::time::Duration::from_secs(2),
        )
        .expect("bounded run");
        assert!(!out.timed_out, "trivial command must not hit the deadline");
        // Precondition: the fixture actually produced output. Without this, an unlaunchable shell
        // makes the byte assertion below fail for the wrong reason.
        assert!(
            !out.stdout.is_empty(),
            "fixture produced no output (code={:?}, stderr={:?}) — the shell never ran, so this \
             test proves nothing about byte fidelity",
            out.code,
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            out.stdout.contains(&0xFF),
            "raw 0xFF byte was not preserved (stdout={:?}) — a lossy decode crept back in",
            out.stdout
        );
        // And the lossy wrapper must still be lossy, proving the two shapes are genuinely distinct
        // — same fixture, so the only difference under test is the decode.
        let mut cmd2 = emit();
        let lossy = output_bounded(
            &mut cmd2,
            std::time::Duration::from_secs(20),
            std::time::Duration::from_secs(2),
        )
        .expect("bounded run");
        assert!(
            lossy.stdout.contains('\u{FFFD}'),
            "the String wrapper should have replaced the invalid byte, got {:?}",
            lossy.stdout
        );
        let _ = std::fs::remove_file(dir.join(&name));
    }

    /// The deadline must actually fire and report `timed_out`, converting a wedged child into a
    /// RETURN rather than a permanent park. This is the property the whole lock fix rests on: a
    /// caller holding `transaction.lock` can only release it by returning.
    #[test]
    fn bounded_run_returns_on_deadline_instead_of_blocking() {
        // Reuse the MEASURED long-lived fixture rather than `ping`: this module's own notes record
        // that `ping` dies with its wrapper here, and it may not even be on PATH — either way the
        // child would exit immediately and the deadline would never be exercised.
        let mut cmd = shell(&sleeper_command());
        let start = std::time::Instant::now();
        let out = output_bounded_bytes(
            &mut cmd,
            std::time::Duration::from_millis(700),
            std::time::Duration::from_secs(2),
        )
        .expect("bounded run must return, not block");
        let elapsed = start.elapsed();
        assert!(
            out.timed_out,
            "a 40s command under a 0.7s deadline must report timed_out"
        );
        assert_eq!(out.code, None, "a killed child has no exit code");
        assert!(!out.success(), "a timed-out run must not look successful");
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "returned only after {elapsed:?} — the deadline did not preempt the child"
        );
    }
}
