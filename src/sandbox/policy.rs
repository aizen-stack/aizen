//! Sandbox policy: the user's `sandbox` config section, environment scrubbing, and the filesystem
//! roots each backend enforces (or, where it cannot, reports as advisory).
//!
//! Environment scrubbing is the one protection that works IDENTICALLY on every platform, so it
//! lives here rather than in a backend: a child process simply never receives Aizen's provider
//! keys, bot tokens, or anything else secret-shaped, no matter which OS enforcement is available.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The `"sandbox"` section of `~/.aizen/cli-config.json`. Every field is optional so an existing
/// config file (or a hand-written partial one) deserializes unchanged.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SandboxSettings {
    /// `auto` (default) | `strict` | `guarded` | `off`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<super::SandboxMode>,
    /// `"deny"` (default) or `"allow"`: whether children get network access WITHOUT a per-command
    /// `network: true` grant. `allow` weakens the default and is reported by `sandbox status`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network: Option<String>,
    /// Let UNATTENDED runs (cron, hosted bots) degrade to the software-guarded backend when the
    /// platform has no kernel sandbox. Default `false`: those runs fail closed instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_guarded_fallback: Option<bool>,
    /// Extra roots children may READ (beyond workspace/toolchain defaults).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub read_only_roots: Vec<String>,
    /// Extra roots children may WRITE (beyond the workspace and the private temp).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub writable_roots: Vec<String>,
    /// Environment variables to pass through DESPITE the secret scrub. Exact names, or a prefix
    /// ending in `*` (`MYAPP_*`). Case-insensitive on Windows, exact elsewhere.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pass_env: Vec<String>,
    /// Resource ceilings. Anything unset uses the built-in default for that limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limits: Option<SandboxLimits>,
    /// Permit a workspace that IS a filesystem root / home directory / system directory. Without
    /// this, `strict` refuses such a workspace (a "workspace-write" sandbox over `C:\` or `/`
    /// protects nothing) and `auto` warns.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dangerous_allow_broad_workspace: Option<bool>,
}

/// Resource ceilings for sandboxed children. Enforcement is per-platform (Job Object on Windows,
/// rlimits on Unix) and reported honestly by `sandbox status`.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SandboxLimits {
    /// Foreground wall-clock cap in seconds. `None` ⇒ the existing `shell_run` default (120s,
    /// `AIZEN_SHELL_TIMEOUT_SECS`). Background processes have no wall cap by design.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_seconds: Option<u64>,
    /// Total memory ceiling (MiB) for a child's whole process tree. `None` ⇒ unlimited (a fat-LTO
    /// link or a webpack build legitimately uses many GiB; a surprise OOM-kill of a real build
    /// erodes trust faster than the limit earns it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mb: Option<u64>,
    /// Max live processes in the child's tree (fork-bomb stop). `None` ⇒ 256.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_processes: Option<u32>,
    /// CPU-time ceiling in seconds (Unix `RLIMIT_CPU`, Windows job user-time). `None` ⇒ unlimited
    /// (foreground work is already wall-clock capped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_seconds: Option<u64>,
    /// Max open file descriptors (Unix only). `None` ⇒ leave the inherited limit.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_open_files: Option<u64>,
    /// Max size of any single file a child creates, MiB (Unix `RLIMIT_FSIZE`). `None` ⇒ unlimited.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_file_size_mb: Option<u64>,
}

/// Defaults applied where the config leaves a limit unset.
pub const DEFAULT_MAX_PROCESSES: u32 = 256;

/// Network capability default from config: `true` ⇒ children may reach the network without a
/// per-command grant. Default is deny.
pub fn network_default_allow(s: &SandboxSettings) -> bool {
    matches!(s.network.as_deref(), Some("allow"))
}

/// The user's sandbox settings (absent section ⇒ all defaults).
pub fn settings() -> SandboxSettings {
    crate::core::cli_config::load().sandbox.unwrap_or_default()
}

// ── environment scrubbing ────────────────────────────────────────────────────

/// Name fragments that mark a variable as secret-bearing. Matched case-insensitively against the
/// WHOLE name, so `AIZEN_API_KEY`, `GITHUB_TOKEN`, `NPM_TOKEN`, `AWS_SECRET_ACCESS_KEY`,
/// `TELEGRAM_BOT_TOKEN`, `DATABASE_PASSWORD`, `OAUTH_CLIENT_SECRET` and `SSH_AUTH_SOCK` all drop.
/// Known cost: benign names containing these fragments (e.g. `XAUTHORITY`) drop too — that is the
/// conservative direction, and `sandbox.pass_env` restores any specific variable a workflow needs.
const DENY_FRAGMENTS: &[&str] = &[
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "AUTH",
];

/// Prefixes dropped wholesale: every `AWS_*` (session tokens, profiles, role ARNs — the family is
/// credential-adjacent even where a single name looks harmless) and every `AIZEN_*` (Aizen's own
/// runtime knobs and secrets are not the child's business).
const DENY_PREFIXES: &[&str] = &["AWS_", "AIZEN_"];

/// Should `name` be withheld from children? Pure so it is testable without touching the real env.
pub fn env_name_denied(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    if DENY_PREFIXES.iter().any(|p| upper.starts_with(p)) {
        return true;
    }
    DENY_FRAGMENTS.iter().any(|f| upper.contains(f))
}

/// Does `name` match a `pass_env` entry (exact, or `PREFIX*`)? Case-insensitive: Windows env names
/// are case-insensitive, and being lenient here only ever RE-ALLOWS something the user listed.
pub fn pass_env_matches(name: &str, pass: &[String]) -> bool {
    let upper = name.to_ascii_uppercase();
    pass.iter().any(|p| {
        let p = p.trim().to_ascii_uppercase();
        match p.strip_suffix('*') {
            Some(prefix) => !prefix.is_empty() && upper.starts_with(prefix),
            None => upper == p,
        }
    })
}

/// The environment a sandboxed child receives: the parent's env minus everything secret-shaped,
/// plus whatever `pass_env` explicitly restores. Returned as owned pairs for `Command::env_clear`
/// + `envs`. Values are never logged by anyone — callers get the pairs, the audit log gets counts.
pub fn scrubbed_env(pass: &[String]) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    std::env::vars_os()
        .filter(|(k, _)| {
            let name = k.to_string_lossy();
            pass_env_matches(&name, pass) || !env_name_denied(&name)
        })
        .collect()
}

/// How many of the parent's variables the scrub withholds right now (for status/audit — names and
/// values stay out of every log).
pub fn scrubbed_count(pass: &[String]) -> usize {
    std::env::vars_os()
        .filter(|(k, _)| {
            let name = k.to_string_lossy();
            !pass_env_matches(&name, pass) && env_name_denied(&name)
        })
        .count()
}

// ── filesystem roots ─────────────────────────────────────────────────────────

/// The filesystem shape of one sandboxed run. Kernel backends enforce it (Linux Landlock, macOS
/// Seatbelt); platforms without one report it as advisory and `sandbox status` says so.
#[derive(Debug, Clone)]
pub struct FsPolicy {
    /// Read+write roots: the workspace, the run's private temp, and build-cache homes.
    pub read_write: Vec<PathBuf>,
    /// Read(+execute)-only roots: system and toolchain directories.
    pub read_only: Vec<PathBuf>,
    /// Directories that must stay UNREADABLE even where broad reads are otherwise allowed
    /// (macOS deny rules; doctor's adversarial checks everywhere). On Linux the allow-list
    /// already excludes them.
    pub deny: Vec<PathBuf>,
}

/// Build the default `workspace-write` policy for a run rooted at `workspace` with private temp
/// `tmp`, folding in the user's configured extra roots.
pub fn fs_policy(workspace: &Path, tmp: Option<&Path>, s: &SandboxSettings) -> FsPolicy {
    let mut rw: Vec<PathBuf> = vec![workspace.to_path_buf()];
    if let Some(t) = tmp {
        rw.push(t.to_path_buf());
    }
    let home = crate::core::config::home_dir();
    // Build-cache homes toolchains write to as a matter of course. Granting them is what keeps
    // `cargo build` / `npm ci` working inside the sandbox; they hold caches, not credentials
    // (registry TOKENS live in env/files the scrub and deny list cover).
    if let Ok(ch) = std::env::var("CARGO_HOME") {
        rw.push(PathBuf::from(ch));
    } else {
        rw.push(home.join(".cargo"));
    }
    rw.push(home.join(".npm"));
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        rw.push(PathBuf::from(xdg));
    } else {
        rw.push(home.join(".cache"));
    }
    for r in &s.writable_roots {
        rw.push(PathBuf::from(r));
    }

    let mut ro: Vec<PathBuf> = Vec::new();
    #[cfg(unix)]
    {
        for p in [
            "/usr", "/bin", "/sbin", "/lib", "/lib64", "/lib32", "/etc", "/opt", "/proc", "/sys",
            "/dev", "/run", "/var",
        ] {
            ro.push(PathBuf::from(p));
        }
        if let Ok(rh) = std::env::var("RUSTUP_HOME") {
            ro.push(PathBuf::from(rh));
        } else {
            ro.push(home.join(".rustup"));
        }
        // Git identity/config the user expects every git child to see.
        ro.push(home.join(".gitconfig"));
        ro.push(home.join(".config").join("git"));
        // Version-manager shims commonly live under home.
        ro.push(home.join(".nvm"));
        ro.push(home.join(".local"));
    }
    #[cfg(windows)]
    {
        // Advisory-only today (no kernel filesystem backend on Windows) — listed so status/doctor
        // and a future AppContainer backend agree on the intended shape.
        if let Ok(win) = std::env::var("SystemRoot") {
            ro.push(PathBuf::from(win));
        }
        for var in ["ProgramFiles", "ProgramFiles(x86)", "ProgramData"] {
            if let Ok(v) = std::env::var(var) {
                ro.push(PathBuf::from(v));
            }
        }
        ro.push(home.join(".rustup"));
        ro.push(home.join(".gitconfig"));
    }
    for r in &s.read_only_roots {
        ro.push(PathBuf::from(r));
    }

    FsPolicy {
        read_write: rw,
        read_only: ro,
        deny: credential_dirs(&home),
    }
}

/// The credential/persistence surfaces a child must never read, workspace aside. Used by macOS
/// deny rules, by the guarded backend's advisory report, and by doctor's adversarial self-tests.
pub fn credential_dirs(home: &Path) -> Vec<PathBuf> {
    let mut v = vec![
        home.join(".aizen"),
        home.join(".ssh"),
        home.join(".aws"),
        home.join(".azure"),
        home.join(".kube"),
        home.join(".docker"),
        home.join(".gnupg"),
        home.join(".git-credentials"),
        home.join(".netrc"),
        home.join(".npmrc"),
        home.join(".config").join("gcloud"),
        home.join(".config").join("gh"),
        home.join(".mozilla"),
        home.join(".config").join("google-chrome"),
        home.join(".config").join("chromium"),
    ];
    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            v.push(PathBuf::from(&appdata).join("gh"));
        }
        if let Ok(lad) = std::env::var("LOCALAPPDATA") {
            v.push(PathBuf::from(&lad).join("Google").join("Chrome"));
            v.push(PathBuf::from(&lad).join("Microsoft").join("Edge"));
        }
    }
    #[cfg(target_os = "macos")]
    {
        v.push(home.join("Library").join("Keychains"));
        v.push(
            home.join("Library")
                .join("Application Support")
                .join("Google"),
        );
    }
    v
}

/// Is `workspace` so broad that a "workspace-write" sandbox around it protects nothing?
/// Filesystem roots, the home directory, and system directories qualify.
pub fn workspace_too_broad(workspace: &Path) -> bool {
    let canon = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    // A root has no parent (`C:\`, `/`).
    if canon.parent().is_none() {
        return true;
    }
    let home = crate::core::config::home_dir();
    if let (Ok(a), Ok(b)) = (canon.canonicalize(), home.canonicalize()) {
        if a == b {
            return true;
        }
    }
    if canon == home {
        return true;
    }
    #[cfg(windows)]
    {
        for var in ["SystemRoot", "ProgramFiles"] {
            if let Ok(v) = std::env::var(var) {
                let sys = PathBuf::from(v);
                if canon == sys
                    || sys.starts_with(&canon) && canon.parent().is_some() && sys != canon
                {
                    // The workspace CONTAINS a system root (e.g. `C:\` handled above; `C:\Windows`
                    // itself or an ancestor of it).
                    if canon == sys || sys.starts_with(&canon) {
                        return true;
                    }
                }
            }
        }
    }
    #[cfg(unix)]
    {
        // Compare against the CANONICAL form of each system dir. macOS (and some
        // BSDs) resolve `/etc` → `/private/etc`, `/var` → `/private/var`, so a bare
        // `Path::new("/etc")` never equals a canonicalized workspace and the broad
        // check silently misses the paths the test (and real mounts) hand us.
        for sys in ["/usr", "/etc", "/bin", "/var", "/home"] {
            let sys_path = Path::new(sys);
            if canon == sys_path {
                return true;
            }
            if let Ok(sys_canon) = sys_path.canonicalize() {
                if canon == sys_canon {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_shaped_names_are_denied() {
        for name in [
            "AIZEN_API_KEY",
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "GITHUB_TOKEN",
            "GH_TOKEN",
            "NPM_TOKEN",
            "CARGO_REGISTRY_TOKEN",
            "TELEGRAM_BOT_TOKEN",
            "DISCORD_TOKEN",
            "AWS_ACCESS_KEY_ID",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "AWS_PROFILE", // AWS_ prefix: the family drops wholesale
            "SSH_AUTH_SOCK",
            "DATABASE_PASSWORD",
            "PGPASSWORD",
            "OAUTH_CLIENT_SECRET",
            "GOOGLE_APPLICATION_CREDENTIALS",
            "AIZEN_SHELL_TIMEOUT_SECS", // AIZEN_* is internal, not the child's business
            "basic_auth_user",          // case-insensitive
        ] {
            assert!(env_name_denied(name), "{name} must be scrubbed");
        }
    }

    #[test]
    fn toolchain_names_pass() {
        for name in [
            "PATH",
            "PATHEXT",
            "SystemRoot",
            "ComSpec",
            "windir",
            "TEMP",
            "TMP",
            "TMPDIR",
            "HOME",
            "USERPROFILE",
            "LANG",
            "LC_ALL",
            "TERM",
            "SHELL",
            "CARGO_HOME",
            "RUSTUP_HOME",
            "GOPATH",
            "JAVA_HOME",
            "VIRTUAL_ENV",
            "NODE_ENV",
            "CI",
            "PROCESSOR_ARCHITECTURE",
            "NUMBER_OF_PROCESSORS",
        ] {
            assert!(!env_name_denied(name), "{name} must pass to children");
        }
    }

    #[test]
    fn pass_env_restores_exact_and_prefix() {
        let pass = vec!["GITHUB_TOKEN".to_string(), "MYAPP_*".to_string()];
        assert!(pass_env_matches("GITHUB_TOKEN", &pass));
        assert!(pass_env_matches("github_token", &pass)); // case-insensitive
        assert!(pass_env_matches("MYAPP_SECRET", &pass));
        assert!(!pass_env_matches("GH_TOKEN", &pass));
        assert!(!pass_env_matches("MYAP", &pass));
        // A bare `*` entry must not become allow-everything.
        assert!(!pass_env_matches("ANY", &["*".to_string()]));
    }

    #[test]
    fn scrubbed_env_drops_a_planted_secret_but_keeps_path() {
        // Use a name no other test reads, set/removed around the assertion window. Env mutation is
        // process-global; keep the window tight.
        std::env::set_var("AIZEN_SBX_TEST_TOKEN", "plaintext-secret");
        let env = scrubbed_env(&[]);
        std::env::remove_var("AIZEN_SBX_TEST_TOKEN");
        assert!(
            !env.iter()
                .any(|(k, _)| k.to_string_lossy() == "AIZEN_SBX_TEST_TOKEN"),
            "planted secret leaked through the scrub"
        );
        assert!(
            env.iter().any(|(k, _)| {
                let k = k.to_string_lossy().to_ascii_uppercase();
                k == "PATH"
            }),
            "PATH must survive the scrub"
        );
    }

    #[test]
    fn broad_workspaces_are_flagged() {
        #[cfg(windows)]
        {
            assert!(workspace_too_broad(Path::new("C:\\")));
        }
        #[cfg(unix)]
        {
            assert!(workspace_too_broad(Path::new("/")));
            assert!(workspace_too_broad(Path::new("/etc")));
        }
        assert!(workspace_too_broad(&crate::core::config::home_dir()));
        // The current directory (a real project checkout) is not broad.
        let cwd = std::env::current_dir().unwrap();
        if cwd != crate::core::config::home_dir() {
            assert!(!workspace_too_broad(&cwd));
        }
    }

    #[test]
    fn credential_dirs_cover_the_spec_list() {
        let home = Path::new("/home/u");
        let dirs = credential_dirs(home);
        for want in [".aizen", ".ssh", ".aws", ".docker", ".kube"] {
            assert!(
                dirs.iter().any(|d| d.ends_with(want)),
                "{want} missing from the deny list"
            );
        }
    }

    #[test]
    fn settings_section_deserializes_from_the_spec_example() {
        let json = r#"{
            "mode": "auto",
            "network": "deny",
            "allow_guarded_fallback": false,
            "read_only_roots": [],
            "writable_roots": [],
            "pass_env": [],
            "limits": { "wall_seconds": 120 }
        }"#;
        let s: SandboxSettings = serde_json::from_str(json).unwrap();
        assert_eq!(s.mode, Some(super::super::SandboxMode::Auto));
        assert_eq!(s.limits.unwrap().wall_seconds, Some(120));
        // And an empty object works (every field defaulted).
        let empty: SandboxSettings = serde_json::from_str("{}").unwrap();
        assert!(empty.mode.is_none());
    }
}
