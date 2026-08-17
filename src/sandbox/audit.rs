//! Owner-only JSONL audit of every sandboxed spawn — `~/.aizen/audit/sandbox.jsonl`.
//!
//! One line per spawn, appended when the call site reports the outcome (or at spawn time for
//! long-lived servers). Commands are hashed, redacted and truncated; environment VALUES are never
//! written by anyone — the scrub count is a number, not a list. Rotation keeps the log bounded:
//! past ~5 MB the file rolls to `sandbox.1.jsonl` and the previous roll is deleted.

use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

const MAX_BYTES: u64 = 5 * 1024 * 1024;
const MAX_CMD_CHARS: usize = 240;

/// One audit line. Everything here is safe to keep on disk: hashes, enums, counts, and a
/// redacted/truncated command string.
#[derive(Debug, Serialize)]
pub struct AuditRecord {
    /// RFC-3339 UTC timestamp of the SPAWN.
    pub ts: String,
    /// Execution/resource scope (turn correlation).
    pub scope: String,
    /// Command origin (`shell_run`, `verify_gate`, `internal_trusted`, …).
    pub origin: &'static str,
    /// For `internal_trusted` spawns: the literal reason the call site declared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trusted_reason: Option<&'static str>,
    /// SHA-256 of the full command line, first 16 hex chars — join lines without storing secrets.
    pub cmd_hash: String,
    /// The command, secret-redacted and truncated to 240 chars.
    pub cmd: String,
    /// Working directory, relative to the workspace root where possible.
    pub cwd: String,
    /// Mode the config/user asked for.
    pub mode_requested: &'static str,
    /// Mode the spawn actually ran under (`auto` resolves to a concrete backend).
    pub mode_effective: &'static str,
    /// Backend label (`linux-landlock-seccomp`, `windows-job-object`, `guarded-software`, …).
    pub backend: &'static str,
    /// Whether the network capability was granted for this spawn.
    pub network: bool,
    /// How many inherited env vars the scrub withheld (names/values never logged).
    pub env_scrubbed: usize,
    /// Why enforcement is weaker than requested, when it is.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub degraded: Vec<String>,
    /// `spawned` | `exit:<code>` | `timeout` | `cancelled` | `spawn-error` | `refused`.
    pub outcome: String,
    /// Wall-clock duration in ms, where the call site measured one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// Where the log lives (also used by `sandbox status` and the tests).
pub fn audit_dir() -> PathBuf {
    crate::core::config::aizen_home().join("audit")
}

fn audit_file() -> PathBuf {
    audit_dir().join("sandbox.jsonl")
}

/// Truncate + redact a command line for storage. Redaction is deliberately aggressive: long
/// token-shaped runs and known key prefixes become `[redacted]`; over-redacting an audit line is
/// the safe direction.
pub fn redact_command(cmd: &str) -> String {
    use once_cell::sync::Lazy;
    use regex::Regex;
    static SECRETY: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"(?x)
              sk-[A-Za-z0-9_-]{8,}                    # OpenAI-style keys
            | gh[pousr]_[A-Za-z0-9]{16,}              # GitHub tokens
            | xox[baprs]-[A-Za-z0-9-]{10,}            # Slack tokens
            | AKIA[A-Z0-9]{12,}                       # AWS access key ids
            | eyJ[A-Za-z0-9_-]{20,}                   # JWTs
            | [A-Za-z0-9+/_=-]{40,}                   # any long opaque token run
            ",
        )
        .unwrap()
    });
    let red = SECRETY.replace_all(cmd, "[redacted]");
    let mut s: String = red.chars().take(MAX_CMD_CHARS).collect();
    if red.chars().count() > MAX_CMD_CHARS {
        s.push('…');
    }
    s
}

/// First 16 hex chars of SHA-256 — enough to correlate, useless to invert.
pub fn command_hash(cmd: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, cmd.as_bytes());
    digest.as_ref()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Append one record. Serialized in-process; failures are swallowed (an audit line must never
/// break the work it describes) but the first failure per process is surfaced once on stderr.
pub fn append(rec: &AuditRecord) {
    // The unit-test build spawns real sandboxed children; their audit lines must not land in the
    // developer's real `~/.aizen/audit/` (the tests assert behavior, not this file's contents).
    if cfg!(test) {
        return;
    }
    static LOCK: Mutex<()> = Mutex::new(());
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    let _g = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Err(e) = try_append(rec) {
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            eprintln!("aizen: sandbox audit log unavailable: {e}");
        }
    }
}

fn try_append(rec: &AuditRecord) -> anyhow::Result<()> {
    let path = audit_file();
    std::fs::create_dir_all(audit_dir())?;
    rotate_if_needed(&path)?;
    let existed = path.exists();
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    let mut line = serde_json::to_string(rec)?;
    line.push('\n');
    f.write_all(line.as_bytes())?;
    drop(f);
    if !existed {
        // Owner-only from the first line: the log names commands and directories.
        let _ = crate::core::persist::harden_owner_only_checked(&path);
    }
    Ok(())
}

fn rotate_if_needed(path: &std::path::Path) -> anyhow::Result<()> {
    let Ok(meta) = std::fs::metadata(path) else {
        return Ok(());
    };
    if meta.len() < MAX_BYTES {
        return Ok(());
    }
    let rolled = path.with_file_name("sandbox.1.jsonl");
    let _ = std::fs::remove_file(&rolled);
    std::fs::rename(path, &rolled)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redaction_kills_token_shapes_and_truncates() {
        let cmd = "curl -H 'Authorization: Bearer sk-abcdefghijklmnop1234' https://x";
        let red = redact_command(cmd);
        assert!(!red.contains("sk-abcdef"), "key survived: {red}");
        assert!(red.contains("[redacted]"));
        // GitHub token, JWT, long opaque run.
        assert!(
            !redact_command("gh auth login --with-token ghp_ABCDEFGHIJKLMNOP1234")
                .contains("ghp_A")
        );
        assert!(!redact_command(&format!("echo {}", "A".repeat(60))).contains(&"A".repeat(41)));
        // Ordinary commands survive readable.
        assert_eq!(redact_command("cargo check"), "cargo check");
        let long = "x".repeat(500);
        assert!(redact_command(&long).chars().count() <= MAX_CMD_CHARS + 1);
    }

    #[test]
    fn hash_is_stable_and_short() {
        assert_eq!(command_hash("cargo check"), command_hash("cargo check"));
        assert_ne!(command_hash("a"), command_hash("b"));
        assert_eq!(command_hash("x").len(), 16);
    }

    #[test]
    fn record_serializes_without_optional_noise() {
        let rec = AuditRecord {
            ts: "2026-08-17T00:00:00Z".into(),
            scope: "repl:main".into(),
            origin: "shell_run",
            trusted_reason: None,
            cmd_hash: command_hash("echo hi"),
            cmd: "echo hi".into(),
            cwd: ".".into(),
            mode_requested: "auto",
            mode_effective: "guarded",
            backend: "guarded-software",
            network: false,
            env_scrubbed: 4,
            degraded: vec![],
            outcome: "exit:0".into(),
            duration_ms: Some(12),
        };
        let json = serde_json::to_string(&rec).unwrap();
        assert!(!json.contains("trusted_reason"));
        assert!(!json.contains("degraded"));
        assert!(json.contains("\"env_scrubbed\":4"));
    }
}
