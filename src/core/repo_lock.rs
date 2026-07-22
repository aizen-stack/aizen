//! Cross-process advisory locks used by repository and shared-state transactions.
//!
//! A Rust `Mutex` only serializes threads in one process. Aizen state is also reachable from sibling
//! agents, workflows, CLI invocations, daemons, and linked worktrees, so coordination that protects
//! persistent state must use an OS lock. The lock handle is the authority: diagnostic owner files may
//! become stale after a hard kill, but an OS lock is released automatically when its process exits.

use anyhow::{Context, Result};
use std::fmt;
use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    Shared,
    Exclusive,
}

#[derive(Debug)]
pub struct LockBusy {
    pub path: PathBuf,
    pub mode: LockMode,
    pub timeout: Duration,
}

impl fmt::Display for LockBusy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mode = match self.mode {
            LockMode::Shared => "shared",
            LockMode::Exclusive => "exclusive",
        };
        write!(
            f,
            "resource is busy (could not acquire {mode} lock {} within {}s); nothing was changed",
            self.path.display(),
            self.timeout.as_secs_f64()
        )
    }
}

impl std::error::Error for LockBusy {}

#[derive(Debug)]
pub struct RepoTxnLock {
    file: File,
    mode: LockMode,
}

impl RepoTxnLock {
    /// Backward-compatible exclusive acquisition used by existing Time Machine and recovery callers.
    pub fn acquire(path: &Path, timeout: Duration) -> Result<Self> {
        Self::acquire_mode(path, LockMode::Exclusive, timeout, None)
    }

    pub fn acquire_shared(path: &Path, timeout: Duration) -> Result<Self> {
        Self::acquire_mode(path, LockMode::Shared, timeout, None)
    }

    pub fn acquire_exclusive(path: &Path, timeout: Duration) -> Result<Self> {
        Self::acquire_mode(path, LockMode::Exclusive, timeout, None)
    }

    /// Acquire while observing an optional turn cancellation token. Cancellation is checked between
    /// non-blocking OS lock attempts, so waiting never strands a tool after Esc.
    pub fn acquire_mode(
        path: &Path,
        mode: LockMode,
        timeout: Duration,
        cancel: Option<&crate::core::cancel::TurnCancel>,
    ) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .with_context(|| format!("opening transaction lock {}", path.display()))?;

        let start = Instant::now();
        loop {
            if cancel.is_some_and(crate::core::cancel::TurnCancel::is_cancelled) {
                anyhow::bail!("cancelled while waiting for lock {}; nothing was changed", path.display());
            }
            match try_lock(&file, mode) {
                Ok(true) => return Ok(Self { file, mode }),
                Ok(false) if start.elapsed() < timeout => {
                    std::thread::sleep(Duration::from_millis(25));
                }
                Ok(false) => {
                    return Err(LockBusy { path: path.to_path_buf(), mode, timeout }.into());
                }
                Err(e) => return Err(e).with_context(|| format!("locking {}", path.display())),
            }
        }
    }

    pub fn mode(&self) -> LockMode {
        self.mode
    }
}

impl Drop for RepoTxnLock {
    fn drop(&mut self) {
        let _ = unlock(&self.file);
    }
}

#[cfg(windows)]
fn try_lock(file: &File, mode: LockMode) -> std::io::Result<bool> {
    use std::mem::zeroed;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::ERROR_LOCK_VIOLATION;
    use windows_sys::Win32::Storage::FileSystem::{LockFileEx, LOCKFILE_EXCLUSIVE_LOCK, LOCKFILE_FAIL_IMMEDIATELY};
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    let flags = LOCKFILE_FAIL_IMMEDIATELY
        | if mode == LockMode::Exclusive { LOCKFILE_EXCLUSIVE_LOCK } else { 0 };
    let ok = unsafe {
        LockFileEx(
            file.as_raw_handle(),
            flags,
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if ok != 0 {
        return Ok(true);
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(ERROR_LOCK_VIOLATION as i32) {
        Ok(false)
    } else {
        Err(err)
    }
}

#[cfg(windows)]
fn unlock(file: &File) -> std::io::Result<()> {
    use std::mem::zeroed;
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::UnlockFileEx;
    use windows_sys::Win32::System::IO::OVERLAPPED;

    let mut overlapped: OVERLAPPED = unsafe { zeroed() };
    let ok = unsafe {
        UnlockFileEx(
            file.as_raw_handle(),
            0,
            u32::MAX,
            u32::MAX,
            &mut overlapped,
        )
    };
    if ok == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn try_lock(file: &File, mode: LockMode) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    const LOCK_SH: i32 = 1;
    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;
    let operation = match mode {
        LockMode::Shared => LOCK_SH,
        LockMode::Exclusive => LOCK_EX,
    } | LOCK_NB;
    let rc = unsafe { flock(file.as_raw_fd(), operation) };
    if rc == 0 {
        return Ok(true);
    }
    let e = std::io::Error::last_os_error();
    if matches!(e.kind(), std::io::ErrorKind::WouldBlock) {
        Ok(false)
    } else {
        Err(e)
    }
}

#[cfg(unix)]
fn unlock(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    const LOCK_UN: i32 = 8;
    let rc = unsafe { flock(file.as_raw_fd(), LOCK_UN) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
extern "C" {
    fn flock(fd: i32, operation: i32) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn lock_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "aizen-repo-lock-{name}-{}-{}.lock",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ))
    }

    #[test]
    fn shared_locks_coexist_and_exclusive_waits() {
        let path = lock_path("modes");
        let a = RepoTxnLock::acquire_shared(&path, Duration::from_millis(100)).unwrap();
        let b = RepoTxnLock::acquire_shared(&path, Duration::from_millis(100)).unwrap();
        assert_eq!(a.mode(), LockMode::Shared);
        assert_eq!(b.mode(), LockMode::Shared);
        let err = RepoTxnLock::acquire_exclusive(&path, Duration::from_millis(30)).unwrap_err();
        assert!(err.downcast_ref::<LockBusy>().is_some(), "{err:#}");
        drop((a, b));
        RepoTxnLock::acquire_exclusive(&path, Duration::from_millis(100)).unwrap();
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn cancellation_stops_a_waiter_without_releasing_owner() {
        let path = lock_path("cancel");
        let owner = RepoTxnLock::acquire_exclusive(&path, Duration::from_millis(100)).unwrap();
        let cancel = crate::core::cancel::TurnCancel::new();
        let (tx, rx) = mpsc::channel();
        let path2 = path.clone();
        let cancel2 = cancel.clone();
        let waiter = std::thread::spawn(move || {
            tx.send(()).unwrap();
            RepoTxnLock::acquire_mode(
                &path2,
                LockMode::Exclusive,
                Duration::from_secs(5),
                Some(&cancel2),
            )
        });
        rx.recv().unwrap();
        cancel.cancel();
        let err = match waiter.join().unwrap() {
            Ok(_) => panic!("cancelled waiter unexpectedly acquired lock"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("cancelled"), "{err:#}");
        let still_busy = RepoTxnLock::acquire_exclusive(&path, Duration::from_millis(30));
        assert!(still_busy.is_err());
        drop(owner);
        let _ = std::fs::remove_file(path);
    }
}
