//! Crash-aware recovery lease + transactional sidecars.
//!
//! The manifest is the commit record. History/draft sidecars are generation-specific and owner-only;
//! a new manifest is published only after every sidecar is durable. Recovery never replays a model or
//! tool call: it restores the last safe conversation boundary and places the interrupted request back
//! in the input box for review.

use crate::core::persist::{atomic_write_owner_only, remove_if_exists};
use crate::core::types::Message;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const SCHEMA: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPhase {
    Idle,
    WaitingModel,
    ExecutingTools,
    AwaitingApproval,
    Finalizing,
}

impl RecoveryPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::WaitingModel => "waiting_model",
            Self::ExecutingTools => "executing_tools",
            Self::AwaitingApproval => "awaiting_approval",
            Self::Finalizing => "finalizing",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryManifest {
    pub schema: u32,
    pub binary_version: String,
    pub pid: u32,
    pub run_id: String,
    /// Stable project label for diagnostics.
    pub repo: String,
    /// Exact canonical repository/worktree scope used to filter offers.
    pub repo_scope: String,
    pub phase: RecoveryPhase,
    pub session_name: Option<String>,
    pub safe_history_len: usize,
    pub checkpoint_id: Option<u32>,
    pub sidecar_generation: u64,
    pub turn_id: Option<u64>,
    /// Monotonic within a turn: once a potentially effecting tool starts, keep this true until a
    /// complete post-turn Idle boundary is durably committed.
    pub side_effects_possible: bool,
    pub updated_unix: u64,
}

#[derive(Debug, Clone)]
pub struct RecoveryOffer {
    pub path: PathBuf,
    pub manifest: RecoveryManifest,
    pub history: Vec<Message>,
    pub pending_draft: Option<String>,
}

static ACTIVE: Mutex<Option<ActiveLease>> = Mutex::new(None);

struct ActiveLease {
    dir: PathBuf,
    manifest: RecoveryManifest,
    _lock: crate::core::repo_lock::RepoTxnLock,
}

fn now_unix() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub fn current_repo_scope() -> String {
    let root = crate::core::config::project_root();
    fs::canonicalize(&root).unwrap_or(root).display().to_string()
}

fn recovery_root() -> PathBuf {
    crate::core::config::nextgen_home().join("recovery")
}

fn lease_dir(run_id: &str) -> PathBuf {
    recovery_root().join(run_id)
}

fn lock_path(dir: &Path) -> PathBuf {
    dir.join("lease.lock")
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("manifest.json")
}

fn history_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("history.{generation}.json"))
}

fn draft_path(dir: &Path, generation: u64) -> PathBuf {
    dir.join(format!("draft.{generation}.txt"))
}

fn new_run_id() -> String {
    format!("{}-{}", now_unix(), std::process::id())
}

/// Begin a recovery lease for the current process. Best-effort: failure never blocks the REPL.
pub fn begin(repo_scope: impl Into<String>, session_name: Option<String>) {
    let run_id = new_run_id();
    let dir = lease_dir(&run_id);
    let _ = fs::create_dir_all(&dir);
    crate::core::config::harden_dir(&dir);
    let lock = match crate::core::repo_lock::RepoTxnLock::acquire(&lock_path(&dir), Duration::from_secs(1)) {
        Ok(l) => l,
        Err(_) => return,
    };
    let manifest = RecoveryManifest {
        schema: SCHEMA,
        binary_version: env!("CARGO_PKG_VERSION").to_string(),
        pid: std::process::id(),
        run_id,
        repo: crate::core::config::project_slug(),
        repo_scope: repo_scope.into(),
        phase: RecoveryPhase::Idle,
        session_name,
        safe_history_len: 0,
        checkpoint_id: None,
        sidecar_generation: 0,
        turn_id: None,
        side_effects_possible: false,
        updated_unix: now_unix(),
    };
    if write_manifest(&dir, &manifest).is_err() {
        return;
    }
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(ActiveLease { dir, manifest, _lock: lock });
}

fn write_manifest(dir: &Path, manifest: &RecoveryManifest) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest)?;
    atomic_write_owner_only(&manifest_path(dir), &bytes)
}

/// Persist the safe conversation + optional unsent draft as one committed generation.
///
/// Sidecars are written first and the manifest last. On any failure the previous manifest/generation
/// remains authoritative and the incomplete new sidecars are removed best-effort.
pub fn checkpoint_history(
    history: &[Message],
    pending_draft: Option<&str>,
    phase: RecoveryPhase,
) -> Result<()> {
    let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = guard.as_mut() else { return Ok(()) };
    let old = active.manifest.clone();
    let generation = old.sidecar_generation.saturating_add(1);
    let hist_path = history_path(&active.dir, generation);
    let draft_path_new = draft_path(&active.dir, generation);
    let result = (|| -> Result<()> {
        atomic_write_owner_only(&hist_path, &serde_json::to_vec(history)?)?;
        if let Some(draft) = pending_draft {
            atomic_write_owner_only(&draft_path_new, draft.as_bytes())?;
        }
        let mut next = old.clone();
        next.phase = phase;
        next.safe_history_len = history.len();
        next.sidecar_generation = generation;
        next.turn_id = pending_draft.map(|_| generation);
        next.updated_unix = now_unix();
        if phase == RecoveryPhase::Idle {
            next.side_effects_possible = false;
            next.checkpoint_id = None;
            next.turn_id = None;
        }
        write_manifest(&active.dir, &next)?;
        active.manifest = next;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&hist_path);
        let _ = fs::remove_file(&draft_path_new);
        return Err(e);
    }
    if old.sidecar_generation > 0 {
        let _ = remove_if_exists(&history_path(&active.dir, old.sidecar_generation));
        let _ = remove_if_exists(&draft_path(&active.dir, old.sidecar_generation));
    }
    Ok(())
}

pub fn set_phase(phase: RecoveryPhase) {
    let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = guard.as_mut() else { return };
    let old = active.manifest.clone();
    active.manifest.phase = phase;
    active.manifest.updated_unix = now_unix();
    if write_manifest(&active.dir, &active.manifest).is_err() {
        active.manifest = old;
    }
}

pub fn mark_side_effects_possible() {
    let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = guard.as_mut() else { return };
    if active.manifest.side_effects_possible {
        return;
    }
    let old = active.manifest.clone();
    active.manifest.side_effects_possible = true;
    active.manifest.updated_unix = now_unix();
    if write_manifest(&active.dir, &active.manifest).is_err() {
        active.manifest = old;
    }
}

pub fn set_checkpoint(id: Option<u32>) {
    let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = guard.as_mut() else { return };
    let old = active.manifest.clone();
    active.manifest.checkpoint_id = id;
    active.manifest.updated_unix = now_unix();
    if write_manifest(&active.dir, &active.manifest).is_err() {
        active.manifest = old;
    }
}

pub fn set_session_name(name: Option<String>) {
    let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = guard.as_mut() else { return };
    let old = active.manifest.clone();
    active.manifest.session_name = name;
    active.manifest.updated_unix = now_unix();
    if write_manifest(&active.dir, &active.manifest).is_err() {
        active.manifest = old;
    }
}

/// Clean exit: drop the lease and delete recovery artifacts for this process.
pub fn clear() {
    let active = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(active) = active {
        let _ = fs::remove_dir_all(&active.dir);
    }
}

/// Scan for stale recovery leases from previous processes in exactly this repository/worktree.
pub fn scan_stale(repo_scope: &str) -> Vec<RecoveryOffer> {
    let root = recovery_root();
    let Ok(rd) = fs::read_dir(&root) else { return Vec::new() };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Ok(g) = ACTIVE.lock() {
            if g.as_ref().is_some_and(|a| a.dir == path) {
                continue;
            }
        }
        let Ok(_lock) = crate::core::repo_lock::RepoTxnLock::acquire(&lock_path(&path), Duration::from_millis(50)) else {
            continue;
        };
        let Ok(raw) = fs::read_to_string(manifest_path(&path)) else {
            quarantine(&path);
            continue;
        };
        let Ok(manifest) = serde_json::from_str::<RecoveryManifest>(&raw) else {
            quarantine(&path);
            continue;
        };
        if manifest.schema != SCHEMA {
            quarantine(&path);
            continue;
        }
        if manifest.repo_scope != repo_scope {
            continue;
        }
        if manifest.sidecar_generation == 0 || manifest.safe_history_len == 0 {
            continue;
        }
        let Some(mut history) = fs::read(history_path(&path, manifest.sidecar_generation))
            .ok()
            .and_then(|b| serde_json::from_slice::<Vec<Message>>(&b).ok())
        else {
            quarantine(&path);
            continue;
        };
        if history.len() < manifest.safe_history_len {
            quarantine(&path);
            continue;
        }
        history.truncate(manifest.safe_history_len);
        let pending_draft = fs::read_to_string(draft_path(&path, manifest.sidecar_generation))
            .ok()
            .filter(|s| !s.trim().is_empty());
        out.push(RecoveryOffer { path, manifest, history, pending_draft });
    }
    out.sort_by_key(|o| std::cmp::Reverse(o.manifest.updated_unix));
    out
}

fn quarantine(path: &Path) {
    let stem = path.file_name().and_then(|s| s.to_str()).unwrap_or("bad");
    let mut q = recovery_root().join(format!("quarantine-{stem}"));
    if q.exists() {
        q = recovery_root().join(format!("quarantine-{stem}-{}", now_unix()));
    }
    let _ = fs::rename(path, q);
}

/// Accept one recovery offer: restore its conversation and delete the lease directory.
pub fn accept(offer: &RecoveryOffer) -> Result<(Vec<Message>, Option<String>)> {
    let history = offer.history.clone();
    let draft = offer.pending_draft.clone();
    fs::remove_dir_all(&offer.path).with_context(|| format!("removing restored lease {}", offer.path.display()))?;
    Ok((history, draft))
}

/// Discard one recovery offer.
pub fn discard(offer: &RecoveryOffer) -> Result<()> {
    fs::remove_dir_all(&offer.path).with_context(|| format!("removing {}", offer.path.display()))?;
    Ok(())
}

/// Human-readable banner for a recovery offer. Draft contents are deliberately not echoed.
pub fn format_offer(offer: &RecoveryOffer) -> String {
    let phase = offer.manifest.phase.as_str();
    let draft = offer.pending_draft.as_ref().map(|_| " · draft saved").unwrap_or_default();
    let ckpt = offer.manifest.checkpoint_id.map(|id| format!(" · checkpoint #{id}")).unwrap_or_default();
    let effects = if offer.manifest.side_effects_possible { " · side effects possible" } else { "" };
    format!(
        "recoverable session · phase={phase} · {} msgs{ckpt}{effects}{draft}",
        offer.history.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(scope: &str, generation: u64, len: usize) -> RecoveryManifest {
        RecoveryManifest {
            schema: SCHEMA,
            binary_version: env!("CARGO_PKG_VERSION").into(),
            pid: 0,
            run_id: "stale".into(),
            repo: "repo".into(),
            repo_scope: scope.into(),
            phase: RecoveryPhase::WaitingModel,
            session_name: None,
            safe_history_len: len,
            checkpoint_id: None,
            sidecar_generation: generation,
            turn_id: Some(generation),
            side_effects_possible: false,
            updated_unix: 1,
        }
    }

    #[test]
    fn phase_names_are_stable() {
        assert_eq!(RecoveryPhase::ExecutingTools.as_str(), "executing_tools");
        assert_eq!(RecoveryPhase::WaitingModel.as_str(), "waiting_model");
    }

    #[test]
    fn stale_offer_is_repo_scoped_and_requires_complete_generation() {
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("aizen-recovery-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        std::env::set_var("AIZEN_HOME", &root);
        let dir = recovery_root().join("stale");
        fs::create_dir_all(&dir).unwrap();
        let m = manifest("repo-a", 7, 2);
        write_manifest(&dir, &m).unwrap();
        atomic_write_owner_only(
            &history_path(&dir, 7),
            &serde_json::to_vec(&vec![Message::system("s")]).unwrap(),
        )
        .unwrap();
        assert!(scan_stale("repo-a").is_empty(), "truncated generation is never offered");
        assert!(scan_stale("repo-b").is_empty(), "foreign repository lease is invisible");
        std::env::remove_var("AIZEN_HOME");
        let _ = fs::remove_dir_all(&root);
    }
}
