//! Canonical workspace identity and ordered cross-process resource leases.
//!
//! The identity distinguishes linked Git worktrees that share one repository store. Stable hashed
//! keys keep lock paths short, cross-platform, and free of source paths or credentials.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::core::repo_lock::{LockMode, RepoTxnLock};

const LOCK_PROTOCOL: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceIdentity {
    pub repo_id: String,
    pub worktree_id: String,
    pub canonical_root: PathBuf,
    pub canonical_git_dir: Option<PathBuf>,
    pub canonical_common_git_dir: Option<PathBuf>,
}

impl WorkspaceIdentity {
    /// [`discover`] with a process-wide per-root cache. Discovery costs three git spawns and the
    /// writer lease runs it on EVERY destructive tool call — within one process the answer never
    /// changes for a given root, and lock correctness wants all callers to AGREE on the key more
    /// than it wants mid-process freshness (a `git init` mid-run re-keys on the next process).
    pub fn discover_cached(root: &Path) -> Result<Self> {
        static CACHE: std::sync::Mutex<
            Option<std::collections::HashMap<String, WorkspaceIdentity>>,
        > = std::sync::Mutex::new(None);
        // The nearest `.git` marker is part of the key: a `git init` mid-process must re-key —
        // an aizen started before it and one after must still agree on the lock path, or the
        // writer lease stops excluding across processes. One bounded stat-walk, no git spawn.
        let marker = {
            let mut cur = Some(root);
            let mut found = String::new();
            while let Some(p) = cur {
                if p.join(".git").exists() {
                    found = normalized_path(p);
                    break;
                }
                cur = p.parent();
            }
            found
        };
        let key = format!("{}|{marker}", normalized_path(root));
        if let Ok(guard) = CACHE.lock() {
            if let Some(id) = guard.as_ref().and_then(|m| m.get(&key)) {
                return Ok(id.clone());
            }
        }
        let id = Self::discover(root)?;
        if let Ok(mut guard) = CACHE.lock() {
            guard
                .get_or_insert_with(Default::default)
                .insert(key, id.clone());
        }
        Ok(id)
    }

    pub fn discover(root: &Path) -> Result<Self> {
        let canonical_root = canonical_existing_or_parent(root)?;
        let top = git_path(root, ["rev-parse", "--show-toplevel"]);
        let git_dir = git_path(root, ["rev-parse", "--absolute-git-dir"]);
        let common_dir = git_path(root, ["rev-parse", "--git-common-dir"]);

        let canonical_root = top
            .as_deref()
            .map(canonical_existing_or_parent)
            .transpose()?
            .unwrap_or(canonical_root);
        let canonical_git_dir = git_dir
            .as_deref()
            .map(canonical_existing_or_parent)
            .transpose()?;
        let canonical_common_git_dir = common_dir
            .as_deref()
            .map(|p| {
                if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    canonical_root.join(p)
                }
            })
            .map(|p| canonical_existing_or_parent(&p))
            .transpose()?;

        let repo_key = canonical_common_git_dir
            .as_ref()
            .or(canonical_git_dir.as_ref())
            .unwrap_or(&canonical_root);
        let repo_id = stable_key(&format!("repo:{}", normalized_path(repo_key)));
        let worktree_id = stable_key(&format!(
            "worktree:{}:{}",
            normalized_path(&canonical_root),
            canonical_git_dir
                .as_ref()
                .map(|p| normalized_path(p))
                .unwrap_or_else(|| "non-git".to_string())
        ));
        Ok(Self {
            repo_id,
            worktree_id,
            canonical_root,
            canonical_git_dir,
            canonical_common_git_dir,
        })
    }

    pub fn lock_root(&self) -> PathBuf {
        crate::core::config::aizen_home()
            .join("locks")
            .join(format!("v{LOCK_PROTOCOL}"))
            .join("repo")
            .join(&self.repo_id)
    }

    pub fn repository_store_lock(&self) -> PathBuf {
        self.lock_root().join("store.lock")
    }

    pub fn workspace_lock(&self) -> PathBuf {
        self.lock_root()
            .join("worktrees")
            .join(&self.worktree_id)
            .join("workspace.lock")
    }

    pub fn timemachine_lock(&self) -> PathBuf {
        self.lock_root()
            .join("worktrees")
            .join(&self.worktree_id)
            .join("timemachine.lock")
    }
}

pub struct WorkspaceWriterLease {
    identity: WorkspaceIdentity,
    _locks: LockSet,
}

impl WorkspaceWriterLease {
    pub fn acquire(
        root: &Path,
        timeout: Duration,
        cancel: Option<&crate::core::cancel::TurnCancel>,
        operation: &str,
    ) -> Result<Self> {
        let identity = WorkspaceIdentity::discover_cached(root)?;
        Self::acquire_identity(identity, timeout, cancel, operation)
    }

    pub fn acquire_identity(
        identity: WorkspaceIdentity,
        timeout: Duration,
        cancel: Option<&crate::core::cancel::TurnCancel>,
        operation: &str,
    ) -> Result<Self> {
        let owner = LockOwner::new(format!("{}-{}", std::process::id(), now_unix()), operation);
        let locks = LockSet::acquire(
            vec![
                LockRequest::new(
                    LockClass::RepositoryStore,
                    identity.repository_store_lock(),
                    LockMode::Shared,
                    "repository store",
                ),
                LockRequest::new(
                    LockClass::Workspace,
                    identity.workspace_lock(),
                    LockMode::Exclusive,
                    "workspace writer",
                ),
            ],
            timeout,
            cancel,
            &owner,
        )?;
        Ok(Self {
            identity,
            _locks: locks,
        })
    }

    pub fn time_machine_lock(&self) -> Result<RepoTxnLock> {
        RepoTxnLock::acquire_exclusive(&self.identity.timemachine_lock(), Duration::from_secs(5))
    }

    pub fn identity(&self) -> &WorkspaceIdentity {
        &self.identity
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LockClass {
    Capacity = 1,
    RepositoryStore = 2,
    Workspace = 3,
    TimeMachine = 4,
    Resource = 5,
}

#[derive(Debug, Clone)]
pub struct LockRequest {
    pub class: LockClass,
    pub path: PathBuf,
    pub mode: LockMode,
    pub label: String,
}

impl LockRequest {
    pub fn new(class: LockClass, path: PathBuf, mode: LockMode, label: impl Into<String>) -> Self {
        Self {
            class,
            path,
            mode,
            label: label.into(),
        }
    }
}

pub struct LockSet {
    held: Vec<HeldLock>,
}

struct HeldLock {
    _lock: RepoTxnLock,
    owner_path: PathBuf,
}

impl LockSet {
    pub fn acquire(
        mut requests: Vec<LockRequest>,
        timeout: Duration,
        cancel: Option<&crate::core::cancel::TurnCancel>,
        owner: &LockOwner,
    ) -> Result<Self> {
        requests.sort_by(|a, b| {
            (a.class, &a.path, mode_rank(a.mode)).cmp(&(b.class, &b.path, mode_rank(b.mode)))
        });
        for pair in requests.windows(2) {
            if pair[0].path == pair[1].path {
                anyhow::bail!("duplicate lock request for {}", pair[0].path.display());
            }
        }

        let mut held = Vec::with_capacity(requests.len());
        for request in requests {
            let lock = RepoTxnLock::acquire_mode(&request.path, request.mode, timeout, cancel)
                .with_context(|| format!("acquiring {}", request.label))?;
            let owner_path = owner_path(&request.path);
            write_owner(&owner_path, owner, &request);
            held.push(HeldLock {
                _lock: lock,
                owner_path,
            });
        }
        Ok(Self { held })
    }

    pub fn len(&self) -> usize {
        self.held.len()
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty()
    }
}

impl Drop for LockSet {
    fn drop(&mut self) {
        for held in self.held.iter().rev() {
            let _ = std::fs::remove_file(&held.owner_path);
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct LockOwner {
    protocol: u32,
    pid: u32,
    binary_version: &'static str,
    run_id: String,
    operation: String,
    acquired_unix: u64,
}

impl LockOwner {
    pub fn new(run_id: impl Into<String>, operation: impl Into<String>) -> Self {
        Self {
            protocol: LOCK_PROTOCOL,
            pid: std::process::id(),
            binary_version: env!("CARGO_PKG_VERSION"),
            run_id: run_id.into(),
            operation: operation.into(),
            acquired_unix: now_unix(),
        }
    }
}

#[derive(Serialize)]
struct OwnerRecord<'a> {
    #[serde(flatten)]
    owner: &'a LockOwner,
    resource: &'a str,
    mode: &'static str,
}

pub fn stable_key(value: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, value.as_bytes());
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

pub fn resource_lock(kind: &str, logical_key: &str) -> PathBuf {
    crate::core::config::aizen_home()
        .join("locks")
        .join(format!("v{LOCK_PROTOCOL}"))
        .join("resources")
        .join(safe_segment(kind))
        .join(format!("{}.lock", stable_key(logical_key)))
}

pub fn store_lock(kind: &str, logical_key: &str) -> PathBuf {
    crate::core::config::aizen_home()
        .join("locks")
        .join(format!("v{LOCK_PROTOCOL}"))
        .join("stores")
        .join(safe_segment(kind))
        .join(format!("{}.lock", stable_key(logical_key)))
}

fn write_owner(path: &Path, owner: &LockOwner, request: &LockRequest) {
    let record = OwnerRecord {
        owner,
        resource: &request.label,
        mode: match request.mode {
            LockMode::Shared => "shared",
            LockMode::Exclusive => "exclusive",
        },
    };
    if let Ok(bytes) = serde_json::to_vec_pretty(&record) {
        let _ = crate::core::persist::atomic_write_owner_only(path, &bytes);
    }
}

fn owner_path(lock: &Path) -> PathBuf {
    let mut name = lock.file_name().unwrap_or_default().to_os_string();
    name.push(".owner.json");
    lock.with_file_name(name)
}

fn mode_rank(mode: LockMode) -> u8 {
    match mode {
        LockMode::Shared => 0,
        LockMode::Exclusive => 1,
    }
}

fn git_path<const N: usize>(root: &Path, args: [&str; N]) -> Option<PathBuf> {
    let out = crate::core::gitx::command()
        .ok()?
        .current_dir(root)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).ok()?;
    let path = text.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn canonical_existing_or_parent(path: &Path) -> Result<PathBuf> {
    if path.exists() {
        return std::fs::canonicalize(path)
            .with_context(|| format!("canonicalizing {}", path.display()));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let canonical_parent = std::fs::canonicalize(parent)
        .with_context(|| format!("canonicalizing parent {}", parent.display()))?;
    Ok(path
        .file_name()
        .map(|n| canonical_parent.join(n))
        .unwrap_or(canonical_parent))
}

fn normalized_path(path: &Path) -> String {
    let mut value = path.to_string_lossy().replace('\\', "/");
    if let Some(rest) = value.strip_prefix("//?/") {
        value = rest.to_string();
    }
    if value.as_bytes().get(1) == Some(&b':') {
        value.replace_range(0..1, &value[..1].to_ascii_lowercase());
    }
    value.trim_end_matches('/').to_string()
}

fn safe_segment(value: &str) -> String {
    let out: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "resource".to_string()
    } else {
        out
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_keys_hide_the_input_and_are_deterministic() {
        let input = "secret-token-value";
        let a = stable_key(input);
        assert_eq!(a, stable_key(input));
        assert_eq!(a.len(), 64);
        assert!(!a.contains(input));
    }

    #[test]
    fn different_non_git_roots_get_different_worktree_ids() {
        let base = std::env::temp_dir().join(format!(
            "aizen-workspace-id-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        let a = base.join("a");
        let b = base.join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let ia = WorkspaceIdentity::discover(&a).unwrap();
        let ib = WorkspaceIdentity::discover(&b).unwrap();
        assert_ne!(ia.worktree_id, ib.worktree_id);
        let _ = std::fs::remove_dir_all(base);
    }

    #[test]
    fn lock_set_sorts_requests_and_releases_all() {
        let base = std::env::temp_dir().join(format!(
            "aizen-lock-set-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        let a = base.join("a.lock");
        let b = base.join("b.lock");
        let owner = LockOwner::new("test", "ordered locks");
        let locks = LockSet::acquire(
            vec![
                LockRequest::new(
                    LockClass::Workspace,
                    b.clone(),
                    LockMode::Exclusive,
                    "workspace",
                ),
                LockRequest::new(
                    LockClass::RepositoryStore,
                    a.clone(),
                    LockMode::Shared,
                    "store",
                ),
            ],
            Duration::from_millis(100),
            None,
            &owner,
        )
        .unwrap();
        assert_eq!(locks.len(), 2);
        drop(locks);
        RepoTxnLock::acquire_exclusive(&b, Duration::from_millis(100)).unwrap();
        let _ = std::fs::remove_dir_all(base);
    }
}
