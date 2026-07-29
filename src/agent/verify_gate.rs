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
    /// A project-supplied command from `./.aizen/verify.json` (trust-gated — see
    /// [`detect_verify_commands`]).
    Custom(String),
}

impl VerifyCommand {
    /// The shell command line to execute.
    pub fn command_line(&self) -> String {
        match self {
            VerifyCommand::Cargo => "cargo check".to_string(),
            VerifyCommand::Npm(script) => format!("npm run {script}"),
            VerifyCommand::NpxTsc => "npx tsc --noEmit".to_string(),
            VerifyCommand::Custom(c) => c.clone(),
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
    pub stable: bool,
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

/// Walk up from `start` (a directory, or a file's directory) to the nearest ancestor that looks
/// like a project root — one carrying a manifest `detect_verify_command` recognizes (`Cargo.toml`,
/// `package.json`, `tsconfig.json`) or a `./.aizen/verify.json`. Returns that directory, or `None`
/// if no ancestor qualifies (the gate then falls back to cwd, preserving the old behavior).
///
/// This exists because the file tools can write OUTSIDE the process cwd (the confine guard was
/// removed): the write target's directory is where the edit really landed, but that directory is
/// often a `src/` subfolder, not the crate root where `Cargo.toml` lives. Detection keys on the
/// manifest sitting in the SAME directory, so the gate must first climb to the directory that holds
/// it — otherwise a real edit to `proj/src/x.rs` finds no manifest in `proj/src/` and skips
/// verification silently, the exact gap this closes.
pub fn verify_root(start: &Path) -> Option<std::path::PathBuf> {
    // If `start` names a file, begin at its directory; a directory begins at itself.
    let mut dir: &Path = if start.is_file() {
        start.parent()?
    } else {
        start
    };
    loop {
        let has_manifest = dir.join("Cargo.toml").is_file()
            || dir.join("package.json").is_file()
            || dir.join("tsconfig.json").is_file()
            || dir.join(".aizen").join("verify.json").is_file();
        if has_manifest {
            return Some(dir.to_path_buf());
        }
        dir = dir.parent()?;
    }
}

/// The COMMAND LIST for a project: `./.aizen/verify.json` (project-supplied, e.g.
/// `{"commands": ["cargo test --lib", "cargo clippy"], "timeout_secs": 180}`) when present AND the
/// project is TRUSTED — auto-running repo-supplied commands is the same supply-chain surface as
/// project mcp.json, so it sits behind the same `mcp::project_trusted()` gate, plus the cmd_guard
/// hard floor per command. Otherwise the built-in single detection. Run in order; first failure is
/// the gate result.
pub fn detect_verify_commands(cwd: &Path) -> Vec<VerifyCommand> {
    if let Some(customs) = load_custom_verify(cwd) {
        if !customs.is_empty() {
            return customs;
        }
    }
    detect_verify_command(cwd).into_iter().collect()
}

/// Parse the trusted `./.aizen/verify.json` commands (≤3 honored; Blocked commands dropped).
/// `None` ⇒ no usable custom file (missing / untrusted / unparseable).
fn load_custom_verify(cwd: &Path) -> Option<Vec<VerifyCommand>> {
    let text = std::fs::read_to_string(cwd.join(".aizen").join("verify.json")).ok()?;
    if !crate::agent::mcp::project_trusted() {
        return None; // untrusted repo → the file is inert
    }
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    Some(
        v.get("commands")?
            .as_array()?
            .iter()
            .filter_map(|c| c.as_str())
            .take(3)
            .filter(|c| {
                !matches!(
                    crate::agent::cmd_guard::classify(c),
                    crate::agent::cmd_guard::Verdict::Blocked(_)
                )
            })
            .map(|c| VerifyCommand::Custom(c.to_string()))
            .collect(),
    )
}

/// The custom file's per-command timeout (clamped [10, 600]); `None` when absent/untrusted.
fn custom_verify_timeout(cwd: &Path) -> Option<u64> {
    let text = std::fs::read_to_string(cwd.join(".aizen").join("verify.json")).ok()?;
    if !crate::agent::mcp::project_trusted() {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(&text).ok()?;
    v.get("timeout_secs")?.as_u64().map(|t| t.clamp(10, 600))
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

/// Run the project's verify command LIST in order, stopping at the first failure (its result is
/// the gate result); all-pass returns the last pass. Returns `None` when there is nothing to run
/// (unknown project) or nothing could even be spawned (best-effort no-op).
pub async fn run_verify_gate(cwd: &Path, timeout_secs: u64) -> Option<VerifyGateResult> {
    let cmds = detect_verify_commands(cwd);
    if cmds.is_empty() {
        return None;
    }
    let baseline = source_fingerprint(cwd);
    let custom_timeout = custom_verify_timeout(cwd);
    let mut last_pass: Option<VerifyGateResult> = None;
    for cmd in cmds {
        let secs = match cmd {
            VerifyCommand::Custom(_) => custom_timeout.unwrap_or(timeout_secs),
            _ => timeout_secs,
        };
        match run_one_verify(cwd, &cmd, secs).await {
            None => continue,
            Some(mut r) => {
                r.stable = baseline.is_some() && baseline == source_fingerprint(cwd);
                if !r.stable {
                    r.passed = false;
                    r.output = "verification result ignored: the source workspace changed while the command was running. No success was reported; retry after the other writer finishes".to_string();
                    return Some(r);
                }
                if !r.passed {
                    return Some(r);
                }
                last_pass = Some(r);
            }
        }
    }
    last_pass
}

/// Run ONE verify command in `cwd` with a wall-clock timeout.
async fn run_one_verify(
    cwd: &Path,
    cmd: &VerifyCommand,
    timeout_secs: u64,
) -> Option<VerifyGateResult> {
    let command_line = cmd.command_line();
    let start = Instant::now();

    let mut command = shell_command(&command_line);
    command
        .current_dir(cwd)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true); // a dropped (timed-out) future kills the child.
                             // `kill_on_drop` alone reaches only the `cmd.exe`/`sh` wrapper. The verify command is a BUILD
                             // (`cargo check`, `tsc`, `npm test`), so the surviving grandchild is a compiler holding a lock on
                             // `target/` — the next verify then blocks on that lock and times out too, and the timeouts
                             // compound instead of clearing. Containment makes the timeout actually stop the work.
    crate::core::proctree::prepare_tokio(&mut command);

    let child = command.spawn().ok()?; // spawn failure (no toolchain) → silent no-op.
    let containment = crate::core::proctree::contain_tokio(&child);
    let dur = Duration::from_secs(timeout_secs.max(1));
    // `wait_with_output` consumes the child, so the tree is killed through the containment handle
    // rather than a `&mut Child` we no longer have.
    let outcome = timeout(dur, child.wait_with_output()).await;
    if outcome.is_err() {
        crate::core::proctree::terminate_tree(&containment);
    }
    match outcome {
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
                stable: true,
            })
        }
        Ok(Err(_)) => None, // io error draining output → no-op.
        Err(_) => Some(VerifyGateResult {
            passed: false,
            command: command_line,
            output: format!("verify timed out after {timeout_secs}s (killed)"),
            duration_ms: start.elapsed().as_millis(),
            stable: true,
        }),
    }
}

fn source_fingerprint(cwd: &Path) -> Option<u64> {
    let mut files = Vec::new();
    collect_source_files(cwd, &mut files, 0);
    files.sort();
    if files.is_empty() {
        return None;
    }
    let mut data = Vec::new();
    for path in files {
        data.extend_from_slice(path.to_string_lossy().as_bytes());
        data.push(0);
        data.extend_from_slice(&std::fs::read(&path).ok()?);
        data.push(0xff);
    }
    let digest = ring::digest::digest(&ring::digest::SHA256, &data);
    Some(u64::from_le_bytes(digest.as_ref()[..8].try_into().ok()?))
}

fn collect_source_files(dir: &Path, out: &mut Vec<std::path::PathBuf>, depth: usize) {
    if depth > 32 {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name == ".git" || name == "target" || name == "node_modules" || name == ".aizen" {
            continue;
        }
        if path.is_dir() {
            collect_source_files(&path, out, depth + 1);
        } else if path.is_file() && is_source_or_manifest(&path) {
            out.push(path);
        }
    }
}

fn is_source_or_manifest(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some(
            "rs" | "toml"
                | "lock"
                | "json"
                | "ts"
                | "tsx"
                | "js"
                | "jsx"
                | "py"
                | "go"
                | "java"
                | "cs"
                | "cpp"
                | "h"
                | "hpp"
        )
    )
}

/// output is SHAPED first: deduped error blocks, capped counts — a 400-line wall of repeated
/// errors buys nothing but tokens.
pub fn format_gate_failure(r: &VerifyGateResult) -> String {
    if !r.stable {
        return format!(
            "[aizen verify] `{}` was discarded because the source workspace changed while it ran. {}",
            r.command, r.output
        );
    }
    let mut msg = format!(
        "[aizen verify] `{}` FAILED ({} ms). Fix these errors before reporting the task done:\n\n{}",
        r.command,
        r.duration_ms,
        shape_failure_output(&r.output)
    );
    if let Some(hint) = crate::features::timemachine::recovery_hint() {
        msg.push_str("\n\n");
        msg.push_str(&hint);
        msg.push_str(
            " Prefer a surgical fix when the error is local; rewind when the approach itself is wrong \
             (wrong design, cascading breakage). After a rewind, re-read files — disk contents changed.",
        );
    }
    msg
}

/// Max distinct error blocks/rows surfaced to the model (the rest are counted, not quoted).
const MAX_ERRORS: usize = 10;
/// Hard cap on the shaped text.
const MAX_SHAPED_CHARS: usize = 3_000;

/// Shape a failing verify output: cargo-style error blocks (deduped by header, ≤5 lines each) or
/// tsc-style error rows (deduped), capped at [`MAX_ERRORS`] with a suppressed-count note; an
/// unrecognized shape falls back to the raw tail (behavior-preserving floor).
pub fn shape_failure_output(raw: &str) -> String {
    let shaped = shape_cargo(raw).or_else(|| shape_tsc(raw));
    match shaped {
        Some(s) => head_chars(&s, MAX_SHAPED_CHARS),
        None => tail_chars(raw, MAX_OUTPUT_CHARS),
    }
}

/// Cargo/rustc shape: blocks starting `error[...]` / `error:`, header + up to 5 detail lines,
/// deduped by header (the same error at N call sites collapses to one block).
fn shape_cargo(raw: &str) -> Option<String> {
    let lines: Vec<&str> = raw.lines().collect();
    let is_err_start = |l: &str| l.starts_with("error[") || l.starts_with("error:");
    let mut blocks: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut warnings = 0usize;
    let mut i = 0usize;
    while i < lines.len() {
        let l = lines[i];
        if l.starts_with("warning:") {
            warnings += 1;
        }
        if is_err_start(l) {
            let mut block: Vec<&str> = vec![l];
            let mut j = i + 1;
            while j < lines.len()
                && block.len() < 6
                && !is_err_start(lines[j])
                && !lines[j].starts_with("warning:")
            {
                if !lines[j].trim().is_empty() {
                    block.push(lines[j]);
                }
                j += 1;
            }
            if seen.insert(l.to_string()) {
                blocks.push(block.join("\n"));
            }
            i = j;
        } else {
            i += 1;
        }
    }
    if blocks.is_empty() {
        return None;
    }
    let total = blocks.len();
    let shown = total.min(MAX_ERRORS);
    let mut out = blocks[..shown].join("\n");
    let mut extras: Vec<String> = Vec::new();
    if total > shown {
        extras.push(format!("+{} more error(s)", total - shown));
    }
    if warnings > 0 {
        extras.push(format!("{warnings} warning(s)"));
    }
    if !extras.is_empty() {
        out.push_str(&format!("\n({} suppressed)", extras.join(", ")));
    }
    Some(out)
}

/// tsc/npm shape: `file(line,col): error TSxxxx: message` rows, deduped by (file, code, message).
fn shape_tsc(raw: &str) -> Option<String> {
    use once_cell::sync::Lazy;
    use regex::Regex;
    static RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?m)^(.+)\((\d+),(\d+)\): error (TS\d+): (.*)$").unwrap());
    let mut seen = std::collections::HashSet::new();
    let mut rows: Vec<String> = Vec::new();
    for c in RE.captures_iter(raw) {
        if seen.insert(format!("{}|{}|{}", &c[1], &c[4], &c[5])) {
            rows.push(format!(
                "{} {}:{}  {} {}",
                &c[1], &c[2], &c[3], &c[4], &c[5]
            ));
        }
    }
    if rows.is_empty() {
        return None;
    }
    let total = rows.len();
    let shown = total.min(MAX_ERRORS);
    let mut out = rows[..shown].join("\n");
    if total > shown {
        out.push_str(&format!("\n(+{} more error(s) suppressed)", total - shown));
    }
    Some(out)
}

/// Keep the first `max` chars (shaped output leads with the errors), marking the elision.
fn head_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}\n…[{} chars truncated]…", n - max)
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
        let d = std::env::temp_dir().join(format!("aizen-verify-{tag}-{}", std::process::id()));
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
        assert_eq!(
            detect_verify_command(&d),
            Some(VerifyCommand::Npm("typecheck".into()))
        );
        assert_eq!(
            VerifyCommand::Npm("typecheck".into()).command_line(),
            "npm run typecheck"
        );
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
    fn verify_root_walks_up_from_a_subdir_to_the_manifest() {
        // The edit landed in `proj/src/` but `Cargo.toml` sits in `proj/` — the gate must climb.
        let root = temp_dir("walkup");
        std::fs::write(root.join("Cargo.toml"), "[package]\nname=\"x\"").unwrap();
        let src = root.join("src").join("nested");
        std::fs::create_dir_all(&src).unwrap();
        // Canonicalize both sides: temp_dir may sit behind a symlink (e.g. macOS /var → /private).
        assert_eq!(
            verify_root(&src).map(|p| p.canonicalize().unwrap()),
            Some(root.canonicalize().unwrap())
        );
        // A file argument begins at its parent dir and still finds the root.
        let file = src.join("x.rs");
        std::fs::write(&file, "// x").unwrap();
        assert_eq!(
            verify_root(&file).map(|p| p.canonicalize().unwrap()),
            Some(root.canonicalize().unwrap())
        );
    }

    #[test]
    fn verify_root_is_none_when_no_ancestor_has_a_manifest() {
        let d = temp_dir("walkup-none");
        let sub = d.join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        // Nothing from `sub` up to the filesystem root carries a manifest we recognize. (temp dirs
        // themselves never do, so this walks to `/` and returns None.)
        assert_eq!(verify_root(&sub), None);
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
    fn shapes_cargo_errors_deduped_and_capped() {
        // 3 error blocks, one duplicated header, plus warnings — dedup + count, keep ≤5 lines each.
        let raw = "\
warning: unused variable: `x`
error[E0308]: mismatched types
 --> src/a.rs:10:5
  = note: expected `u32`
error[E0308]: mismatched types
 --> src/a.rs:10:5
error[E0425]: cannot find value `foo`
 --> src/b.rs:2:1
warning: dead code
";
        let s = shape_failure_output(raw);
        assert_eq!(
            s.matches("error[E0308]").count(),
            1,
            "duplicate header deduped: {s}"
        );
        assert!(s.contains("error[E0425]"), "{s}");
        assert!(
            s.contains("--> src/a.rs:10:5"),
            "the location line survives: {s}"
        );
        assert!(
            s.contains("2 warning(s)") && s.contains("suppressed"),
            "{s}"
        );
    }

    #[test]
    fn shapes_tsc_error_rows() {
        let raw = "\
src/app.ts(10,5): error TS2322: Type 'string' is not assignable to type 'number'.
src/app.ts(10,5): error TS2322: Type 'string' is not assignable to type 'number'.
src/lib.ts(3,1): error TS2304: Cannot find name 'foo'.
";
        let s = shape_failure_output(raw);
        assert_eq!(s.matches("TS2322").count(), 1, "duplicate row deduped: {s}");
        assert!(s.contains("src/lib.ts 3:1"), "{s}");
    }

    #[test]
    fn unknown_shape_falls_back_to_tail() {
        let raw = format!("{}THE REAL FAILURE AT THE END", "noise\n".repeat(2000));
        let s = shape_failure_output(&raw);
        assert!(
            s.contains("THE REAL FAILURE AT THE END"),
            "tail preserved: …{}",
            &s[s.len().saturating_sub(80)..]
        );
        assert!(s.contains("truncated"));
    }

    #[test]
    fn custom_verify_requires_trust_gate() {
        // An untrusted repo's verify.json must be INERT. Sandbox AIZEN_HOME so the developer's
        // real trust store (which may trust THIS repo) can't leak into the assertion.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = temp_dir("custom-untrusted-home");
        std::env::set_var("AIZEN_HOME", &home);
        let d = temp_dir("custom-untrusted");
        std::fs::create_dir_all(d.join(".aizen")).unwrap();
        std::fs::write(
            d.join(".aizen").join("verify.json"),
            r#"{"commands":["echo pwned"]}"#,
        )
        .unwrap();
        std::fs::write(d.join("Cargo.toml"), "[package]").unwrap();
        let cmds = detect_verify_commands(&d);
        std::env::remove_var("AIZEN_HOME");
        assert_eq!(
            cmds,
            vec![VerifyCommand::Cargo],
            "untrusted verify.json ignored: {cmds:?}"
        );
    }

    #[test]
    fn format_failure_includes_command_and_output() {
        let r = VerifyGateResult {
            passed: false,
            command: "cargo check".into(),
            output: "error[E0308]: mismatched types".into(),
            duration_ms: 1234,
            stable: true,
        };
        let msg = format_gate_failure(&r);
        assert!(msg.contains("cargo check"));
        assert!(msg.contains("E0308"));
        assert!(msg.contains("Fix these errors"));
    }
}
