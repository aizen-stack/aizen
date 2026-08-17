//! What THIS platform can actually enforce — probed once, cached, and never inflated.
//!
//! Every capability is reported on the four-level scale the whole subsystem speaks:
//! [`Enforcement::Enforced`] (the kernel stops it), [`Enforcement::Partial`] (the kernel stops
//! some of it), [`Enforcement::Advisory`] (software checks only — a determined child bypasses
//! them), [`Enforcement::Unavailable`] (nothing even checks). `sandbox status` prints this
//! verbatim; `strict` mode consults it before every spawn and refuses when the requested policy
//! cannot be kernel-enforced.

use serde::Serialize;

/// How hard one capability is actually held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Enforcement {
    /// The kernel denies the operation regardless of what the child does.
    Enforced,
    /// The kernel denies part of the surface; the rest is software-checked or open. The `detail`
    /// string on the report says which part. (Constructed only by the macOS backend today, so a
    /// Windows/Linux build sees it as never-built — it is still part of the reporting contract.)
    #[allow(dead_code)]
    Partial,
    /// Software checks before/around the spawn only — no kernel backstop.
    Advisory,
    /// Not even software checks exist for this capability on this platform.
    Unavailable,
}

impl Enforcement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Enforced => "enforced",
            Self::Partial => "partial",
            Self::Advisory => "advisory",
            Self::Unavailable => "unavailable",
        }
    }
    /// Is this good enough for `strict` (which promises kernel enforcement)?
    pub fn kernel_backed(self) -> bool {
        matches!(self, Self::Enforced | Self::Partial)
    }
}

/// Which backend would run a spawn on this platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// Linux: Landlock (filesystem) + seccomp (network) + rlimits + no_new_privs.
    /// (Constructed only in the Linux backend, which other targets compile out.)
    #[allow(dead_code)]
    Linux,
    /// Windows: Job Object containment/limits + env scrubbing. Filesystem and network have no
    /// kernel enforcement without AppContainer (not implemented) — reported honestly below.
    #[allow(dead_code)]
    Windows,
    /// macOS: Seatbelt (`sandbox-exec`) profile + rlimits. (macOS backend only.)
    #[allow(dead_code)]
    Macos,
    /// Software guards only (also the fallback when a kernel backend probe fails).
    Guarded,
}

impl BackendKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Linux => "linux-landlock-seccomp",
            Self::Windows => "windows-job-object",
            Self::Macos => "macos-seatbelt",
            Self::Guarded => "guarded-software",
        }
    }
}

/// The full capability matrix for the strongest backend available here.
#[derive(Debug, Clone, Serialize)]
pub struct CapabilityReport {
    pub backend: BackendKind,
    /// Reading files outside the allowed roots.
    pub fs_read: Enforcement,
    /// Writing/deleting files outside the workspace + private temp + cache roots.
    pub fs_write: Enforcement,
    /// Opening outbound network connections when the policy denies them.
    pub network_deny: Enforcement,
    /// Children not receiving Aizen's secrets in their environment.
    pub env_isolation: Enforcement,
    /// Kill-the-whole-tree containment (timeout, cancel, crash).
    pub process_containment: Enforcement,
    /// Process-count / memory / CPU ceilings.
    pub resource_limits: Enforcement,
    /// Why anything above is less than `enforced`, in one honest sentence per line.
    pub notes: Vec<String>,
}

/// Probe the strongest backend available on this host. Cached: the answer cannot change within a
/// process lifetime (kernel features don't appear mid-run).
pub fn probe() -> &'static CapabilityReport {
    static REPORT: once_cell::sync::Lazy<CapabilityReport> = once_cell::sync::Lazy::new(probe_now);
    &REPORT
}

fn probe_now() -> CapabilityReport {
    #[cfg(target_os = "linux")]
    {
        return super::backend::linux::probe();
    }
    #[cfg(windows)]
    {
        return super::backend::windows::probe();
    }
    #[cfg(target_os = "macos")]
    {
        return super::backend::macos::probe();
    }
    #[allow(unreachable_code)]
    guarded_report(vec![
        "no kernel sandbox backend exists for this platform".to_string()
    ])
}

/// The software-only report — what `guarded` mode (or a failed kernel probe) can honestly claim.
pub fn guarded_report(mut notes: Vec<String>) -> CapabilityReport {
    notes.push(
        "guarded is a software guard, not a kernel sandbox: a child that ignores policy is only \
         caught where the OS mechanisms below say 'enforced'"
            .to_string(),
    );
    CapabilityReport {
        backend: BackendKind::Guarded,
        fs_read: Enforcement::Advisory,
        fs_write: Enforcement::Advisory,
        network_deny: Enforcement::Advisory,
        env_isolation: Enforcement::Enforced, // scrubbing removes vars before spawn — no bypass
        process_containment: containment_enforcement(),
        resource_limits: Enforcement::Advisory,
        notes,
    }
}

/// Tree containment is kernel-backed on both main platforms (Job Object / process groups) — the
/// one mechanism the codebase already exercises everywhere.
pub fn containment_enforcement() -> Enforcement {
    if cfg!(any(windows, unix)) {
        Enforcement::Enforced
    } else {
        Enforcement::Unavailable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_returns_the_platform_backend() {
        let r = probe();
        #[cfg(windows)]
        assert_eq!(r.backend, BackendKind::Windows);
        #[cfg(target_os = "linux")]
        assert!(matches!(
            r.backend,
            BackendKind::Linux | BackendKind::Guarded
        ));
        #[cfg(target_os = "macos")]
        assert!(matches!(
            r.backend,
            BackendKind::Macos | BackendKind::Guarded
        ));
        // Env isolation is scrub-based and platform-independent: always enforceable.
        assert_eq!(r.env_isolation, Enforcement::Enforced);
    }

    #[test]
    fn strict_gate_levels() {
        assert!(Enforcement::Enforced.kernel_backed());
        assert!(Enforcement::Partial.kernel_backed());
        assert!(!Enforcement::Advisory.kernel_backed());
        assert!(!Enforcement::Unavailable.kernel_backed());
    }

    #[test]
    fn report_serializes_lowercase() {
        let json = serde_json::to_string(&guarded_report(vec![])).unwrap();
        assert!(json.contains("\"guarded-software\"") || json.contains("\"guarded\""));
        assert!(json.contains("\"advisory\""));
    }
}
