//! Fast verify gate (the harness's F2 lever — the extension's `qualityGate`, ported lean).
//!
//! After an editing run, before the agent reports Done, run a FAST typecheck/build (never the
//! test suite — that's slow and flaky) once. On failure, the loop injects the compiler errors
//! and grants one fix turn. This catches the "model says done but it doesn't compile" failure
//! mode for ~one subprocess of cost. Best-effort throughout: a missing toolchain / unknown
//! project shape / spawn failure all degrade to a silent no-op (never block, never panic).
//!
//! Detection priority (cross-platform via `std::path::Path`): `Cargo.toml` → `cargo check`;
//! else `package.json` with a `typecheck`/`type-check`/`tsc` script → `npm run <script>`; else
//! `tsconfig.json` → `npx tsc --noEmit`; else `None`. Commands run through the platform shell
//! (`cmd /C` / `sh -c`) so the npm/npx `.cmd` shims resolve on Windows.

use std::path::Path;
use std::time::Instant;
use tokio::process::Command;
use tokio::time::{timeout, Duration};

/// The detected verify command for a project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyCommand {
    /// Rust project: `cargo check` (typecheck-equivalent, faster than a full build).
    Cargo,
    /// Node project: `npm run <script>` (the first of typecheck / type-check / tsc that exists).
    Npm(String),
    /// TypeScript with a `tsconfig.json` but no script: `npx tsc --noEmit`.
    NpxTsc,
}

impl VerifyCommand {
    /// The shell command line to execute.
    pub fn command_line(&self) -> String {
        match self {
            VerifyCommand::Cargo => "cargo check".to_string(),
            VerifyCommand::Npm(script) => format!("npm run {script}"),
            VerifyCommand::NpxTsc => "npx tsc --noEmit".to_string(),
        }
    }
}

/// The outcome of one verify-gate run.
#[derive(Debug, Clone)]
pub struct VerifyGateResult {
    pub passed: bool,
    pub command: String,
    pub output: String,
    pub duration_ms: u128,
}

/// Keep only the LAST `MAX_OUTPUT_CHARS` of combined output — compiler error stacks live at the
/// tail (the summary line, the error count), and that's what the model needs to fix.
const MAX_OUTPUT_CHARS: usize = 4000;

/// Detect the project's fast-verify command, or `None` if the shape isn't recognized.
pub fn detect_verify_command(cwd: &Path) -> Option<VerifyCommand> {
    // Cargo first: a repo may carry both manifests; `cargo check` is the faster, more precise
    // typecheck for the Rust crate the agent most likely just edited.
    if cwd.join("Cargo.toml").is_file() {
        return Some(VerifyCommand::Cargo);
    }
    let pkg = cwd.join("package.json");
    if pkg.is_file() {
        if let Some(script) = detect_npm_typecheck_script(&pkg) {
            return Some(VerifyCommand::Npm(script));
        }
    }
    if cwd.join("tsconfig.json").is_file() {
        return Some(VerifyCommand::NpxTsc);
    }
    None
}

/// Parse `package.json` and return the first typecheck-flavored script that exists.
/// Best-effort: a missing/invalid file or absent `scripts` → `None` (no panic).
fn detect_npm_typecheck_script(pkg: &Path) -> Option<String> {
    let text = std::fs::read_to_string(pkg).ok()?;
    let json: serde_json::Value = serde_json::from_str(&text).ok()?;
    let scripts = json.get("scripts")?.as_object()?;
    ["typecheck", "type-check", "tsc"]
        .into_iter()
        .find(|c| scripts.contains_key(*c))
        .map(String::from)
}

/// Build a shell command (matches `ShellRun`'s platform handling so npm/npx `.cmd` shims resolve).
fn shell_command(command_line: &str) -> Command {
    if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command_line);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command_line);
        c
    }
}

/// Run the detected verify command in `cwd` with a wall-clock timeout. Returns `None` when there
/// is nothing to run (unknown project) or the command can't even be spawned (best-effort no-op).
pub async fn run_verify_gate(cwd: &Path, timeout_secs: u64) -> Option<VerifyGateResult> {
    let cmd = detect_verify_command(cwd)?;
    let command_line = cmd.command_line();
    let start = Instant::now();

    let mut command = shell_command(&command_line);
    command
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true); // a dropped (timed-out) future kills the child.

    let child = command.spawn().ok()?; // spawn failure (no toolchain) → silent no-op.
    let dur = Duration::from_secs(timeout_secs.max(1));
    match timeout(dur, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            let mut combined = String::from_utf8_lossy(&output.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&output.stderr);
            if !stderr.trim().is_empty() {
                if !combined.is_empty() && !combined.ends_with('\n') {
                    combined.push('\n');
                }
                combined.push_str(&stderr);
            }
            Some(VerifyGateResult {
                passed: output.status.success(),
                command: command_line,
                output: tail_chars(combined.trim_end(), MAX_OUTPUT_CHARS),
                duration_ms: start.elapsed().as_millis(),
            })
        }
        Ok(Err(_)) => None, // io error draining output → no-op.
        Err(_) => Some(VerifyGateResult {
            passed: false,
            command: command_line,
            output: format!("verify timed out after {timeout_secs}s (killed)"),
            duration_ms: start.elapsed().as_millis(),
        }),
    }
}

/// The user-message text injected when the gate fails (the model's one fix-turn prompt).
pub fn format_gate_failure(r: &VerifyGateResult) -> String {
    format!(
        "[aizen verify] `{}` FAILED ({} ms). Fix these errors before reporting the task done:\n\n{}",
        r.command, r.duration_ms, r.output
    )
}

/// Keep the last `max` chars, marking the elision (errors are at the tail).
fn tail_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let skip = n - max;
    let tail: String = s.chars().skip(skip).collect();
    format!("…[{skip} chars truncated]…\n{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ng-verify-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn detects_cargo() {
        let d = temp_dir("cargo");
        std::fs::write(d.join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        assert_eq!(detect_verify_command(&d), Some(VerifyCommand::Cargo));
        assert_eq!(VerifyCommand::Cargo.command_line(), "cargo check");
    }

    #[test]
    fn cargo_takes_precedence_over_npm() {
        let d = temp_dir("both");
        std::fs::write(d.join("Cargo.toml"), "[package]").unwrap();
        std::fs::write(d.join("package.json"), r#"{"scripts":{"typecheck":"tsc"}}"#).unwrap();
        assert_eq!(detect_verify_command(&d), Some(VerifyCommand::Cargo));
    }

    #[test]
    fn detects_npm_script_in_priority_order() {
        let d = temp_dir("npm");
        // both type-check and typecheck present → typecheck wins (higher priority).
        std::fs::write(
            d.join("package.json"),
            r#"{"scripts":{"build":"x","type-check":"tsc","typecheck":"tsc --noEmit"}}"#,
        )
        .unwrap();
        assert_eq!(detect_verify_command(&d), Some(VerifyCommand::Npm("typecheck".into())));
        assert_eq!(VerifyCommand::Npm("typecheck".into()).command_line(), "npm run typecheck");
    }

    #[test]
    fn falls_back_to_npx_tsc() {
        let d = temp_dir("npx");
        // package.json with no typecheck script + a tsconfig → npx tsc --noEmit.
        std::fs::write(d.join("package.json"), r#"{"scripts":{"build":"x"}}"#).unwrap();
        std::fs::write(d.join("tsconfig.json"), "{}").unwrap();
        assert_eq!(detect_verify_command(&d), Some(VerifyCommand::NpxTsc));
        assert_eq!(VerifyCommand::NpxTsc.command_line(), "npx tsc --noEmit");
    }

    #[test]
    fn no_recognized_project_is_none() {
        let d = temp_dir("none");
        std::fs::write(d.join("readme.txt"), "hi").unwrap();
        assert_eq!(detect_verify_command(&d), None);
    }

    #[test]
    fn invalid_package_json_degrades_to_none() {
        let d = temp_dir("badpkg");
        std::fs::write(d.join("package.json"), "{not json").unwrap();
        // no Cargo.toml, no tsconfig, unparseable package.json → None (no panic).
        assert_eq!(detect_verify_command(&d), None);
    }

    #[test]
    fn tail_chars_keeps_tail_and_marks_elision() {
        let s = "A".repeat(50) + &"B".repeat(50);
        let t = tail_chars(&s, 30);
        assert!(t.ends_with(&"B".repeat(30)));
        assert!(t.contains("truncated"));
        assert_eq!(tail_chars("short", 4000), "short");
    }

    #[test]
    fn format_failure_includes_command_and_output() {
        let r = VerifyGateResult {
            passed: false,
            command: "cargo check".into(),
            output: "error[E0308]: mismatched types".into(),
            duration_ms: 1234,
        };
        let msg = format_gate_failure(&r);
        assert!(msg.contains("cargo check"));
        assert!(msg.contains("E0308"));
        assert!(msg.contains("Fix these errors"));
    }
}
