//! macOS backend — Seatbelt via `/usr/bin/sandbox-exec`, plus rlimits.
//!
//! Scope, stated exactly:
//! * **fs write**: kernel-enforced allow-list (workspace, private temp, cache roots, `/dev`,
//!   the per-user `/private/var/folders` scratch tree).
//! * **network**: kernel-enforced deny when the policy denies it.
//! * **fs read**: PARTIAL — reads stay broadly allowed except the credential directories, which
//!   are explicitly denied. A full read allow-list breaks too many system frameworks to ship
//!   blind; the honest label is `partial` and `sandbox status` says so.
//! * `sandbox-exec` is deprecated-but-present API surface; the probe checks the binary exists and
//!   is executable at runtime rather than assuming any particular macOS version.
//!
//! NOT runtime-verified by the author's machines (no macOS host) — the profile is exercised by CI
//! on a macOS runner (`sandbox-tests` job), and `strict` trusts only the runtime probe.

use crate::sandbox::capabilities::{BackendKind, CapabilityReport, Enforcement};
use crate::sandbox::policy::FsPolicy;
use std::path::Path;

const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// Is Seatbelt usable here? Existence + executability of the system binary; nothing else is
/// assumed.
pub fn available() -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(SANDBOX_EXEC)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

pub fn probe() -> CapabilityReport {
    if !available() {
        return crate::sandbox::capabilities::guarded_report(vec![
            "/usr/bin/sandbox-exec is missing — Seatbelt backend unavailable on this host"
                .to_string(),
        ]);
    }
    CapabilityReport {
        backend: BackendKind::Macos,
        fs_read: Enforcement::Partial,
        fs_write: Enforcement::Enforced,
        network_deny: Enforcement::Enforced,
        env_isolation: Enforcement::Enforced,
        process_containment: Enforcement::Enforced,
        resource_limits: Enforcement::Partial,
        notes: vec![
            "fs read is partial: reads are allowed except the credential directories (a full read \
             allow-list breaks system frameworks)"
                .to_string(),
            "resource limits are partial: rlimits apply, but RLIMIT_AS is not reliably enforced \
             by XNU"
                .to_string(),
        ],
    }
}

/// Escape one path for a Seatbelt string literal.
fn sb_escape(p: &Path) -> String {
    p.display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// Build the Seatbelt profile for one spawn. Deny-write-by-default with an allow-list; network
/// denied when the policy says so; credential directories unreadable in every case.
pub fn profile(fs: &FsPolicy, deny_network: bool) -> String {
    let mut s = String::from("(version 1)\n(allow default)\n");
    if deny_network {
        s.push_str("(deny network*)\n");
    }
    // Writes: deny everywhere, then re-allow the policy's roots. Later rules win in Seatbelt, so
    // the order deny-then-allow yields "workspace-write".
    s.push_str("(deny file-write*)\n");
    for root in &fs.read_write {
        s.push_str(&format!(
            "(allow file-write* (subpath \"{}\"))\n",
            sb_escape(root)
        ));
    }
    // The per-user scratch tree and devices every process touches.
    s.push_str("(allow file-write* (subpath \"/private/var/folders\"))\n");
    s.push_str("(allow file-write* (subpath \"/dev\"))\n");
    // Credential surfaces stay unreadable even under the broad read default — LAST so they win.
    for d in &fs.deny {
        s.push_str(&format!(
            "(deny file-read* (subpath \"{}\"))\n",
            sb_escape(d)
        ));
    }
    s
}

/// Rewrite `(program, args)` to run under `sandbox-exec` with `profile`.
pub fn wrap(
    program: &Path,
    args: &[String],
    profile_text: &str,
) -> (std::path::PathBuf, Vec<String>) {
    let mut new_args = vec!["-p".to_string(), profile_text.to_string()];
    new_args.push(program.display().to_string());
    new_args.extend(args.iter().cloned());
    (std::path::PathBuf::from(SANDBOX_EXEC), new_args)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_shape_denies_then_allows() {
        let fs = FsPolicy {
            read_write: vec!["/ws/proj".into(), "/tmp/aizen-sbx-1".into()],
            read_only: vec![],
            deny: vec!["/Users/u/.ssh".into(), "/Users/u/.aws".into()],
        };
        let p = profile(&fs, true);
        assert!(p.starts_with("(version 1)"));
        assert!(p.contains("(deny network*)"));
        assert!(p.contains("(deny file-write*)"));
        assert!(p.contains("(allow file-write* (subpath \"/ws/proj\"))"));
        // Deny rules for credential dirs come AFTER the allow-default so they win.
        let read_deny = p
            .find("(deny file-read* (subpath \"/Users/u/.ssh\"))")
            .unwrap();
        let allow_default = p.find("(allow default)").unwrap();
        assert!(read_deny > allow_default);
        // No network-deny line when the policy allows it.
        assert!(!profile(&fs, false).contains("deny network"));
    }

    #[test]
    fn wrap_prefixes_sandbox_exec() {
        let (prog, args) = wrap(
            Path::new("/bin/sh"),
            &["-c".into(), "id".into()],
            "(version 1)",
        );
        assert_eq!(prog, std::path::PathBuf::from(SANDBOX_EXEC));
        assert_eq!(args[0], "-p");
        assert_eq!(args[2], "/bin/sh");
        assert_eq!(args[3], "-c");
    }

    #[test]
    fn escaping_handles_quotes() {
        assert_eq!(sb_escape(Path::new("/a\"b")), "/a\\\"b");
    }
}
