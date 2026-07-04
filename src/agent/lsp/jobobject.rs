//! Windows Job Object: guarantees a language server's WHOLE process tree dies with us.
//!
//! Node-based servers (pyright, typescript-language-server) install as `.cmd` shims, so the child
//! we spawn is `cmd.exe`, which then spawns the real `node.exe`. `kill_on_drop`/`TerminateProcess`
//! only kill the direct child — the `node` process would be orphaned and keep running (and holding
//! RAM) after Aizen exits or `/lsp off`. The fix (the standard one, used by every serious Windows
//! process supervisor): put the child in a Job Object configured with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` — when the job handle closes (our [`Job`] drops, or the
//! Aizen process dies and the OS closes its handles), the kernel terminates every process in the
//! job. Works even on hard crashes, which per-process cleanup code never can.
//!
//! Everything is best-effort: any API failure yields `None` and we fall back to plain
//! `kill_on_drop` (exactly Phase 1 behavior) — never an error, never a crash.
//!
//! This is the only `unsafe` in the codebase; it is confined to four kernel32 calls with
//! locally-checked invariants (valid handle in, struct + size pair match).

#![cfg(windows)]

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};

/// An owned kill-on-close Job Object handle. Dropping it (or the process dying) terminates every
/// process that was assigned to it.
pub struct Job(HANDLE);

// SAFETY: a Job Object HANDLE is a kernel object reference, not thread-affine state; the Win32 job
// APIs are documented thread-safe. We only move it into the mainloop task and close it once (Drop).
unsafe impl Send for Job {}
unsafe impl Sync for Job {}

impl Job {
    /// Create a kill-on-close job. `None` on any failure (caller falls back to `kill_on_drop`).
    fn kill_on_close() -> Option<Self> {
        // SAFETY: null attributes + null name are the documented "anonymous job" arguments; the
        // returned handle is checked before use and owned by `Job` (closed exactly once in Drop).
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return None;
        }
        let job = Job(handle);
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        // SAFETY: `info` is a properly-initialized JOBOBJECT_EXTENDED_LIMIT_INFORMATION and the size
        // argument is exactly its size — the (pointer, length) pair the API requires.
        let ok = unsafe {
            SetInformationJobObject(
                job.0,
                JobObjectExtendedLimitInformation,
                std::ptr::from_ref(&info).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        (ok != 0).then_some(job) // a job without kill-on-close is useless — drop it (closes handle)
    }

    /// Assign a process (by its raw process handle) to this job.
    fn assign(&self, process: HANDLE) -> bool {
        // SAFETY: both handles are live — the job is owned by self, the process handle comes from a
        // just-spawned tokio Child the caller still owns.
        unsafe { AssignProcessToJobObject(self.0, process) != 0 }
    }
}

impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: self.0 is a valid handle we own; this is the single close. Kill-on-close fires
        // here, terminating the assigned process tree.
        unsafe { CloseHandle(self.0) };
    }
}

/// Put a freshly-spawned server child into a kill-on-close job. Returns the job to KEEP ALIVE for
/// the child's lifetime (drop it ⇒ the tree dies). `None` ⇒ job APIs unavailable/failed — caller
/// silently falls back to plain `kill_on_drop` (direct child only).
pub fn contain(child: &tokio::process::Child) -> Option<Job> {
    let job = Job::kill_on_close()?;
    let handle = child.raw_handle()? as HANDLE;
    job.assign(handle).then_some(job)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_creation_smoke() {
        // Creation + configuration must succeed on any supported Windows. (Assigning + killing a
        // real tree is exercised by the ignored end-to-end test via a node server when installed.)
        assert!(Job::kill_on_close().is_some());
    }
}
