//! What a typed REPL line means before it becomes a turn: `#remember`, `!shell` escapes, and
//! `@file` / `` !`cmd` `` expansion.
//!
//! Shared by both REPL surfaces so a line typed into either one is preprocessed identically.

use crate::features::commands;
use crate::memory;
use crate::ui::{splash, theme, tui};
use console::style;

/// What preprocessing a typed REPL line decided.
pub(crate) enum InputPre {
    /// A `#remember` / `!shell-escape` — handled inline, run NO agent turn.
    Handled,
    /// A normal message (its `@file` / inline `` !`cmd` `` refs expanded) → send as a chat turn.
    Send(String),
}

/// Cap shell-escape output so one chatty command can't flood the transcript.
const SHELL_ESCAPE_CAP: usize = 6000;

/// Preprocess a typed REPL line for the input-box affordances: `#text` captures a memory fact and
/// `!cmd` is a shell escape (both run NO turn); a normal message has its `@file` and inline
/// `` !`cmd` `` refs expanded. Output routes through `tui::emit_line` (works under the sticky TUI and
/// the plain REPL alike). Sync — every step (remember / classify / expand / run) is synchronous.
pub(crate) fn preprocess_input(line: &str) -> InputPre {
    let t = line.trim_start();
    // `#text` → remember a fact directly (the highest-confidence capture → straight into the store).
    if let Some(rest) = t.strip_prefix('#') {
        let text = rest.trim();
        if text.is_empty() {
            tui::emit_line(
                &style("# — type the fact after the # to remember it (this project's zone; `#global: …` for everywhere)")
                    .dim()
                    .to_string(),
            );
        } else {
            match memory::remember(text) {
                Ok(id) => tui::emit_line(
                    &style(format!("🧠 remembered ({id})"))
                        .color256(splash::ACCENT)
                        .to_string(),
                ),
                Err(e) => tui::emit_line(&format!("{} {e}", style("memory:").red())),
            }
        }
        return InputPre::Handled;
    }
    // `!cmd` → shell escape. The user typed it explicitly (like a terminal), so it runs without an
    // approval prompt — but the hard safety floor still refuses catastrophic commands.
    if let Some(rest) = t.strip_prefix('!') {
        let cmd = rest.trim();
        if cmd.is_empty() {
            tui::emit_line(
                &style("! — type a shell command after the !")
                    .dim()
                    .to_string(),
            );
            return InputPre::Handled;
        }
        match crate::agent::cmd_guard::classify(cmd) {
            crate::agent::cmd_guard::Verdict::Blocked(reason) => {
                tui::emit_line(&format!(
                    "{} blocked by the safety floor: {reason}",
                    theme::warn("✗")
                ));
            }
            _ => {
                let out = run_shell_escape(cmd);
                tui::emit_line(&format!(
                    "{} {cmd}\n{out}",
                    style("$").color256(splash::ACCENT)
                ));
            }
        }
        return InputPre::Handled;
    }
    // A normal message → expand `@file` + inline `` !`cmd` `` before it's sent to the agent.
    match commands::expand_refs(line) {
        Ok(expanded) => InputPre::Send(expanded),
        Err(e) => {
            tui::emit_line(&format!("{} {e}", style("input:").red()));
            InputPre::Handled // a ref failed (e.g. a blocked `!`cmd``) → don't send a half-expanded turn
        }
    }
}

/// Run a user-typed `!cmd` shell escape in the working dir, capturing stdout+stderr (lossy-decode +
/// `chcp 65001` like `shell_run` so non-English Windows output isn't dropped), capped for display.
fn run_shell_escape(command: &str) -> String {
    run_shell_escape_in(command, None)
}

/// As `run_shell_escape`, but in an EXPLICIT directory. The hostbot daemon passes its lane's cwd:
/// several bots share one process, so `/sh` must run where that bot was told to work, not wherever
/// the process happens to be. `None` ⇒ inherit the process cwd (the REPL's `!cmd`).
///
/// Origin split for the sandbox: a REPL `!cmd` is the user at the keyboard (`UserEscape`); a
/// hostbot `/sh` arrives over a remote lane (`TelegramShell`) and is subject to the unattended
/// fail-closed rule when no kernel backend exists.
pub(crate) fn run_shell_escape_in(command: &str, dir: Option<&std::path::Path>) -> String {
    use std::time::Duration;
    /// A `!cmd` escape runs on the REPL's own thread, so an unbounded wait freezes the entire UI —
    /// not one tool call. `Command::output()` has no deadline (it waits for pipe EOF, which a
    /// grandchild outliving its wrapper never delivers), so this goes through the bounded helper.
    /// Generous, because the user typed this command deliberately and is watching it.
    const ESCAPE_TIMEOUT: Duration = Duration::from_secs(120);
    const ESCAPE_DRAIN_GRACE: Duration = Duration::from_secs(2);

    let origin = if dir.is_some() {
        crate::sandbox::CommandOrigin::TelegramShell
    } else {
        crate::sandbox::CommandOrigin::UserEscape
    };
    let cwd = dir
        .map(std::path::Path::to_path_buf)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let mut sbx = match crate::sandbox::runner::prepare_std(
        crate::sandbox::request::SandboxRequest::shell(origin, command, cwd.clone(), cwd)
            .private_tmp(false) // a user-typed command writing to %TEMP% should see the real one
            .wall_timeout(ESCAPE_TIMEOUT),
    ) {
        Ok(s) => s,
        Err(e) => return format!("[refused: {e}]"),
    };
    let bounded =
        crate::core::proctree::output_bounded(&mut sbx.command, ESCAPE_TIMEOUT, ESCAPE_DRAIN_GRACE);
    sbx.finish(match &bounded {
        Ok(o) if o.timed_out => crate::sandbox::runner::Outcome::Timeout,
        Ok(o) => crate::sandbox::runner::Outcome::Exit(o.code),
        Err(_) => crate::sandbox::runner::Outcome::SpawnFailed,
    });
    match bounded {
        Ok(o) => {
            let mut s = o.stdout;
            if !o.stderr.trim().is_empty() {
                if !s.is_empty() && !s.ends_with('\n') {
                    s.push('\n');
                }
                s.push_str(&o.stderr);
            }
            if o.output_truncated {
                s.push_str("\n…[output cut: a surviving child process still held the pipe]");
            }
            let s = s.trim_end().to_string();
            let s = if s.chars().count() > SHELL_ESCAPE_CAP {
                let head: String = s.chars().take(SHELL_ESCAPE_CAP).collect();
                format!("{head}\n…[output truncated]")
            } else {
                s
            };
            if o.timed_out {
                return format!(
                    "[timed out after {}s — killed the whole process tree]\n{s}",
                    ESCAPE_TIMEOUT.as_secs()
                )
                .trim_end()
                .to_string();
            }
            if s.is_empty() {
                format!("(exit {}, no output)", o.code.unwrap_or(-1))
            } else {
                s
            }
        }
        Err(e) => format!("[failed to run: {e}]"),
    }
}
