//! Per-platform enforcement. Each backend exposes:
//!
//! * `probe()` — an honest [`super::capabilities::CapabilityReport`] for this host;
//! * an `apply_*` surface the runner calls while building a `Command`.
//!
//! Only the backend for the compile target is built; the runner reaches them through
//! target-gated calls so a Windows binary carries no Linux syscall table and vice versa.

pub mod guarded;

#[cfg(target_os = "linux")]
pub mod linux;

#[cfg(windows)]
pub mod windows;

#[cfg(target_os = "macos")]
pub mod macos;
