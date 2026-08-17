//! [`SandboxRequest`] — everything the runner must know about one spawn, gathered at the call
//! site. The request carries WHAT should run and WHO asked; the runner decides HOW (mode, backend,
//! environment, roots) and refuses when the platform cannot honor a strict policy.

use super::CommandOrigin;
use std::path::PathBuf;
use std::time::Duration;

/// The program half of a request: a shell command line (wrapped in the platform shell exactly the
/// way `shell_run` always has), or a direct program + argv (LSP/MCP servers, git plumbing).
#[derive(Debug, Clone)]
pub enum CommandSpec {
    /// Run through the platform shell: `cmd /C chcp 65001>nul & <line>` on Windows, `sh -c <line>`
    /// elsewhere. The `chcp` prelude is behavior-preserving — legacy builtins emit UTF-8 instead of
    /// the OEM codepage (see `shell_run`).
    Shell { line: String },
    /// Spawn `program` directly with `args` (no shell). `use_cmd_shim` routes through `cmd /C` on
    /// Windows for `.cmd`/`.bat` runners that `CreateProcessW` cannot execute (npx/uvx shims).
    Exec {
        program: PathBuf,
        args: Vec<String>,
        use_cmd_shim: bool,
    },
}

/// One spawn's worth of intent. Built by call sites; consumed by [`super::runner`].
#[derive(Debug, Clone)]
pub struct SandboxRequest {
    /// Who asked (a call-site constant, never model data).
    pub origin: CommandOrigin,
    /// What to run.
    pub spec: CommandSpec,
    /// Working directory for the child.
    pub cwd: PathBuf,
    /// The workspace this run is confined to (the write root for filesystem policy).
    pub workspace_root: PathBuf,
    /// Whether the network capability was REQUESTED (and, for model-originated commands, already
    /// approved upstream). `false` ⇒ the policy default (deny unless config says otherwise).
    pub network: bool,
    /// Execution/resource scope (the `exec_ctx` resource scope) — audit correlation.
    pub scope: String,
    /// Background (no wall-clock cap; lives past the tool call) vs foreground.
    pub background: bool,
    /// The wall-clock cap the CALL SITE will enforce, recorded for audit/status. The runner does
    /// not run the wait loop — the battle-tested per-site loops (cancel-aware polling, condvars,
    /// async timeouts) stay where they are.
    pub wall_timeout: Option<Duration>,
    /// Extra variables applied AFTER the scrub (an MCP server's configured env — its own tokens
    /// are its business; Aizen's are not inherited).
    pub extra_env: Vec<(String, String)>,
    /// Give the child a PRIVATE temp directory (TMP/TEMP/TMPDIR pointed at a fresh per-run dir,
    /// removed when the guard drops). Off for user escapes, where surprising temp relocation
    /// would be visible.
    pub private_tmp: bool,
}

impl SandboxRequest {
    /// A shell-line request with the common defaults: foreground, no network, private temp.
    pub fn shell(
        origin: CommandOrigin,
        line: impl Into<String>,
        cwd: PathBuf,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            origin,
            spec: CommandSpec::Shell { line: line.into() },
            cwd,
            workspace_root,
            network: false,
            scope: current_scope(),
            background: false,
            wall_timeout: None,
            extra_env: Vec::new(),
            private_tmp: true,
        }
    }

    /// A direct-exec request (server processes, plumbing).
    pub fn exec(
        origin: CommandOrigin,
        program: PathBuf,
        args: Vec<String>,
        cwd: PathBuf,
        workspace_root: PathBuf,
    ) -> Self {
        Self {
            origin,
            spec: CommandSpec::Exec {
                program,
                args,
                use_cmd_shim: false,
            },
            cwd,
            workspace_root,
            network: false,
            scope: current_scope(),
            background: true,
            wall_timeout: None,
            extra_env: Vec::new(),
            private_tmp: true,
        }
    }

    pub fn network(mut self, allowed: bool) -> Self {
        self.network = allowed;
        self
    }
    pub fn background(mut self, bg: bool) -> Self {
        self.background = bg;
        self
    }
    pub fn wall_timeout(mut self, t: Duration) -> Self {
        self.wall_timeout = Some(t);
        self
    }
    pub fn extra_env(mut self, env: Vec<(String, String)>) -> Self {
        self.extra_env = env;
        self
    }
    pub fn private_tmp(mut self, on: bool) -> Self {
        self.private_tmp = on;
        self
    }
    pub fn cmd_shim(mut self, on: bool) -> Self {
        if let CommandSpec::Exec {
            ref mut use_cmd_shim,
            ..
        } = self.spec
        {
            *use_cmd_shim = on;
        }
        self
    }

    /// The command as one displayable line (for audit redaction/truncation).
    pub fn display_line(&self) -> String {
        match &self.spec {
            CommandSpec::Shell { line } => line.clone(),
            CommandSpec::Exec { program, args, .. } => {
                let mut s = program.display().to_string();
                for a in args {
                    s.push(' ');
                    s.push_str(a);
                }
                s
            }
        }
    }
}

/// The calling turn's resource scope (audit correlation), read from the execution context the same
/// way stateful tools do. `default` outside any pinned turn.
fn current_scope() -> String {
    crate::core::exec_ctx::current()
        .map(|c| c.resource_scope())
        .unwrap_or_else(|| "default".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builders_set_what_they_say() {
        let r = SandboxRequest::shell(
            CommandOrigin::ShellRun,
            "echo hi",
            PathBuf::from("."),
            PathBuf::from("."),
        )
        .network(true)
        .background(true)
        .wall_timeout(Duration::from_secs(5));
        assert!(r.network);
        assert!(r.background);
        assert_eq!(r.wall_timeout, Some(Duration::from_secs(5)));
        assert_eq!(r.display_line(), "echo hi");

        let e = SandboxRequest::exec(
            CommandOrigin::Lsp,
            PathBuf::from("rust-analyzer"),
            vec!["--version".into()],
            PathBuf::from("."),
            PathBuf::from("."),
        )
        .cmd_shim(true);
        match e.spec {
            CommandSpec::Exec { use_cmd_shim, .. } => assert!(use_cmd_shim),
            _ => panic!("exec spec expected"),
        }
        assert_eq!(e.display_line(), "rust-analyzer --version");
    }
}
