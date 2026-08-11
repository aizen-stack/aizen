//! Central git executable resolution — ONE process-wide answer to "which git do we run?".
//!
//! Before this module every subsystem spawned `Command::new("git")` and inherited PATH luck:
//! a shell without git on PATH silently re-keyed the project slug (memory-zone fork), made
//! `project_root()` degrade to cwd, and hard-blocked protected edits (the time machine treated
//! spawn-ENOENT as a real git failure). Resolution order: `AIZEN_GIT` override → PATH
//! (PATHEXT-aware) → well-known install locations. Cached per (`AIZEN_GIT`, `PATH`) so tests
//! that repoint either are never served a stale answer while production pays the probe once.

use std::path::{Path, PathBuf};

/// Typed marker for "no git executable anywhere" — carried in the anyhow chain so callers can
/// tell *git is absent* apart from *git rejected the operation* without substring matching.
#[derive(Debug, Clone, Copy)]
pub struct GitMissing;

impl std::fmt::Display for GitMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "git executable not found (checked AIZEN_GIT, PATH, and known install locations)"
        )
    }
}
impl std::error::Error for GitMissing {}

/// True when `e`'s chain contains [`GitMissing`] — robust against any `.context()` wrapping.
pub fn is_git_missing(e: &anyhow::Error) -> bool {
    e.chain().any(|c| c.downcast_ref::<GitMissing>().is_some())
}

/// How the executable was found. Drives [`resolution_note`] — anything other than plain
/// "on PATH" is worth one visible line, because it changes behavior the user can observe.
#[derive(Debug, Clone, PartialEq)]
enum Resolution {
    /// `AIZEN_GIT` pointed at an existing file — used verbatim, PATH never consulted.
    Override(PathBuf),
    /// Found by walking PATH (the boring, expected case).
    OnPath(PathBuf),
    /// Not on PATH but present at a well-known install location.
    Fallback(PathBuf),
    /// Nowhere. Also the result of an `AIZEN_GIT` pointing at a nonexistent file — an explicit
    /// override must never silently fall through to a different git than the one asked for.
    Missing,
}

impl Resolution {
    fn path(&self) -> Option<&Path> {
        match self {
            Resolution::Override(p) | Resolution::OnPath(p) | Resolution::Fallback(p) => Some(p),
            Resolution::Missing => None,
        }
    }
}

/// The resolved git executable, or `None` when git is genuinely absent.
pub fn git_exe() -> Option<PathBuf> {
    resolved().path().map(Path::to_path_buf)
}

/// A ready `Command` for the resolved git, or `Err(GitMissing)` — best-effort call sites keep
/// their old "git broken → skip" behavior with a plain `.ok()?`, while strict sites propagate a
/// typed, self-explanatory error instead of a raw spawn ENOENT.
///
/// On Windows the command carries `CREATE_NO_WINDOW`: some callers (`workspace_txn`, `repo_map`,
/// `config::project_root`) run git via a bare `.output()` that never touches `proctree::prepare`,
/// so the flag has to live here too — otherwise those spawns keep allocating a console and can trip
/// the `0xc0000142` loader-init failure the flag exists to avoid.
pub fn command() -> anyhow::Result<std::process::Command> {
    match git_exe() {
        Some(p) => Ok(no_window(std::process::Command::new(p))),
        None => Err(anyhow::Error::new(GitMissing)),
    }
}

/// Apply `CREATE_NO_WINDOW` on Windows; a pass-through elsewhere. Centralizes the flag so every
/// git spawn in this module (including the `--version` probe) gets it.
fn no_window(mut cmd: std::process::Command) -> std::process::Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(crate::core::proctree::CREATE_NO_WINDOW);
    }
    cmd
}

/// One-line status for the identity surfaces (startup banner, `/where`). `None` when git was
/// found on PATH — the expected case earns no noise; every other outcome changes observable
/// behavior and is stated once, plainly.
pub fn resolution_note() -> Option<String> {
    match resolved() {
        Resolution::OnPath(_) => None,
        Resolution::Override(p) => Some(format!("git: {} (AIZEN_GIT override)", p.display())),
        Resolution::Fallback(p) => Some(format!("git: not on PATH — using {}", p.display())),
        Resolution::Missing => Some(
            "git: not found (AIZEN_GIT/PATH/known locations) — project identity uses the nearest \
             .git marker or folder path; time-machine checkpoints are off"
                .to_string(),
        ),
    }
}

/// Cached resolve. Keyed by the raw `AIZEN_GIT` + `PATH` values so an env change (tests, a
/// mid-session `PATH` fix) invalidates naturally; a hit is one env read + string compare.
fn resolved() -> Resolution {
    static CACHE: std::sync::Mutex<Option<(String, Resolution)>> = std::sync::Mutex::new(None);
    let over = std::env::var("AIZEN_GIT").unwrap_or_default();
    let path_var = std::env::var("PATH").unwrap_or_default();
    let key = format!("{over}|{path_var}");
    if let Ok(guard) = CACHE.lock() {
        if let Some((k, r)) = guard.as_ref() {
            if *k == key {
                // Revalidate a cached hit: if the resolved file vanished (git uninstalled
                // mid-session), fall through to a fresh probe instead of serving a path whose
                // spawn-ENOENT would read as a REAL git failure and hard-block edits.
                match r.path() {
                    Some(p) if !p.is_file() => {}
                    _ => return r.clone(),
                }
            }
        }
    }
    let r = resolve_from(over.trim(), &path_var);
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((key, r.clone()));
    }
    r
}

/// Pure resolution core (injectable for tests — no test may mutate the real env vars, they are
/// process-global and the suite runs in parallel).
fn resolve_from(override_var: &str, path_var: &str) -> Resolution {
    if !override_var.is_empty() {
        // Tolerate a value pasted with its quotes, and absolutize a relative override NOW —
        // callers spawn with per-call `current_dir`, where a relative path would resolve
        // somewhere else entirely (possibly to a repo-planted binary).
        let trimmed = override_var.trim_matches(|c| c == '"' || c == '\'');
        let mut p = PathBuf::from(trimmed);
        if p.is_relative() {
            p = std::fs::canonicalize(&p).unwrap_or(p);
        }
        if p.is_file() {
            return Resolution::Override(p);
        }
        return Resolution::Missing;
    }
    if let Some(p) = which_git(path_var) {
        return Resolution::OnPath(p);
    }
    for cand in well_known_candidates() {
        // Existence is not enough for the fallbacks: macOS ships a `/usr/bin/git` xcrun SHIM
        // that exists but errors without Command Line Tools — accepting it would hard-block
        // every protected edit with a "real" git failure. One `--version` probe (cached with
        // the resolution) proves the candidate actually runs.
        if is_executable(&cand) && probe_works(&cand) {
            return Resolution::Fallback(cand);
        }
    }
    Resolution::Missing
}

/// One real `git --version` run — used only for well-known fallback candidates (a PATH hit and
/// an explicit override are taken at face value, matching the old behavior).
fn probe_works(p: &Path) -> bool {
    no_window(std::process::Command::new(p))
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// `is_file` plus the execute bit on Unix — a mode-644 `git` on PATH is not runnable and must
/// not shadow a runnable one further down the walk.
fn is_executable(p: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p)
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }
    #[cfg(not(unix))]
    {
        p.is_file()
    }
}

/// Hand-rolled `which git` over an explicit PATH value, honoring Windows `PATHEXT` (git ships as
/// `git.exe`; a bare-name probe would miss it). Same posture as `lsp::discovery::which_on_path`
/// — hand-rolled because the `which` crate's dependency tree breaks the slim windows-gnu link.
fn which_git(path_var: &str) -> Option<PathBuf> {
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    };
    for dir in std::env::split_paths(path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        if exts.is_empty() {
            let p = dir.join("git");
            if is_executable(&p) {
                return Some(p);
            }
        }
        for ext in &exts {
            let p = dir.join(format!("git{ext}"));
            if is_executable(&p) {
                return Some(p);
            }
        }
    }
    None
}

/// Standard install locations tried when PATH has no git. Ordered most-common-first; only
/// existence is checked, never executed speculatively.
fn well_known_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if cfg!(windows) {
        for pf in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Ok(base) = std::env::var(pf) {
                v.push(PathBuf::from(&base).join("Git").join("cmd").join("git.exe"));
                v.push(PathBuf::from(&base).join("Git").join("bin").join("git.exe"));
            }
        }
        if let Ok(lad) = std::env::var("LOCALAPPDATA") {
            v.push(
                PathBuf::from(&lad)
                    .join("Programs")
                    .join("Git")
                    .join("cmd")
                    .join("git.exe"),
            );
        }
        if let Ok(up) = std::env::var("USERPROFILE") {
            v.push(
                PathBuf::from(&up)
                    .join("scoop")
                    .join("shims")
                    .join("git.exe"),
            );
        }
    } else {
        for p in [
            "/usr/bin/git",
            "/usr/local/bin/git",
            "/opt/homebrew/bin/git",
        ] {
            v.push(PathBuf::from(p));
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Manual temp dir (no tempfile dep) — removed by the guard on drop.
    struct TempDir(PathBuf);
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fake_git_dir(name: &str) -> (TempDir, PathBuf) {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "aizen-gitx-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let exe = dir.join(name);
        std::fs::write(&exe, b"#!/bin/sh\n").expect("write fake git");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))
                .expect("make fake git executable");
        }
        (TempDir(dir), exe)
    }

    #[test]
    fn override_pointing_at_real_file_wins() {
        let (_d, exe) = fake_git_dir(if cfg!(windows) { "git.exe" } else { "git" });
        let r = resolve_from(exe.to_str().unwrap(), "");
        assert_eq!(r, Resolution::Override(exe));
    }

    #[test]
    fn override_beats_a_valid_path_git_and_tolerates_quotes() {
        // Two REAL files: one reachable via PATH, one via AIZEN_GIT. The explicit override must
        // win outright — PATH is never consulted — and a value pasted with its surrounding
        // quotes (the natural Windows copy-paste shape) must still resolve.
        let (_d1, on_path) = fake_git_dir(if cfg!(windows) { "git.exe" } else { "git" });
        let (_d2, override_exe) = fake_git_dir(if cfg!(windows) { "git.exe" } else { "git" });
        let path_var = std::env::join_paths([on_path.parent().unwrap()])
            .unwrap()
            .into_string()
            .unwrap();
        let r = resolve_from(override_exe.to_str().unwrap(), &path_var);
        assert_eq!(r, Resolution::Override(override_exe.clone()));
        let quoted = format!("\"{}\"", override_exe.display());
        assert_eq!(
            resolve_from(&quoted, &path_var),
            Resolution::Override(override_exe)
        );
    }

    #[test]
    fn override_pointing_at_nothing_means_missing_not_fallthrough() {
        // An explicit AIZEN_GIT must never silently run some OTHER git — and this is also how
        // tests/users simulate a gitless machine deterministically.
        let (_d, exe) = fake_git_dir(if cfg!(windows) { "git.exe" } else { "git" });
        let bogus = exe.with_file_name("no-such-git.exe");
        let path_var = std::env::join_paths([exe.parent().unwrap()])
            .unwrap()
            .into_string()
            .unwrap();
        let r = resolve_from(bogus.to_str().unwrap(), &path_var);
        assert_eq!(r, Resolution::Missing);
    }

    #[test]
    fn path_walk_finds_git_with_platform_extension() {
        let (_d, exe) = fake_git_dir(if cfg!(windows) { "git.exe" } else { "git" });
        let path_var = std::env::join_paths([exe.parent().unwrap()])
            .unwrap()
            .into_string()
            .unwrap();
        let r = resolve_from("", &path_var);
        // Case-insensitive compare: PATHEXT may spell the extension `.EXE`, and the Windows FS
        // resolves it against `git.exe` anyway — same file, different case.
        match r {
            Resolution::OnPath(p) => assert_eq!(
                p.to_string_lossy().to_lowercase(),
                exe.to_string_lossy().to_lowercase()
            ),
            other => panic!("expected OnPath, got {other:?}"),
        }
    }

    #[test]
    fn empty_path_without_wellknown_hit_is_missing_or_fallback() {
        // With an empty PATH the only possible outcomes are a well-known install or Missing —
        // never a panic, never a bogus path.
        let r = resolve_from("", "");
        match r {
            Resolution::Fallback(p) => assert!(p.is_file(), "fallback must exist: {}", p.display()),
            Resolution::Missing => {}
            other => panic!("empty PATH cannot resolve OnPath/Override, got {other:?}"),
        }
    }

    #[test]
    fn git_missing_survives_context_wrapping() {
        use anyhow::Context as _;
        let e: anyhow::Error = anyhow::Error::new(GitMissing);
        let wrapped = Err::<(), _>(e)
            .context("running git (is git installed?)")
            .context("time machine could not use git in this directory")
            .unwrap_err();
        assert!(is_git_missing(&wrapped));
        let plain = anyhow::anyhow!("fatal: not a git repository");
        assert!(!is_git_missing(&plain));
    }
}
