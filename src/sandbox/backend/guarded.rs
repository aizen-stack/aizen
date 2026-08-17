//! The software-guarded floor — the protections that exist on EVERY platform, kernel backend or
//! not: environment scrubbing, the private per-run temp directory, and pre-spawn policy checks.
//! Everything here is honest about being software: a child that ignores policy is only stopped
//! where a kernel backend adds real enforcement on top.

use crate::sandbox::policy;
use std::path::PathBuf;

/// A private per-run temp directory. `TMP`/`TEMP`/`TMPDIR` point here so a well-behaved toolchain
/// writes its scratch files into a directory that (a) is scoped to this run and (b) is deleted
/// when the guard drops — the "test leaves junk behind" problem, closed at the spawn boundary.
#[derive(Debug)]
pub struct PrivateTmp {
    path: PathBuf,
}

impl PrivateTmp {
    /// Create a fresh directory under the system temp. Best-effort: a filesystem that refuses
    /// yields `None` and the child simply keeps the shared temp.
    pub fn create(scope: &str) -> Option<Self> {
        // Scope slugs may carry separators (`repl:main/task/7`) — flatten to a filename-safe tag.
        let tag: String = scope
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .take(40)
            .collect();
        let path = std::env::temp_dir().join(format!(
            "aizen-sbx-{}-{}-{}",
            std::process::id(),
            tag,
            crate::core::persist::unique_sequence()
        ));
        std::fs::create_dir_all(&path).ok()?;
        Some(Self { path })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for PrivateTmp {
    fn drop(&mut self) {
        // Best-effort: a file a live grandchild still holds open on Windows will make this fail;
        // the startup sweep in `sandbox doctor` collects strays.
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Delete abandoned `aizen-sbx-*` directories older than a day (crash leftovers). Called by
/// `sandbox doctor`; returns how many were removed.
pub fn sweep_stale_tmp() -> usize {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return 0;
    };
    let mut removed = 0;
    for e in entries.flatten() {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("aizen-sbx-") {
            continue;
        }
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .map(|age| age.as_secs() > 24 * 3600)
            .unwrap_or(false);
        if stale && std::fs::remove_dir_all(e.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

/// Apply the guarded floor to a command environment: wipe the inherited env, insert the scrubbed
/// set, then the private temp override, then any explicit extras (an MCP server's own configured
/// variables — applied last so they win).
pub fn apply_env<C: EnvTarget>(
    cmd: &mut C,
    pass_env: &[String],
    tmp: Option<&PrivateTmp>,
    extra: &[(String, String)],
) {
    cmd.env_clear_();
    for (k, v) in policy::scrubbed_env(pass_env) {
        cmd.env_(&k, &v);
    }
    if let Some(t) = tmp {
        let p = t.path().as_os_str();
        for var in ["TMP", "TEMP", "TMPDIR"] {
            cmd.env_(std::ffi::OsStr::new(var), p);
        }
    }
    for (k, v) in extra {
        cmd.env_(std::ffi::OsStr::new(k), std::ffi::OsStr::new(v));
    }
}

/// The two `Command` flavors, unified for env application only (the smallest surface the runner
/// needs — everything else stays flavor-specific).
pub trait EnvTarget {
    fn env_clear_(&mut self);
    fn env_(&mut self, k: &std::ffi::OsStr, v: &std::ffi::OsStr);
}

impl EnvTarget for std::process::Command {
    fn env_clear_(&mut self) {
        self.env_clear();
    }
    fn env_(&mut self, k: &std::ffi::OsStr, v: &std::ffi::OsStr) {
        self.env(k, v);
    }
}

impl EnvTarget for tokio::process::Command {
    fn env_clear_(&mut self) {
        self.env_clear();
    }
    fn env_(&mut self, k: &std::ffi::OsStr, v: &std::ffi::OsStr) {
        self.env(k, v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_tmp_creates_and_removes_its_directory() {
        let tmp = PrivateTmp::create("repl:main/task/7").expect("temp fs available");
        let path = tmp.path().to_path_buf();
        assert!(path.is_dir());
        assert!(
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("aizen-sbx-"),
            "recognizable prefix so the doctor sweep can find strays"
        );
        drop(tmp);
        assert!(!path.exists(), "guard drop must remove the directory");
    }

    #[test]
    fn apply_env_scrubs_and_overrides_temp() {
        std::env::set_var("AIZEN_SBX_GUARDED_TOKEN", "s3cret");
        let tmp = PrivateTmp::create("t").unwrap();
        let mut cmd = std::process::Command::new("x");
        apply_env(
            &mut cmd,
            &[],
            Some(&tmp),
            &[("EXTRA_ONE".into(), "1".into())],
        );
        std::env::remove_var("AIZEN_SBX_GUARDED_TOKEN");
        let envs: Vec<(String, String)> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                Some((
                    k.to_string_lossy().into_owned(),
                    v?.to_string_lossy().into_owned(),
                ))
            })
            .collect();
        assert!(
            !envs.iter().any(|(k, _)| k == "AIZEN_SBX_GUARDED_TOKEN"),
            "secret must not reach the child spec"
        );
        assert!(envs.iter().any(|(k, _)| k.eq_ignore_ascii_case("path")));
        let tmp_var = envs
            .iter()
            .find(|(k, _)| k == "TMP")
            .map(|(_, v)| v.clone());
        assert_eq!(tmp_var.as_deref(), Some(&*tmp.path().to_string_lossy()));
        assert!(envs.iter().any(|(k, v)| k == "EXTRA_ONE" && v == "1"));
    }
}
