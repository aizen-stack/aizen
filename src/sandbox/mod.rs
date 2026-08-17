//! OS-level sandbox for every child process the model or the repository can influence.
//!
//! This layer sits BELOW approval and `cmd_guard`, and answers a different question. Approval asks
//! "did the user agree to run this?"; `cmd_guard` refuses a short list of catastrophic commands
//! outright. The sandbox asks: **even if the command is malicious or compromised, what can it
//! actually reach?** — and enforces the answer with kernel mechanisms where the platform has them,
//! honestly reporting where it does not.
//!
//! # Structure
//!
//! * [`policy`] — the user's `sandbox` config section, environment scrubbing, filesystem roots.
//! * [`request`] — [`request::SandboxRequest`]: everything the runner needs to know about one spawn.
//! * [`capabilities`] — what THIS platform can enforce, probed once and reported without inflation.
//! * [`runner`] — the one place a sandboxed `Command` is built: shell wrapping, env scrubbing,
//!   private temp, backend hardening, strict-mode fail-closed checks, audit.
//! * [`audit`] — owner-only JSONL log of every sandboxed spawn under `~/.aizen/audit/`.
//! * [`backend`] — per-platform enforcement (Linux Landlock/seccomp/rlimit; Windows Job Object
//!   limits; macOS Seatbelt; the software-only guarded floor everywhere).
//!
//! # What the model can and cannot choose
//!
//! Call sites pick the [`CommandOrigin`]; the model only ever supplies command STRINGS and the
//! declared `network` capability (which is an approval-gated escalation, never a default). There is
//! no `bypass_sandbox` flag anywhere, and [`CommandOrigin::InternalTrusted`] is constructed only by
//! Aizen's own code with a literal reason — a tool argument cannot name it.
//!
//! # Honesty contract
//!
//! Every capability is reported as one of `enforced` / `partial` / `advisory` / `unavailable`
//! (see [`capabilities::Enforcement`]). `strict` mode refuses to spawn rather than run with less
//! than kernel enforcement; `auto` degrades and says so. Nothing here claims to protect against a
//! kernel exploit, an already-compromised account, or secrets the user placed inside the workspace.

pub mod audit;
pub mod backend;
pub mod capabilities;
pub mod policy;
pub mod request;
pub mod runner;

use serde::{Deserialize, Serialize};
use std::str::FromStr;
use std::sync::atomic::{AtomicU8, Ordering};

/// How much the OS is asked to enforce around a child process.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    /// Strongest available backend; interactive sessions may fall back to `guarded` (warned once
    /// per session). Cron / hosted-bot / non-TTY runs fail closed instead of falling back, unless
    /// `sandbox.allow_guarded_fallback` is set.
    #[default]
    Auto,
    /// Kernel enforcement or nothing: if the platform cannot enforce the requested capabilities,
    /// the spawn is refused BEFORE it starts. Never self-downgrades; `/yolo` does not weaken it.
    Strict,
    /// Software guards only (approval, cmd_guard, env scrubbing, timeouts, tree-kill, resource
    /// limits where the OS offers them) — explicitly NOT a kernel sandbox, and reported as such.
    Guarded,
    /// OS sandbox disabled. The hard command floor, approval, process-tree containment, timeouts
    /// and the audit log all still apply — but env scrubbing and resource limits are off too, so
    /// children run exactly as they did before this subsystem existed. User-only: nothing the
    /// model outputs can select this mode.
    Off,
}

impl SandboxMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Strict => "strict",
            Self::Guarded => "guarded",
            Self::Off => "off",
        }
    }
}

impl std::fmt::Display for SandboxMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SandboxMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "strict" => Ok(Self::Strict),
            "guarded" => Ok(Self::Guarded),
            "off" => Ok(Self::Off),
            other => Err(format!(
                "unknown sandbox mode '{other}' — use auto, strict, guarded, or off"
            )),
        }
    }
}

/// Who asked for this process. Chosen by the CALL SITE (Aizen's own code), never by the model:
/// the model supplies command strings, not origins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOrigin {
    /// The `shell_run` tool (model-generated foreground command).
    ShellRun,
    /// `process action=start` (model-generated background command).
    ProcessStart,
    /// The post-edit verify gate (`cargo check` / `tsc` / trusted `.aizen/verify.json`).
    VerifyGate,
    /// A language server child (binary from PATH discovery; input is repository content).
    Lsp,
    /// A `` !`cmd` `` expansion inside a user-defined custom command (repository-influenced).
    CustomCommand,
    /// The user's own `!cmd` escape typed at the REPL prompt.
    UserEscape,
    /// `/sh` (or an agent shell) arriving through a hosted bot lane (Telegram/Discord). Remote:
    /// fails closed when no kernel backend exists, unless the user opted into guarded fallback.
    TelegramShell,
    /// A scheduled (cron) run that spawns a command DIRECTLY. Today a cron job runs a whole agent
    /// turn instead, which marks the process unattended ([`set_process_unattended`]) and issues
    /// ordinary [`Self::ShellRun`] calls — so this variant is reserved, not yet constructed.
    #[allow(dead_code)]
    Cron,
    /// A delegated sub-agent's command. Reserved: a sub-agent's `shell_run` reaches the runner as
    /// [`Self::ShellRun`] with the child's resource scope in the audit line; policy inheritance is
    /// structural (mode is process-global, approval is inherited), so a child cannot escalate.
    #[allow(dead_code)]
    SubAgent,
    /// A workflow stage's command. Same reservation as [`Self::SubAgent`].
    #[allow(dead_code)]
    Workflow,
    /// An MCP stdio server process (configured command; trust-gated per project elsewhere).
    McpStdio,
    /// An Aizen-internal spawn whose program AND arguments are Aizen-controlled (git plumbing,
    /// `systemctl` service management, opening the OAuth browser). Carries a literal reason for
    /// the audit trail. NOT reachable from any tool argument.
    InternalTrusted(&'static str),
}

impl CommandOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ShellRun => "shell_run",
            Self::ProcessStart => "process_start",
            Self::VerifyGate => "verify_gate",
            Self::Lsp => "lsp",
            Self::CustomCommand => "custom_command",
            Self::UserEscape => "user_escape",
            Self::TelegramShell => "telegram_shell",
            Self::Cron => "cron",
            Self::SubAgent => "sub_agent",
            Self::Workflow => "workflow",
            Self::McpStdio => "mcp_stdio",
            Self::InternalTrusted(_) => "internal_trusted",
        }
    }

    /// Origins that run UNATTENDED (scheduled or remote): under `auto` these fail closed when the
    /// platform has no kernel backend, instead of silently degrading to software guards, unless
    /// the user set `sandbox.allow_guarded_fallback = true`.
    pub fn unattended(&self) -> bool {
        matches!(self, Self::Cron | Self::TelegramShell)
    }
}

/// Marks the WHOLE process as unattended (a `cron run` invocation, the `serve` daemon). Every
/// spawn then falls under the fail-closed rule regardless of its per-call origin — an agent turn
/// started by cron issues plain `shell_run` calls, and those must not read as interactive.
static PROCESS_UNATTENDED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn set_process_unattended() {
    PROCESS_UNATTENDED.store(true, Ordering::Relaxed);
}

pub fn process_unattended() -> bool {
    PROCESS_UNATTENDED.load(Ordering::Relaxed)
}

// ── process-wide effective mode ──────────────────────────────────────────────
// The mode is resolved once from config at startup (`init_mode`) and may be changed at runtime by
// the USER only (`/sandbox`, `--sandbox`, `AIZEN_SANDBOX`). Stored as an atomic so tool bodies on
// blocking threads read it without locks.

const MODE_UNSET: u8 = u8::MAX;
static RUNTIME_MODE: AtomicU8 = AtomicU8::new(MODE_UNSET);

fn encode(m: SandboxMode) -> u8 {
    match m {
        SandboxMode::Auto => 0,
        SandboxMode::Strict => 1,
        SandboxMode::Guarded => 2,
        SandboxMode::Off => 3,
    }
}

fn decode(v: u8) -> Option<SandboxMode> {
    match v {
        0 => Some(SandboxMode::Auto),
        1 => Some(SandboxMode::Strict),
        2 => Some(SandboxMode::Guarded),
        3 => Some(SandboxMode::Off),
        _ => None,
    }
}

/// Set the process-wide mode (startup config load, `--sandbox`, `/sandbox`). User-driven only.
pub fn set_mode(m: SandboxMode) {
    RUNTIME_MODE.store(encode(m), Ordering::Relaxed);
}

/// The effective mode: the runtime override if one was set, else `AIZEN_SANDBOX`, else the
/// persisted config value, else `auto`.
pub fn mode() -> SandboxMode {
    if let Some(m) = decode(RUNTIME_MODE.load(Ordering::Relaxed)) {
        return m;
    }
    if let Ok(v) = std::env::var("AIZEN_SANDBOX") {
        if let Ok(m) = v.parse::<SandboxMode>() {
            return m;
        }
    }
    crate::core::cli_config::load()
        .sandbox
        .and_then(|s| s.mode)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parses_and_displays() {
        assert_eq!("auto".parse(), Ok(SandboxMode::Auto));
        assert_eq!("STRICT".parse(), Ok(SandboxMode::Strict));
        assert_eq!("guarded".parse(), Ok(SandboxMode::Guarded));
        assert_eq!("off".parse(), Ok(SandboxMode::Off));
        assert!("yolo".parse::<SandboxMode>().is_err());
        assert_eq!(SandboxMode::Strict.to_string(), "strict");
    }

    #[test]
    fn unattended_origins_are_exactly_the_remote_and_scheduled_ones() {
        assert!(CommandOrigin::Cron.unattended());
        assert!(CommandOrigin::TelegramShell.unattended());
        for o in [
            CommandOrigin::ShellRun,
            CommandOrigin::ProcessStart,
            CommandOrigin::VerifyGate,
            CommandOrigin::Lsp,
            CommandOrigin::CustomCommand,
            CommandOrigin::UserEscape,
            CommandOrigin::SubAgent,
            CommandOrigin::Workflow,
            CommandOrigin::McpStdio,
            CommandOrigin::InternalTrusted("test"),
        ] {
            assert!(!o.unattended(), "{o:?} must not be unattended");
        }
    }

    #[test]
    fn mode_roundtrips_through_the_atomic() {
        // Serialized against other tests via the encode/decode pair rather than the global (tests
        // run in parallel; only this test writes the global, and it restores the unset state).
        for m in [
            SandboxMode::Auto,
            SandboxMode::Strict,
            SandboxMode::Guarded,
            SandboxMode::Off,
        ] {
            assert_eq!(decode(encode(m)), Some(m));
        }
        assert_eq!(decode(MODE_UNSET), None);
    }
}
