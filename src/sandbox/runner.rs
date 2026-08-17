//! The one place a sandboxed `Command` is built. Call sites hand in a [`SandboxRequest`]; this
//! module resolves the mode, refuses what `strict` cannot kernel-enforce, wraps the platform
//! shell, scrubs the environment, arms the platform backend, and hands back a ready command plus
//! an audit handle. The battle-tested wait loops (cancel-aware polling, condvars, async timeouts)
//! stay at the call sites — the runner decides *what runs and with which walls*, not *how it is
//! awaited*.

use super::audit::{self, AuditRecord};
use super::backend::guarded::{self, PrivateTmp};
use super::capabilities::{self, BackendKind};
use super::policy::{self, SandboxSettings};
use super::request::{CommandSpec, SandboxRequest};
use super::SandboxMode;
use anyhow::{anyhow, Result};
use std::path::PathBuf;
use std::time::Instant;

/// How one finished (or failed) spawn is reported to the audit log.
#[derive(Debug, Clone, Copy)]
pub enum Outcome {
    Exit(Option<i32>),
    Timeout,
    Cancelled,
    SpawnFailed,
    /// A long-lived server was started; its exit is not awaited here.
    Spawned,
}

impl Outcome {
    fn label(self) -> String {
        match self {
            Self::Exit(Some(c)) => format!("exit:{c}"),
            Self::Exit(None) => "exit:killed".to_string(),
            Self::Timeout => "timeout".to_string(),
            Self::Cancelled => "cancelled".to_string(),
            Self::SpawnFailed => "spawn-error".to_string(),
            Self::Spawned => "spawned".to_string(),
        }
    }
}

/// Audit + private-temp state shared by [`Sandboxed`] and [`SpawnGuard`]. The record is boxed:
/// this state is embedded in long-lived structs (an MCP transport, a background-process entry),
/// and ~15 fields of inline `String`s would bloat every one of them.
struct AuditState {
    rec: Option<Box<AuditRecord>>,
    started: Instant,
    tmp: Option<PrivateTmp>,
}

impl AuditState {
    fn finish(&mut self, outcome: Outcome) {
        if let Some(mut rec) = self.rec.take() {
            rec.outcome = outcome.label();
            rec.duration_ms = Some(self.started.elapsed().as_millis() as u64);
            audit::append(&rec);
        }
        // The private temp is NOT torn down here: a long-lived server audits `spawned` and keeps
        // using its temp. Removal happens when the owning `Sandboxed`/`SpawnGuard` drops.
    }
}

/// A ready-to-spawn sandboxed command. Spawn `command`, contain the child through
/// [`Sandboxed::contain`] (which carries the policy's Job-Object limits on Windows), then report
/// the outcome with [`Sandboxed::finish`] — or move the bookkeeping out via
/// [`Sandboxed::into_guard`] for background processes and servers.
pub struct Sandboxed<C> {
    pub command: C,
    state: AuditState,
    #[cfg(windows)]
    job_limits: Option<crate::core::proctree::windows_job::JobLimits>,
    /// Degradations worth surfacing to the caller (already in the audit record too).
    pub degraded: Vec<String>,
}

impl<C> Sandboxed<C> {
    /// Report the outcome and write the audit line (idempotent).
    pub fn finish(&mut self, outcome: Outcome) {
        self.state.finish(outcome);
    }

    /// Keep only the bookkeeping (audit + private temp) — for spawns that outlive the call site's
    /// stack frame (background processes, language/MCP servers).
    pub fn into_guard(mut self) -> SpawnGuard {
        SpawnGuard {
            state: AuditState {
                rec: self.state.rec.take(),
                started: self.state.started,
                tmp: self.state.tmp.take(),
            },
        }
    }
}

impl Sandboxed<std::process::Command> {
    /// Contain a just-spawned child with the policy's resource ceilings.
    pub fn contain(&self, child: &std::process::Child) -> crate::core::proctree::Containment {
        #[cfg(windows)]
        {
            if let Some(l) = &self.job_limits {
                return crate::core::proctree::windows_job::contain_with_limits(child, l);
            }
        }
        crate::core::proctree::contain(child)
    }
}

impl Sandboxed<tokio::process::Command> {
    pub fn contain_tokio(
        &self,
        child: &tokio::process::Child,
    ) -> crate::core::proctree::Containment {
        #[cfg(windows)]
        {
            if let Some(l) = &self.job_limits {
                return crate::core::proctree::windows_job::contain_tokio_with_limits(child, l);
            }
        }
        crate::core::proctree::contain_tokio(child)
    }
}

/// The detached bookkeeping of a long-lived spawn: finishes the audit line and keeps the private
/// temp alive until the process is done. Dropping it without `finish` writes nothing further (the
/// spawn line was already recorded if the caller chose `Outcome::Spawned`).
pub struct SpawnGuard {
    state: AuditState,
}

impl SpawnGuard {
    pub fn finish(&mut self, outcome: Outcome) {
        self.state.finish(outcome);
    }
}

// ── mode resolution ──────────────────────────────────────────────────────────

struct Resolution {
    effective: &'static str,
    backend: &'static str,
    /// Scrub env + arm limits (everything except `off`).
    guard: bool,
    /// Arm the kernel backend (auto with one available, or strict). Read only by the Linux/macOS
    /// arms of the build macro, so a Windows build sees the field as never-read.
    #[allow(dead_code)]
    kernel: bool,
    degraded: Vec<String>,
}

/// Does this host have a kernel SANDBOX (filesystem or network enforcement), as opposed to only
/// kernel resource containment? This is the bar the unattended fail-closed rule measures against.
fn kernel_sandbox_available() -> bool {
    let r = capabilities::probe();
    r.fs_write.kernel_backed() || r.network_deny.kernel_backed()
}

fn warn_once(msg: &str) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let line = format!("sandbox: {msg}");
    if crate::ui::tui::active() {
        crate::ui::tui::emit_line(&line);
    } else {
        eprintln!("{line}");
    }
}

/// Audit a policy refusal and build the error the caller surfaces verbatim.
fn refusal(req: &SandboxRequest, settings: &SandboxSettings, msg: &str) -> anyhow::Error {
    let mut rec = base_record(req, settings, "refused", "none", vec![], 0);
    rec.outcome = "refused".to_string();
    audit::append(&rec);
    anyhow!("sandbox refused this command: {msg}")
}

fn resolve(req: &SandboxRequest, settings: &SandboxSettings) -> Result<Resolution> {
    let requested = super::mode();
    let report = capabilities::probe();
    let broad_ok = settings.dangerous_allow_broad_workspace.unwrap_or(false);

    match requested {
        SandboxMode::Off => Ok(Resolution {
            effective: "off",
            backend: "disabled",
            guard: false,
            kernel: false,
            degraded: vec![],
        }),
        SandboxMode::Guarded => Ok(Resolution {
            effective: "guarded",
            backend: BackendKind::Guarded.as_str(),
            guard: true,
            kernel: false,
            degraded: vec![],
        }),
        SandboxMode::Strict => {
            if policy::workspace_too_broad(&req.workspace_root) && !broad_ok {
                return Err(refusal(
                    req,
                    settings,
                    "strict: the workspace is a root/home/system directory — a workspace-write \
                     sandbox over it protects nothing. Narrow the workspace, or set \
                     sandbox.dangerous_allow_broad_workspace",
                ));
            }
            if !report.fs_write.kernel_backed() {
                return Err(refusal(
                    req,
                    settings,
                    "strict: this platform cannot kernel-enforce the filesystem policy (no \
                     AppContainer backend on Windows). Use `--sandbox guarded` for software \
                     guards, or run on Linux/macOS for kernel enforcement",
                ));
            }
            let deny_net = !(req.network || policy::network_default_allow(settings));
            if deny_net && !report.network_deny.kernel_backed() {
                return Err(refusal(
                    req,
                    settings,
                    "strict: network deny cannot be kernel-enforced here — refusing rather than \
                     pretending",
                ));
            }
            Ok(Resolution {
                effective: "strict",
                backend: report.backend.as_str(),
                guard: true,
                kernel: true,
                degraded: vec![],
            })
        }
        SandboxMode::Auto => {
            if kernel_sandbox_available() {
                if policy::workspace_too_broad(&req.workspace_root) && !broad_ok {
                    warn_once(
                        "workspace is a root/home/system directory — the filesystem sandbox \
                         around it is nearly meaningless. Narrow the workspace (see `aizen \
                         sandbox status`)",
                    );
                }
                return Ok(Resolution {
                    effective: "auto",
                    backend: report.backend.as_str(),
                    guard: true,
                    kernel: true,
                    degraded: vec![],
                });
            }
            // No kernel sandbox on this host. Unattended origins fail closed unless the user
            // opted into the software fallback; interactive sessions degrade with one warning.
            if (req.origin.unattended() || super::process_unattended())
                && !settings.allow_guarded_fallback.unwrap_or(false)
            {
                return Err(refusal(
                    req,
                    settings,
                    "no kernel sandbox on this platform and this is an unattended run (cron / \
                     hosted bot). Refusing per fail-closed policy — set \
                     sandbox.allow_guarded_fallback=true to accept software guards for \
                     unattended runs",
                ));
            }
            warn_once(
                "no kernel sandbox backend on this platform — commands run with software guards \
                 only (guarded). `aizen sandbox status` shows exactly what is and is not enforced",
            );
            Ok(Resolution {
                effective: "guarded",
                backend: report.backend.as_str(),
                guard: true,
                kernel: false,
                degraded: vec!["auto degraded to guarded: no kernel sandbox backend".to_string()],
            })
        }
    }
}

fn base_record(
    req: &SandboxRequest,
    settings: &SandboxSettings,
    effective: &'static str,
    backend: &'static str,
    degraded: Vec<String>,
    env_scrubbed: usize,
) -> AuditRecord {
    let line = req.display_line();
    let cwd = req
        .cwd
        .strip_prefix(&req.workspace_root)
        .map(|p| {
            let s = p.display().to_string();
            if s.is_empty() {
                ".".to_string()
            } else {
                s
            }
        })
        .unwrap_or_else(|_| req.cwd.display().to_string());
    AuditRecord {
        ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        scope: req.scope.clone(),
        origin: req.origin.as_str(),
        trusted_reason: match req.origin {
            super::CommandOrigin::InternalTrusted(r) => Some(r),
            _ => None,
        },
        cmd_hash: audit::command_hash(&line),
        cmd: audit::redact_command(&line),
        cwd,
        mode_requested: super::mode().as_str(),
        mode_effective: effective,
        backend,
        network: req.network || policy::network_default_allow(settings),
        env_scrubbed,
        degraded,
        outcome: String::new(),
        duration_ms: None,
    }
}

// ── command construction ─────────────────────────────────────────────────────

/// The platform program+argv for a request, before any kernel wrapping.
fn resolve_argv(spec: &CommandSpec) -> (PathBuf, Vec<String>) {
    match spec {
        CommandSpec::Shell { line } => {
            if cfg!(windows) {
                // `chcp 65001>nul` first so legacy builtins emit UTF-8 — the exact wrapper
                // `shell_run` has always used (see its comment).
                (
                    PathBuf::from("cmd"),
                    vec!["/C".into(), format!("chcp 65001>nul & {line}")],
                )
            } else {
                (PathBuf::from("sh"), vec!["-c".into(), line.clone()])
            }
        }
        CommandSpec::Exec {
            program,
            args,
            use_cmd_shim,
        } => {
            if *use_cmd_shim && cfg!(windows) {
                let mut v = vec!["/C".into(), program.display().to_string()];
                v.extend(args.iter().cloned());
                (PathBuf::from("cmd"), v)
            } else {
                (program.clone(), args.clone())
            }
        }
    }
}

macro_rules! build_prepared {
    ($fn_name:ident, $cmd_ty:ty, $new:expr, $proctree_prepare:path, $apply_kernel:ident) => {
        /// Build a sandboxed command of this flavor. Returns `Err` when policy refuses the spawn
        /// (strict without kernel backing, unattended without fallback, broad workspace).
        pub fn $fn_name(req: SandboxRequest) -> Result<Sandboxed<$cmd_ty>> {
            let settings = policy::settings();
            let res = resolve(&req, &settings)?;

            #[allow(unused_mut)]
            let (mut program, mut args) = resolve_argv(&req.spec);
            let tmp = if res.guard && req.private_tmp {
                PrivateTmp::create(&req.scope)
            } else {
                None
            };

            #[allow(unused_mut)]
            let mut degraded = res.degraded.clone();

            #[cfg(target_os = "macos")]
            {
                if res.kernel && super::backend::macos::available() {
                    let deny_net = !(req.network || policy::network_default_allow(&settings));
                    let mut fs = policy::fs_policy(
                        &req.workspace_root,
                        tmp.as_ref().map(|t| t.path()),
                        &settings,
                    );
                    fs.read_write.push(req.cwd.clone());
                    let prof = super::backend::macos::profile(&fs, deny_net);
                    let (p, a) = super::backend::macos::wrap(&program, &args, &prof);
                    program = p;
                    args = a;
                }
            }

            let mut command: $cmd_ty = $new(&program);
            command.args(&args);
            command.current_dir(&req.cwd);

            let mut env_scrubbed = 0usize;
            if res.guard {
                env_scrubbed = policy::scrubbed_count(&settings.pass_env);
                guarded::apply_env(
                    &mut command,
                    &settings.pass_env,
                    tmp.as_ref(),
                    &req.extra_env,
                );
            } else {
                for (k, v) in &req.extra_env {
                    command.env(k, v);
                }
            }

            #[cfg(target_os = "linux")]
            {
                if res.kernel {
                    let deny_net = !(req.network || policy::network_default_allow(&settings));
                    let mut fs = policy::fs_policy(
                        &req.workspace_root,
                        tmp.as_ref().map(|t| t.path()),
                        &settings,
                    );
                    fs.read_write.push(req.cwd.clone());
                    let limits = settings.limits.clone().unwrap_or_default();
                    let sbx = super::backend::linux::LinuxSandbox::build(&fs, deny_net, &limits);
                    if res.effective == "strict" && !sbx.degraded.is_empty() {
                        return Err(refusal(
                            &req,
                            &settings,
                            &format!(
                                "strict requires kernel enforcement, but: {}",
                                sbx.degraded.join("; ")
                            ),
                        ));
                    }
                    degraded.extend(sbx.degraded.iter().cloned());
                    sbx.$apply_kernel(&mut command);
                }
            }

            #[cfg(windows)]
            let job_limits = if res.guard {
                Some(super::backend::windows::job_limits(
                    &settings.limits.clone().unwrap_or_default(),
                ))
            } else {
                None
            };

            $proctree_prepare(&mut command);

            let rec = base_record(
                &req,
                &settings,
                res.effective,
                res.backend,
                degraded.clone(),
                env_scrubbed,
            );
            Ok(Sandboxed {
                command,
                state: AuditState {
                    rec: Some(Box::new(rec)),
                    started: Instant::now(),
                    tmp,
                },
                #[cfg(windows)]
                job_limits,
                degraded,
            })
        }
    };
}

fn new_std(p: &PathBuf) -> std::process::Command {
    std::process::Command::new(p)
}
fn new_tokio(p: &PathBuf) -> tokio::process::Command {
    tokio::process::Command::new(p)
}

build_prepared!(
    prepare_std,
    std::process::Command,
    new_std,
    crate::core::proctree::prepare,
    apply_std
);
build_prepared!(
    prepare_tokio,
    tokio::process::Command,
    new_tokio,
    crate::core::proctree::prepare_tokio,
    apply_tokio
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::CommandOrigin;
    use std::sync::Mutex;

    /// The effective mode is process-global and several tests here flip or depend on it, so they
    /// serialize on this lock (the suite runs tests in parallel threads).
    static MODE_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        MODE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn req(origin: CommandOrigin) -> SandboxRequest {
        let cwd = std::env::current_dir().unwrap();
        SandboxRequest::shell(origin, "echo sandboxed", cwd.clone(), cwd)
    }

    #[test]
    fn prepared_command_has_a_scrubbed_env() {
        let _g = lock();
        crate::sandbox::set_mode(SandboxMode::Auto);
        std::env::set_var("AIZEN_SBX_RUNNER_TOKEN", "leakme");
        let sbx = prepare_std(req(CommandOrigin::ShellRun)).expect("prepare");
        std::env::remove_var("AIZEN_SBX_RUNNER_TOKEN");
        let has = sbx
            .command
            .get_envs()
            .any(|(k, _)| k.to_string_lossy() == "AIZEN_SBX_RUNNER_TOKEN");
        assert!(!has, "planted secret reached the prepared command");
        // env_clear was used: the command carries an explicit env list, PATH included.
        assert!(sbx
            .command
            .get_envs()
            .any(|(k, v)| k.to_string_lossy().eq_ignore_ascii_case("path") && v.is_some()));
    }

    /// End-to-end on the real platform shell: the child prints its environment; the secret must
    /// not appear, PATH must. This is the cross-platform adversarial check for env isolation.
    #[test]
    fn spawned_child_does_not_see_aizen_secrets() {
        let _g = lock();
        crate::sandbox::set_mode(SandboxMode::Auto);
        std::env::set_var("AIZEN_SBX_E2E_TOKEN", "leakme-e2e");
        let line = if cfg!(windows) { "set" } else { "env" };
        let cwd = std::env::current_dir().unwrap();
        let mut sbx = prepare_std(SandboxRequest::shell(
            CommandOrigin::ShellRun,
            line,
            cwd.clone(),
            cwd,
        ))
        .expect("prepare");
        let out = sbx
            .command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .expect("spawn env dump");
        std::env::remove_var("AIZEN_SBX_E2E_TOKEN");
        sbx.finish(Outcome::Exit(out.status.code()));
        let dump = String::from_utf8_lossy(&out.stdout);
        assert!(
            !dump.contains("AIZEN_SBX_E2E_TOKEN"),
            "the child saw the secret"
        );
        assert!(
            dump.to_uppercase().contains("PATH="),
            "the child lost PATH — scrub too aggressive: {dump}"
        );
    }

    #[test]
    fn private_tmp_redirects_the_child_temp() {
        let _g = lock();
        crate::sandbox::set_mode(SandboxMode::Auto);
        let cwd = std::env::current_dir().unwrap();
        let line = if cfg!(windows) {
            "echo %TEMP%"
        } else {
            "echo $TMPDIR"
        };
        let mut sbx = prepare_std(SandboxRequest::shell(
            CommandOrigin::ShellRun,
            line,
            cwd.clone(),
            cwd,
        ))
        .expect("prepare");
        let out = sbx
            .command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .output()
            .expect("spawn");
        sbx.finish(Outcome::Exit(out.status.code()));
        let printed = String::from_utf8_lossy(&out.stdout);
        assert!(
            printed.contains("aizen-sbx-"),
            "child temp was not redirected: {printed}"
        );
    }

    #[test]
    fn off_mode_keeps_the_inherited_environment() {
        let _g = lock();
        // `off` must behave exactly like the pre-sandbox spawn: no env_clear.
        crate::sandbox::set_mode(SandboxMode::Off);
        let sbx = prepare_std(req(CommandOrigin::ShellRun));
        crate::sandbox::set_mode(SandboxMode::Auto);
        let sbx = sbx.expect("prepare");
        let explicit: Vec<_> = sbx.command.get_envs().collect();
        assert!(
            explicit.is_empty(),
            "off mode must inherit the parent env untouched"
        );
    }

    #[test]
    fn strict_fails_closed_where_the_kernel_cannot_enforce() {
        let _g = lock();
        crate::sandbox::set_mode(SandboxMode::Strict);
        let r = prepare_std(req(CommandOrigin::ShellRun));
        crate::sandbox::set_mode(SandboxMode::Auto);
        #[cfg(windows)]
        {
            let e = r
                .err()
                .expect("strict on Windows must refuse (no AppContainer backend)");
            let msg = e.to_string();
            assert!(msg.contains("sandbox refused"), "got: {msg}");
            assert!(msg.contains("guarded"), "must point at the way out: {msg}");
        }
        #[cfg(not(windows))]
        {
            // On a kernel with Landlock this succeeds; without one it must refuse. Either way it
            // must not pretend: success implies the probe reported kernel backing.
            if let Ok(s) = r {
                assert!(crate::sandbox::capabilities::probe()
                    .fs_write
                    .kernel_backed());
                drop(s);
            }
        }
    }

    #[test]
    fn unattended_origins_fail_closed_without_kernel_or_optin() {
        let _g = lock();
        crate::sandbox::set_mode(SandboxMode::Auto);
        if kernel_sandbox_available() {
            return; // on a Landlock host the unattended path proceeds under the kernel
        }
        let e = prepare_std(req(CommandOrigin::Cron))
            .err()
            .expect("cron must fail closed here");
        assert!(e.to_string().contains("allow_guarded_fallback"), "{e}");
    }
}
