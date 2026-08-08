//! Multi-session cooperation: who else is working in this repository, on what, and which files
//! they touched.
//!
//! Aizen is routinely opened in several terminals at once against ONE working tree — auth in this
//! window, the TUI in that one, tests in a third, and a last window that reviews and commits.
//! Git sees only the union of all of it: `git diff` cannot say which line belongs to which window,
//! so the reviewing window has no way to check one task's work, and no safe way to commit only it.
//!
//! This module is the missing bookkeeping. Every session publishes a manifest; every turn that
//! touched the workspace appends the files it changed to that manifest's ledger, keyed by the
//! pre-edit checkpoint the change is measured FROM. That key is what makes per-session diff
//! possible after the fact: even once a later session overwrites the same file, this session's
//! pre-image is still reachable as a blob inside its own checkpoint tree.
//!
//! Why turn-level attribution is exact rather than a guess: [`crate::core::workspace_txn::
//! WorkspaceWriterLease`] is acquired EXCLUSIVE on the first writing tool of a turn and held until
//! the turn ends (see `agent::execute_calls`). Two aizen sessions on one worktree are therefore
//! already serialized at turn granularity, so "everything that changed between this turn's pre-edit
//! checkpoint and now" is exactly this turn's work. Edits made by an external editor, or by git run
//! by hand, land inside that window and are attributed to whoever held the lease — documented, not
//! silently corrected.
//!
//! # Conflict handling, in the three places it actually happens
//!
//! The lease serializes turns; it does NOT make a read-modify-write atomic across turns. Session A
//! reads a file in turn 1, B rewrites it in turn 2, A writes in turn 3 — every lock honored at every
//! instant, and the logical operation still torn. Three defenses, each at the layer that can see the
//! problem:
//!
//! 1. **`file_edit` / `multi_edit`** match `old_string` against a FRESH read, so a rewritten region
//!    fails to match on its own. Nothing to add.
//! 2. **`file_write` / `file_move --overwrite`** have no anchor — a whole-file overwrite whose CAS
//!    passes against whatever is on disk right now. [`crate::core::read_ledger`] is the anchor: what
//!    THIS session last saw, per path, refusing the overwrite when the ground moved.
//! 3. **Commit** takes the lease for the stage step (`git add` reads a tree a peer may be writing)
//!    and pins the reviewed index by its tree hash, so what lands is what was approved.
//!
//! Overlap records remain a WARNING and never block: the second writer is frequently the point, and
//! git cannot split one file by author anyway. What must not happen silently is losing the first
//! writer's bytes, which is what the ledger above prevents.
//!
//! Layout (`repo_id`, so linked worktrees of one repository share a registry):
//!
//! ```text
//! ~/.aizen/coop/<repo_id>/
//!   sessions/<session_id>.json   one session's manifest — written ONLY by its owner
//!   sessions/<session_id>.lock   held for the process lifetime ⇒ liveness (see `alive`)
//!   shared.json                  claims / overlaps / committed — written by anyone under a lock
//! ```
//!
//! Liveness needs no heartbeat: the owner holds an OS lock on its `.lock` for as long as it runs,
//! and the OS releases it on exit however the process died. A reader that CAN take that lock is
//! looking at a session that is gone (`abandoned`) — the same trick [`crate::core::recovery`] uses.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::core::persist::atomic_write_owner_only;
use crate::core::repo_lock::RepoTxnLock;
use crate::core::workspace_txn::{store_lock, WorkspaceIdentity};
use crate::features::timemachine::{self, DiffReport, DiffSide};

/// Manifest schema. A manifest from a newer binary is left alone; an unparseable one is swept.
const SCHEMA: u32 = 1;
/// Shared claims file schema.
const SHARED_SCHEMA: u32 = 1;

/// Ceiling on tracked paths per session — a runaway generated-file turn must not grow the manifest
/// without bound. Overflow is reported in `truncated_files`, never silently dropped.
const MAX_FILES: usize = 4000;
/// Ceiling on retained overlap records.
const MAX_OVERLAPS: usize = 500;
/// Finished/committed manifests older than this are swept on read.
const RETENTION_SECS: u64 = 7 * 24 * 3600;
/// How long `seal_turn` waits for the workspace writer lease before proceeding without it.
const SEAL_LEASE_WAIT: Duration = Duration::from_secs(5);
/// Wall-clock ceiling for the plumbing git calls this module makes itself.
const GIT_TIMEOUT: Duration = Duration::from_secs(60);
const GIT_DRAIN_GRACE: Duration = Duration::from_millis(500);

/// What a session is doing right now, as published by the session itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    /// At the prompt, waiting for the user.
    Idle,
    /// A turn is in flight.
    Working,
    /// Blocked on an approval prompt.
    AwaitingApproval,
    /// The user declared this window's task complete (`/team done`).
    Done,
    /// The process exited cleanly.
    Finished,
    /// The last turn ended in an error.
    Failed,
}

impl SessionState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::AwaitingApproval => "awaiting-approval",
            Self::Done => "done",
            Self::Finished => "finished",
            Self::Failed => "failed",
        }
    }

    /// Whether this state implies the process should still be running (so a released lock means it
    /// died rather than left).
    fn expects_live_process(self) -> bool {
        matches!(
            self,
            Self::Idle | Self::Working | Self::AwaitingApproval | Self::Done | Self::Failed
        )
    }
}

/// One file this session changed, and the checkpoint its pre-image lives in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TouchedFile {
    /// Repository-relative path exactly as git reports it.
    pub path: String,
    /// Pre-edit checkpoint of the turn that FIRST changed this file in this session. The pre-image
    /// is a blob in that checkpoint's tree, so it survives later overwrites by other sessions.
    pub base: u32,
    /// Pre-edit checkpoint of EVERY turn of this session that changed this path, ascending.
    ///
    /// `base` is the first of these and is what a whole-file diff measures from. This list is what
    /// makes a *shared* file separable: each entry opens an interval that contains one turn by this
    /// session and no other, so replaying just these intervals reconstructs this session's version
    /// of a file another session also edited (see [`split_shared`]). Empty on manifests written
    /// before the field existed — readers fall back to `base`.
    #[serde(default)]
    pub bases: Vec<u32>,
    /// Turn number (within this session) of the most recent change.
    pub last_turn: u64,
    /// Latest git status letter seen for this path (`A`/`M`/`D`/`R`/`C`/`T`).
    pub status: char,
}

/// One session's published state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionManifest {
    pub schema: u32,
    pub session_id: String,
    pub pid: u32,
    pub binary_version: String,
    pub repo_id: String,
    pub worktree_id: String,
    /// Canonical worktree root this session works in.
    pub root: String,
    #[serde(default)]
    pub branch: Option<String>,
    /// Saved-session slug (`~/.aizen/sessions/<name>.json`), when there is one.
    #[serde(default)]
    pub session_name: Option<String>,
    /// What this window is doing, for another window to read.
    #[serde(default)]
    pub task: String,
    /// Set by `/team task` — pinned, never overwritten by the automatic description.
    #[serde(default)]
    pub task_pinned: bool,
    pub state: SessionState,
    #[serde(default)]
    pub turns: u64,
    /// Pre-edit checkpoint of the turn currently in flight, set by [`note_turn_base`].
    #[serde(default)]
    pub base_checkpoint: Option<u32>,
    #[serde(default)]
    pub files: Vec<TouchedFile>,
    /// Paths dropped because the ledger hit [`MAX_FILES`].
    #[serde(default)]
    pub truncated_files: u64,
    /// Set when a turn touched the workspace but no checkpoint could be taken (no git, or not a
    /// repository) — per-session diff is unavailable and the reason is worth showing.
    #[serde(default)]
    pub degraded: Option<String>,
    pub started_unix: u64,
    pub updated_unix: u64,
}

/// Cross-session state. Written by any session, always under one lock.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shared {
    #[serde(default = "shared_schema")]
    pub schema: u32,
    /// path → the session that most recently claimed it.
    #[serde(default)]
    pub owners: BTreeMap<String, Claim>,
    #[serde(default)]
    pub overlaps: Vec<Overlap>,
    /// session_id → when its changes were committed.
    #[serde(default)]
    pub committed: BTreeMap<String, u64>,
}

fn shared_schema() -> u32 {
    SHARED_SCHEMA
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            schema: SHARED_SCHEMA,
            owners: BTreeMap::new(),
            overlaps: Vec::new(),
            committed: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub session_id: String,
    pub unix: u64,
}

/// Two sessions changed one file. Recorded for both, blocking neither.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Overlap {
    pub path: String,
    /// The session that claimed the path first.
    pub first: String,
    /// The session that changed it afterwards.
    pub second: String,
    pub unix: u64,
}

/// A manifest plus what the reader can only work out at read time.
#[derive(Debug, Clone)]
pub struct SessionView {
    pub manifest: SessionManifest,
    /// The owning process still holds its lock.
    pub alive: bool,
    /// This is the reading process's own session.
    pub is_self: bool,
    /// Its changes have been committed by a coordinator.
    pub committed: bool,
    /// Files it shares with another session.
    pub overlapping: Vec<String>,
}

impl SessionView {
    /// State to SHOW: a session whose process is gone but which never said goodbye is abandoned.
    pub fn effective_state(&self) -> &'static str {
        if self.committed {
            return "committed";
        }
        if !self.alive && self.manifest.state.expects_live_process() {
            return "abandoned";
        }
        self.manifest.state.as_str()
    }

    /// Whether a coordinator may commit this session's work without `--force`.
    pub fn ready_to_commit(&self) -> bool {
        !self.alive
            || matches!(
                self.manifest.state,
                SessionState::Done | SessionState::Finished | SessionState::Failed
            )
    }

    /// Short label for the worktree: `.` for the main tree, else its directory name.
    pub fn worktree_label(&self) -> String {
        Path::new(&self.manifest.root)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string()
    }
}

struct Active {
    manifest: SessionManifest,
    _lock: RepoTxnLock,
}

static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Strip control characters and clamp length. Manifests are read by another process and rendered
/// into a live TUI: an escape sequence in a task description must never reach the terminal.
///
/// Runs of whitespace collapse to one space. Neutralizing a control character by replacing it
/// leaves a gap next to the space that usually precedes it, and a task line is rendered as ONE row
/// in `/team status` — ragged internal spacing there is just noise.
fn safe_text(value: &str, max: usize) -> String {
    let neutralized = value
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .take(max);
    let mut out = String::new();
    let mut in_space = false;
    for c in neutralized {
        if c.is_whitespace() {
            in_space = true;
            continue;
        }
        if in_space && !out.is_empty() {
            out.push(' ');
        }
        in_space = false;
        out.push(c);
    }
    out
}

fn coop_root(repo_id: &str) -> PathBuf {
    crate::core::config::aizen_home().join("coop").join(repo_id)
}

fn sessions_dir(repo_id: &str) -> PathBuf {
    coop_root(repo_id).join("sessions")
}

fn manifest_path(repo_id: &str, session_id: &str) -> PathBuf {
    sessions_dir(repo_id).join(format!("{session_id}.json"))
}

fn liveness_lock_path(repo_id: &str, session_id: &str) -> PathBuf {
    sessions_dir(repo_id).join(format!("{session_id}.lock"))
}

fn shared_path(repo_id: &str) -> PathBuf {
    coop_root(repo_id).join("shared.json")
}

/// Lock guarding [`Shared`]. Lives under `~/.aizen/locks/` like every other cross-process lock, so
/// the registry directory itself holds only data.
fn shared_lock(repo_id: &str) -> PathBuf {
    store_lock("coop-shared", repo_id)
}

/// Identity of the workspace this process cooperates in.
pub fn identity() -> Result<WorkspaceIdentity> {
    WorkspaceIdentity::discover_cached(&crate::core::config::project_root())
}

/// `repo_id` for the current workspace, or `None` when it cannot be determined (never fatal — the
/// whole module is best-effort bookkeeping).
fn current_repo_id() -> Option<String> {
    identity().ok().map(|i| i.repo_id)
}

// ---------------------------------------------------------------------------------------------
// Publishing this session
// ---------------------------------------------------------------------------------------------

/// Publish this process as a session in the current repository. Best-effort: any failure leaves the
/// REPL working exactly as before, just invisible to `/team`.
pub fn begin(session_name: Option<String>) {
    let Ok(id) = identity() else { return };
    let session_id = format!("{}-{}", now_unix(), std::process::id());
    let dir = sessions_dir(&id.repo_id);
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    crate::core::config::harden_dir(&coop_root(&id.repo_id));
    crate::core::config::harden_dir(&dir);
    // The liveness lock is taken FIRST and held for the process lifetime. Failing to take it means
    // another process owns this id (impossible in practice — the id carries our pid) so we bail
    // rather than publish a session whose liveness would read as dead.
    let Ok(lock) = RepoTxnLock::acquire_exclusive(
        &liveness_lock_path(&id.repo_id, &session_id),
        Duration::from_secs(1),
    ) else {
        return;
    };
    let manifest = SessionManifest {
        schema: SCHEMA,
        session_id,
        pid: std::process::id(),
        binary_version: env!("CARGO_PKG_VERSION").to_string(),
        repo_id: id.repo_id.clone(),
        worktree_id: id.worktree_id.clone(),
        root: id.canonical_root.display().to_string(),
        branch: current_branch(&id.canonical_root),
        session_name,
        task: String::new(),
        task_pinned: false,
        state: SessionState::Idle,
        turns: 0,
        base_checkpoint: None,
        files: Vec::new(),
        truncated_files: 0,
        degraded: None,
        started_unix: now_unix(),
        updated_unix: now_unix(),
    };
    if write_manifest(&manifest).is_err() {
        return;
    }
    *ACTIVE.lock().unwrap_or_else(|e| e.into_inner()) = Some(Active {
        manifest,
        _lock: lock,
    });
}

fn write_manifest(m: &SessionManifest) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(m)?;
    atomic_write_owner_only(&manifest_path(&m.repo_id, &m.session_id), &bytes)
}

/// Mutate this session's manifest and republish it. Rolls back in memory if the write failed, so
/// what other windows read is always what this one believes.
fn update<F: FnOnce(&mut SessionManifest)>(f: F) {
    let mut guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(active) = guard.as_mut() else { return };
    let before = active.manifest.clone();
    f(&mut active.manifest);
    active.manifest.updated_unix = now_unix();
    if write_manifest(&active.manifest).is_err() {
        active.manifest = before;
    }
}

/// This session's id, when it is published.
pub fn current_session_id() -> Option<String> {
    ACTIVE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|a| a.manifest.session_id.clone())
}

pub fn set_state(state: SessionState) {
    update(|m| m.state = state);
}

/// Pin the task description shown to other windows. An empty string un-pins it.
pub fn set_task(task: &str) {
    let text = safe_text(task, 160);
    update(|m| {
        if text.is_empty() {
            m.task_pinned = false;
            m.task = String::new();
        } else {
            m.task = text.clone();
            m.task_pinned = true;
        }
    });
}

/// Offer an automatic task description (the todo summary, or the first user prompt). Never
/// overwrites a description pinned with `/team task`.
pub fn suggest_task(text: &str) {
    let text = safe_text(text, 160);
    if text.is_empty() {
        return;
    }
    update(|m| {
        if !m.task_pinned {
            m.task = text.clone();
        }
    });
}

/// Record the pre-edit checkpoint of the turn now in flight. Called from the same place the agent
/// loop notes it for the time machine, so the two can never disagree about which tree a turn is
/// measured from.
pub fn note_turn_base(checkpoint: u32) {
    update(|m| {
        // First writing tool of the turn wins: later tools in the same turn share that pre-image.
        if m.base_checkpoint.is_none() {
            m.base_checkpoint = Some(checkpoint);
        }
        m.degraded = None;
    });
}

/// Record that a turn wrote to the workspace with no checkpoint available (no git, or not a
/// repository). The file list still accrues; only the diff is unavailable.
pub fn note_checkpoint_unavailable(reason: &str) {
    let reason = safe_text(reason, 120);
    update(|m| m.degraded = Some(reason.clone()));
}

/// Fold the finished turn into this session's ledger and return warnings worth showing the user.
///
/// Runs the diff from this turn's pre-edit checkpoint to the working tree, unions the result into
/// the ledger, and claims those paths. Called on EVERY turn end (success, cancel, or error) — a
/// cancelled turn can still have written files, and forgetting them is what makes a coordinator's
/// commit incomplete.
pub fn seal_turn() -> Vec<String> {
    let (repo_id, session_id, base, turn) = {
        let guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(a) = guard.as_ref() else {
            return Vec::new();
        };
        (
            a.manifest.repo_id.clone(),
            a.manifest.session_id.clone(),
            a.manifest.base_checkpoint,
            a.manifest.turns.saturating_add(1),
        )
    };
    update(|m| {
        m.turns = turn;
        m.base_checkpoint = None;
    });
    let Some(base) = base else {
        // The turn never touched the workspace; nothing to attribute.
        return Vec::new();
    };

    // Hold the writer lease while measuring, so no other session's turn can be mid-write and have
    // its work counted as ours. Best-effort: if the lease cannot be had we still measure, because
    // an unattributed change is worse than a slightly over-attributed one.
    let _lease = crate::core::workspace_txn::WorkspaceWriterLease::acquire(
        &crate::core::config::project_root(),
        SEAL_LEASE_WAIT,
        None,
        "coop seal",
    )
    .ok();

    let changed =
        match timemachine::diff(&DiffSide::Checkpoint(base), &DiffSide::Working, &[], None) {
            Ok(report) => report
                .files
                .iter()
                .flat_map(|f| {
                    // A rename touches both names: the new path is this session's, and the old path
                    // must be staged too or the commit would leave the original file behind.
                    let mut v = vec![(f.status, f.path.clone())];
                    if let Some(old) = &f.old_path {
                        v.push(('D', old.clone()));
                    }
                    v
                })
                .collect::<Vec<_>>(),
            Err(e) => {
                note_checkpoint_unavailable(&format!("diff from checkpoint #{base} failed: {e}"));
                return Vec::new();
            }
        };
    if changed.is_empty() {
        return Vec::new();
    }

    let mut overflow = 0u64;
    update(|m| {
        overflow = merge_touched(&mut m.files, &changed, base, turn);
        m.truncated_files = m.truncated_files.saturating_add(overflow);
    });

    let paths: Vec<String> = changed.iter().map(|(_, p)| p.clone()).collect();
    let mut warnings = claim_paths(&repo_id, &session_id, &paths).unwrap_or_default();
    if overflow > 0 {
        warnings.push(format!(
            "coop: this session has touched more than {MAX_FILES} files; {overflow} path(s) are not \
             tracked and would be missed by /team commit"
        ));
    }
    warnings
}

/// Union `changed` into `files`. Returns how many paths had to be dropped at [`MAX_FILES`].
///
/// `base` is recorded only the FIRST time a path appears: that is the checkpoint whose tree still
/// holds this session's pre-image of the file, and re-basing it on every later turn would silently
/// narrow the diff to the most recent turn's work.
fn merge_touched(
    files: &mut Vec<TouchedFile>,
    changed: &[(char, String)],
    base: u32,
    turn: u64,
) -> u64 {
    let mut dropped = 0u64;
    for (status, path) in changed {
        if let Some(existing) = files.iter_mut().find(|f| &f.path == path) {
            existing.last_turn = turn;
            existing.status = *status;
            // Every turn that touched the path is remembered, because each one opens an interval
            // holding only this session's work. Dropping the later ones would make a split silently
            // reconstruct just the first turn's version.
            if !existing.bases.contains(&base) {
                existing.bases.push(base);
                existing.bases.sort_unstable();
            }
            continue;
        }
        if files.len() >= MAX_FILES {
            dropped += 1;
            continue;
        }
        files.push(TouchedFile {
            path: path.clone(),
            base,
            last_turn: turn,
            status: *status,
            bases: vec![base],
        });
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    dropped
}

/// Clean shutdown: stop expecting a live process, but KEEP the manifest. A window that finishes its
/// task and is closed is exactly the window whose diff the coordinator still needs to review.
pub fn clear() {
    let taken = ACTIVE.lock().unwrap_or_else(|e| e.into_inner()).take();
    if let Some(active) = taken {
        let mut m = active.manifest;
        if m.state != SessionState::Done {
            m.state = SessionState::Finished;
        }
        m.base_checkpoint = None;
        m.updated_unix = now_unix();
        let _ = write_manifest(&m);
    }
}

// ---------------------------------------------------------------------------------------------
// Shared claims
// ---------------------------------------------------------------------------------------------

fn load_shared(repo_id: &str) -> Shared {
    let Ok(bytes) = fs::read(shared_path(repo_id)) else {
        return Shared::default();
    };
    serde_json::from_slice::<Shared>(&bytes)
        .ok()
        .filter(|s| s.schema == SHARED_SCHEMA)
        .unwrap_or_default()
}

fn save_shared(repo_id: &str, shared: &Shared) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(shared)?;
    let _ = fs::create_dir_all(coop_root(repo_id));
    atomic_write_owner_only(&shared_path(repo_id), &bytes)
}

/// Read-modify-write [`Shared`] under its lock.
fn with_shared<T, F: FnOnce(&mut Shared) -> T>(repo_id: &str, f: F) -> Result<T> {
    let _lock = RepoTxnLock::acquire_exclusive(&shared_lock(repo_id), Duration::from_secs(5))?;
    let mut shared = load_shared(repo_id);
    let out = f(&mut shared);
    save_shared(repo_id, &shared)?;
    Ok(out)
}

/// Claim `paths` for `session_id`, recording an overlap wherever another session got there first.
/// Never refuses: two sessions editing one file is a fact to report, not an error to block on.
fn claim_paths(repo_id: &str, session_id: &str, paths: &[String]) -> Result<Vec<String>> {
    with_shared(repo_id, |shared| {
        let now = now_unix();
        let mut warnings = Vec::new();
        for path in paths {
            match shared.owners.get(path) {
                Some(claim) if claim.session_id != session_id => {
                    let first = claim.session_id.clone();
                    let already = shared.overlaps.iter().any(|o| {
                        &o.path == path
                            && ((o.first == first && o.second == session_id)
                                || (o.first == session_id && o.second == first))
                    });
                    if !already {
                        if shared.overlaps.len() >= MAX_OVERLAPS {
                            shared.overlaps.remove(0);
                        }
                        shared.overlaps.push(Overlap {
                            path: path.clone(),
                            first: first.clone(),
                            second: session_id.to_string(),
                            unix: now,
                        });
                        warnings.push(format!(
                            "⚠ {path} was also changed by session {first} — both sets of changes \
                             are in the file; git cannot split them by line"
                        ));
                    }
                }
                _ => {}
            }
            shared.owners.insert(
                path.clone(),
                Claim {
                    session_id: session_id.to_string(),
                    unix: now,
                },
            );
        }
        warnings
    })
}

// ---------------------------------------------------------------------------------------------
// Reading the registry
// ---------------------------------------------------------------------------------------------

/// Is the session that owns `lock` still running? A lock we can take is a lock nobody holds.
fn alive(repo_id: &str, session_id: &str) -> bool {
    let path = liveness_lock_path(repo_id, session_id);
    if !path.exists() {
        return false;
    }
    RepoTxnLock::acquire_exclusive(&path, Duration::from_millis(50)).is_err()
}

/// Every session known in this repository, newest activity first.
///
/// Sweeps debris as it goes: unparseable manifests, and finished/committed ones past
/// [`RETENTION_SECS`].
pub fn list() -> Vec<SessionView> {
    let Some(repo_id) = current_repo_id() else {
        return Vec::new();
    };
    list_in(&repo_id)
}

fn list_in(repo_id: &str) -> Vec<SessionView> {
    let Ok(rd) = fs::read_dir(sessions_dir(repo_id)) else {
        return Vec::new();
    };
    let shared = load_shared(repo_id);
    let mine = current_session_id();
    let now = now_unix();
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        let manifest = fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<SessionManifest>(&b).ok());
        let Some(manifest) = manifest.filter(|m| m.schema <= SCHEMA) else {
            // Debris or a manifest from a newer binary: sweep only what we could not read at all,
            // and leave a readable future schema for the binary that owns it.
            let _ = crate::core::persist::remove_if_exists(&path);
            continue;
        };
        let is_self = mine.as_deref() == Some(manifest.session_id.as_str());
        let alive = is_self || alive(repo_id, &manifest.session_id);
        let committed = shared.committed.contains_key(&manifest.session_id);
        let terminal = committed
            || matches!(
                manifest.state,
                SessionState::Finished | SessionState::Done | SessionState::Failed
            );
        if !alive && terminal && now.saturating_sub(manifest.updated_unix) > RETENTION_SECS {
            let _ = crate::core::persist::remove_if_exists(&path);
            let _ = crate::core::persist::remove_if_exists(&liveness_lock_path(
                repo_id,
                &manifest.session_id,
            ));
            continue;
        }
        let overlapping: Vec<String> = shared
            .overlaps
            .iter()
            .filter(|o| o.first == manifest.session_id || o.second == manifest.session_id)
            .map(|o| o.path.clone())
            .collect();
        out.push(SessionView {
            manifest,
            alive,
            is_self,
            committed,
            overlapping,
        });
    }
    out.sort_by(|a, b| {
        b.manifest
            .updated_unix
            .cmp(&a.manifest.updated_unix)
            .then_with(|| a.manifest.session_id.cmp(&b.manifest.session_id))
    });
    out
}

/// One line for startup: are other windows working in this repository right now?
///
/// `None` when this is the only session, which is the common case and earns no noise. Abandoned
/// sessions ARE counted: an unreviewed pile of changes from a window that crashed is precisely what
/// a new window needs to hear about before it starts editing the same files.
pub fn peers_banner() -> Option<String> {
    let peers: Vec<SessionView> = list().into_iter().filter(|v| !v.is_self).collect();
    if peers.is_empty() {
        return None;
    }
    let live = peers.iter().filter(|v| v.alive).count();
    let abandoned = peers
        .iter()
        .filter(|v| v.effective_state() == "abandoned")
        .count();
    let pending = peers
        .iter()
        .filter(|v| !v.committed && !v.manifest.files.is_empty())
        .count();
    let mut bits = Vec::new();
    if live > 0 {
        bits.push(format!("{live} running"));
    }
    if abandoned > 0 {
        bits.push(format!("{abandoned} abandoned"));
    }
    if pending > 0 {
        bits.push(format!("{pending} with uncommitted changes"));
    }
    if bits.is_empty() {
        bits.push(format!("{} known", peers.len()));
    }
    Some(format!(
        "⚑ {} other aizen session(s) in this repo — {} · /team status",
        peers.len(),
        bits.join(", ")
    ))
}

/// Resolve a user-typed session reference: exact id, unique id suffix/prefix, `self`, or a 1-based
/// row index as shown by `/team status`.
pub fn resolve(reference: &str) -> Result<SessionView> {
    let sessions = list();
    if sessions.is_empty() {
        bail!("no aizen sessions are registered in this repository yet");
    }
    let needle = reference.trim();
    if needle.is_empty() {
        bail!("name a session (see /team status)");
    }
    if needle.eq_ignore_ascii_case("self") || needle == "." {
        return sessions
            .into_iter()
            .find(|s| s.is_self)
            .context("this session is not registered");
    }
    if let Ok(index) = needle.parse::<usize>() {
        if index >= 1 && index <= sessions.len() {
            return Ok(sessions.into_iter().nth(index - 1).unwrap());
        }
    }
    if let Some(exact) = sessions.iter().find(|s| s.manifest.session_id == needle) {
        return Ok(exact.clone());
    }
    let matches: Vec<&SessionView> = sessions
        .iter()
        .filter(|s| {
            s.manifest.session_id.starts_with(needle) || s.manifest.session_id.ends_with(needle)
        })
        .collect();
    match matches.len() {
        1 => Ok(matches[0].clone()),
        0 => bail!("no session matches '{needle}' (see /team status)"),
        n => bail!("'{needle}' matches {n} sessions — use the full id from /team status"),
    }
}

/// Files this session changed, deduplicated and sorted.
pub fn session_paths(view: &SessionView) -> Vec<String> {
    let mut paths: Vec<String> = view.manifest.files.iter().map(|f| f.path.clone()).collect();
    paths.sort();
    paths.dedup();
    paths
}

/// Per-session diff: one report per pre-edit checkpoint this session measured from.
///
/// Grouping by base is what keeps the answer honest once another window has overwritten the same
/// file — each group is diffed from the tree that holds THIS session's pre-image.
pub fn session_diff(view: &SessionView, patch: bool) -> Result<Vec<DiffReport>> {
    if view.manifest.files.is_empty() {
        return Ok(Vec::new());
    }
    let here = identity()?;
    if view.manifest.worktree_id != here.worktree_id {
        bail!(
            "session {} works in a different worktree ({}) — run /team diff from there, or use \
             `aizen work list` to see it",
            view.manifest.session_id,
            view.manifest.root
        );
    }
    let mut by_base: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    for f in &view.manifest.files {
        by_base.entry(f.base).or_default().push(f.path.clone());
    }
    let patch_limit = if patch { Some(200_000) } else { None };
    let mut reports = Vec::new();
    for (base, paths) in by_base {
        let report = timemachine::diff(
            &DiffSide::Checkpoint(base),
            &DiffSide::Working,
            &paths,
            patch_limit,
        )
        .with_context(|| format!("diffing session changes from checkpoint #{base}"))?;
        if !report.is_empty() {
            reports.push(report);
        }
    }
    Ok(reports)
}

/// Every recorded overlap in this repository.
pub fn overlaps() -> Vec<Overlap> {
    current_repo_id()
        .map(|id| load_shared(&id).overlaps)
        .unwrap_or_default()
}

/// Who else — if anyone — most recently claimed `rel_path`, provided that session is still live and
/// is not us. `None` when the path is unclaimed, ours, or owned by a session that has already gone.
///
/// This is the pre-write half of overlap handling. [`claim_paths`] records an overlap AFTER a turn
/// has written, which is right for reporting but useless for prevention: a whole-file overwrite has
/// already destroyed the other window's work by then. A tool about to blow away a file's contents
/// asks this first, and refuses when the answer is another live session (see
/// [`crate::core::read_ledger`]).
///
/// Deliberately cheap and lock-free: a stale read here can only cost a spurious refusal that names
/// the session to check, never a silent loss. Claims from DEAD sessions are ignored — an abandoned
/// window's leftovers must not wedge the tree forever.
pub fn live_peer_claim(rel_path: &str) -> Option<String> {
    let repo_id = current_repo_id()?;
    let mine = current_session_id();
    let claim = load_shared(&repo_id).owners.remove(rel_path)?;
    if mine.as_deref() == Some(claim.session_id.as_str()) {
        return None;
    }
    alive(&repo_id, &claim.session_id).then_some(claim.session_id)
}

/// [`live_peer_claim`] for an absolute path. `None` when the path lies outside this session's
/// worktree (no claim could name it) or when no live peer holds it.
pub fn live_peer_claim_at(abs: &Path) -> Option<String> {
    live_peer_claim(&relative_to_worktree(abs)?)
}

/// Repository-relative form of `abs`, as git would name it, for claim lookups. `None` when the path
/// is outside this session's worktree — a claim on it could not be meaningful.
pub fn relative_to_worktree(abs: &Path) -> Option<String> {
    let guard = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    let root = PathBuf::from(&guard.as_ref()?.manifest.root);
    let canon = abs.canonicalize().unwrap_or_else(|_| abs.to_path_buf());
    let rel = canon.strip_prefix(&root).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    (!s.is_empty()).then_some(s)
}

/// Current path claims, newest first.
pub fn claims() -> Vec<(String, Claim)> {
    let Some(repo_id) = current_repo_id() else {
        return Vec::new();
    };
    let mut v: Vec<(String, Claim)> = load_shared(&repo_id).owners.into_iter().collect();
    v.sort_by(|a, b| b.1.unix.cmp(&a.1.unix).then_with(|| a.0.cmp(&b.0)));
    v
}

// ---------------------------------------------------------------------------------------------
// Splitting one file that two sessions both edited
// ---------------------------------------------------------------------------------------------

/// One session's reconstructed version of a file another session also changed.
#[derive(Debug, Clone)]
pub struct SplitFile {
    pub path: String,
    /// Reconstructed content, or `None` when this session's work removed the file.
    pub content: Option<Vec<u8>>,
    /// Git file mode to stage it with.
    pub mode: String,
    /// Regions where this session's edit and the peer's cannot both be kept. Non-zero means the
    /// reconstruction contains conflict markers and MUST NOT be staged.
    pub conflicts: u32,
    /// How many of this session's turns had to be replayed.
    pub replayed: usize,
    /// Set when this file cannot be separated at all; the reason is written for the user.
    pub unavailable: Option<String>,
}

impl SplitFile {
    /// Safe to stage: a clean reconstruction with no conflicts.
    pub fn usable(&self) -> bool {
        self.unavailable.is_none() && self.conflicts == 0
    }
}

/// Checkpoint ids that START a turn, for every session in this worktree, ascending.
///
/// The end of one session's turn is the next turn to begin anywhere in the worktree, so this is the
/// set of interval boundaries. Manual checkpoints (`aizen time save`) are deliberately excluded: they
/// do not end a turn, and treating them as boundaries would truncate a turn's interval and silently
/// drop the rest of that turn's work from the reconstruction.
///
/// `base_checkpoint` of a session mid-turn is included — that turn's writes are in flight and must
/// bound the previous interval rather than being swept into it.
fn turn_boundaries(repo_id: &str, worktree_id: &str, existing: &BTreeSet<u32>) -> Vec<u32> {
    let mut ids: Vec<u32> = list_in(repo_id)
        .into_iter()
        .filter(|v| v.manifest.worktree_id == worktree_id)
        .flat_map(|v| {
            let mut per: Vec<u32> = v
                .manifest
                .files
                .iter()
                .flat_map(|f| {
                    if f.bases.is_empty() {
                        vec![f.base]
                    } else {
                        f.bases.clone()
                    }
                })
                .collect();
            per.extend(v.manifest.base_checkpoint);
            per
        })
        .filter(|id| existing.contains(id))
        .collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// Tree of the last commit, or `None` in a repository with no commits yet.
fn head_tree(root: &Path) -> Option<String> {
    git_ok(
        root,
        &[
            "rev-parse".into(),
            "-q".into(),
            "--verify".into(),
            "HEAD^{tree}".into(),
        ],
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty())
}

/// Scratch file inside the git directory.
///
/// NOT in the working tree: a temp file there would be picked up by another session's checkpoint
/// (`add -A`) and land in its snapshot. The git directory is excluded from all of that.
fn scratch_path(root: &Path, tag: &str) -> Result<PathBuf> {
    let dir = git_ok(root, &["rev-parse".into(), "--absolute-git-dir".into()])?
        .trim()
        .to_string();
    Ok(PathBuf::from(dir).join(format!(
        "aizen-coop-{tag}-{}-{}",
        std::process::id(),
        crate::core::persist::unique_sequence()
    )))
}

/// Three-way merge of raw bytes, returning the merged content and the conflict count.
///
/// Done through files rather than pipes so content that is not valid UTF-8 survives: the bounded
/// runner decodes stdout lossily, which would corrupt a binary or latin-1 file on the way through.
/// `git merge-file` rewrites `ours` in place, so the result is read back from disk byte for byte.
fn merge_three_way(
    root: &Path,
    ours: &[u8],
    base: &[u8],
    theirs: &[u8],
    labels: (&str, &str, &str),
) -> Result<(Vec<u8>, u32)> {
    let o = scratch_path(root, "ours")?;
    let b = scratch_path(root, "base")?;
    let t = scratch_path(root, "theirs")?;
    let write_all = || -> std::io::Result<()> {
        fs::write(&o, ours)?;
        fs::write(&b, base)?;
        fs::write(&t, theirs)
    };
    write_all().context("writing scratch files for the three-way merge")?;
    let args: Vec<String> = vec![
        "merge-file".into(),
        "-q".into(),
        "-L".into(),
        labels.0.to_string(),
        "-L".into(),
        labels.1.to_string(),
        "-L".into(),
        labels.2.to_string(),
        o.display().to_string(),
        b.display().to_string(),
        t.display().to_string(),
    ];
    let code = git_code(root, &args);
    let merged = fs::read(&o);
    for p in [&o, &b, &t] {
        let _ = fs::remove_file(p);
    }
    let merged = merged.context("reading back the merged result")?;
    match code {
        // `merge-file` reports the conflict count as its exit status; anything from the error range
        // means it could not merge at all, and a "0 conflicts" reading there would be a lie.
        Some(c) if (0..128).contains(&c) => Ok((merged, c as u32)),
        other => bail!(
            "git merge-file could not reconstruct this file (exit {})",
            other
                .map(|c| c.to_string())
                .unwrap_or_else(|| "killed".into())
        ),
    }
}

/// Reconstruct `path` as it would look with ONLY `view`'s work applied.
///
/// Why the pre-image is the wrong starting point: if the peer edited the file BEFORE this session
/// first touched it, this session's pre-edit checkpoint already contains the peer's work, so diffing
/// from it would carry that work along — exactly what a split is supposed to remove. The base is
/// therefore the last commit, and only this session's own turns are replayed onto it.
///
/// Each turn is recoverable because writes are serialized: the workspace writer lease is held for a
/// whole turn, so between one turn's pre-edit checkpoint and the next turn's there is the work of
/// exactly one session. Replaying those intervals — and no others — onto the committed base yields
/// this session's version, with the peer's edits to the same file left out.
///
/// Reads only. The working tree is never touched, so the peer keeps editing throughout.
pub fn split_shared(view: &SessionView, path: &str) -> Result<SplitFile> {
    let here = identity()?;
    if view.manifest.worktree_id != here.worktree_id {
        bail!(
            "session {} works in a different worktree ({})",
            view.manifest.session_id,
            view.manifest.root
        );
    }
    let entry = view
        .manifest
        .files
        .iter()
        .find(|f| f.path == path)
        .with_context(|| format!("session {} did not change {path}", view.manifest.session_id))?;
    let mut out = SplitFile {
        path: path.to_string(),
        content: None,
        mode: String::new(),
        conflicts: 0,
        replayed: 0,
        unavailable: None,
    };

    let mut mine: Vec<u32> = if entry.bases.is_empty() {
        vec![entry.base]
    } else {
        entry.bases.clone()
    };
    mine.sort_unstable();
    mine.dedup();

    let existing: BTreeSet<u32> = timemachine::checkpoint_ids()
        .context("listing checkpoints")?
        .into_iter()
        .collect();
    let pruned: Vec<u32> = mine
        .iter()
        .copied()
        .filter(|i| !existing.contains(i))
        .collect();
    if !pruned.is_empty() {
        // Without the pre-image of a turn there is no way to know what that turn changed, and
        // guessing would hand the user a file that silently drops work.
        out.unavailable = Some(format!(
            "checkpoint(s) {} for this session's edits to {path} have been pruned, so its turns \
             cannot be replayed",
            pruned
                .iter()
                .map(|i| format!("#{i}"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        return Ok(out);
    }

    let root = PathBuf::from(&view.manifest.root);
    let bounds = turn_boundaries(
        &view.manifest.repo_id,
        &view.manifest.worktree_id,
        &existing,
    );
    let working =
        timemachine::resolve_tree(&DiffSide::Working).context("hashing the working tree")?;

    let base_blob = match head_tree(&root) {
        Some(tree) => timemachine::blob_in_tree(&tree, path)
            .with_context(|| format!("reading the committed version of {path}"))?,
        None => None,
    };
    let mut acc: Option<Vec<u8>> = base_blob.as_ref().map(|b| b.bytes.clone());
    if let Some(b) = &base_blob {
        out.mode = b.mode.clone();
    }

    let sid = view.manifest.session_id.clone();
    for start in &mine {
        let start_tree = timemachine::resolve_tree(&DiffSide::Checkpoint(*start))
            .with_context(|| format!("resolving checkpoint #{start}"))?;
        let before = timemachine::blob_in_tree(&start_tree, path)?;
        let end_tree = match bounds.iter().copied().find(|b| b > start) {
            Some(end) => timemachine::resolve_tree(&DiffSide::Checkpoint(end))
                .with_context(|| format!("resolving checkpoint #{end}"))?,
            // No later turn anywhere: this session's work is the newest thing in the tree.
            None => working.clone(),
        };
        let after = timemachine::blob_in_tree(&end_tree, path)?;

        if before.as_ref().map(|b| &b.oid) == after.as_ref().map(|a| &a.oid) {
            continue;
        }
        out.replayed += 1;
        if let Some(a) = &after {
            out.mode = a.mode.clone();
        }
        match (&before, &after) {
            // The turn removed the file: nothing later can be merged onto absence.
            (_, None) => acc = None,
            (None, Some(a)) => match &acc {
                None => acc = Some(a.bytes.clone()),
                Some(cur) => {
                    let (merged, c) = merge_three_way(
                        &root,
                        cur,
                        &[],
                        &a.bytes,
                        (&sid, "committed", "this session"),
                    )?;
                    out.conflicts += c;
                    acc = Some(merged);
                }
            },
            (Some(bf), Some(af)) => {
                let Some(cur) = &acc else {
                    // The file exists only because the OTHER session created it. There is no
                    // committed base to replay onto, and starting from the peer's creation would
                    // commit the peer's work under this session's name.
                    out.unavailable = Some(format!(
                        "{path} does not exist in the last commit and was created by another \
                         session, so this session's changes to it cannot be separated"
                    ));
                    return Ok(out);
                };
                let (merged, c) = merge_three_way(
                    &root,
                    cur,
                    &bf.bytes,
                    &af.bytes,
                    (&sid, "before this turn", "after this turn"),
                )?;
                out.conflicts += c;
                acc = Some(merged);
            }
        }
    }
    out.content = acc;
    if out.mode.is_empty() {
        out.mode = "100644".to_string();
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// Coordinator: commit one session's work
// ---------------------------------------------------------------------------------------------

/// What a commit-by-session would do, computed before anything is staged.
#[derive(Debug, Clone)]
pub struct CommitPlan {
    pub session_id: String,
    pub paths: Vec<String>,
    /// Paths this session shares with another one: staging them carries the other session's
    /// changes to the same file along, because git tracks content and not authorship.
    pub shared_paths: Vec<String>,
    /// Reasons this plan is not safe to run without `--force`.
    pub blockers: Vec<String>,
    pub root: PathBuf,
}

/// Build the commit plan for one session. Read-only: nothing is staged here.
pub fn plan_commit(view: &SessionView) -> Result<CommitPlan> {
    let here = identity()?;
    let mut blockers = Vec::new();
    if view.manifest.worktree_id != here.worktree_id {
        blockers.push(format!(
            "session works in another worktree ({}) — commit it from there",
            view.manifest.root
        ));
    }
    if view.committed {
        blockers.push("this session's changes were already committed".to_string());
    }
    if !view.ready_to_commit() {
        blockers.push(format!(
            "session is still {} — wait for it, mark it with /team done, or pass --force",
            view.effective_state()
        ));
    }
    let paths = session_paths(view);
    if paths.is_empty() {
        blockers.push("session has no recorded file changes".to_string());
    }
    if view.manifest.truncated_files > 0 {
        blockers.push(format!(
            "{} path(s) exceeded the tracking ceiling and are NOT in this plan",
            view.manifest.truncated_files
        ));
    }
    let shared_paths: Vec<String> = paths
        .iter()
        .filter(|p| view.overlapping.contains(p))
        .cloned()
        .collect();
    Ok(CommitPlan {
        session_id: view.manifest.session_id.clone(),
        paths,
        shared_paths,
        blockers,
        root: PathBuf::from(&view.manifest.root),
    })
}

/// Stage one reconstructed file, index-only.
///
/// The working tree is deliberately NOT written: it holds the union of both sessions' work, the peer
/// is still editing it, and overwriting it with this session's version alone would destroy the peer's
/// live changes — the precise failure this whole module exists to prevent. Git allows an index entry
/// to differ from the file on disk, so the commit can hold one session's version while the other
/// session keeps working from the union.
///
/// The bytes came out of existing blobs, i.e. already in clean-filtered form, so `--no-filters`
/// stops a `.gitattributes` clean driver from being applied a second time.
fn stage_split(root: &Path, split: &SplitFile) -> Result<()> {
    let Some(bytes) = &split.content else {
        // This session's work removed the file.
        return git_ok(
            root,
            &[
                "update-index".into(),
                "--force-remove".into(),
                "--".into(),
                split.path.clone(),
            ],
        )
        .map(|_| ());
    };
    let scratch = scratch_path(root, "blob")?;
    fs::write(&scratch, bytes)
        .with_context(|| format!("writing the reconstructed {}", split.path))?;
    let hashed = git_ok(
        root,
        &[
            "hash-object".into(),
            "-w".into(),
            "--no-filters".into(),
            "--".into(),
            scratch.display().to_string(),
        ],
    );
    let _ = fs::remove_file(&scratch);
    let oid = hashed
        .with_context(|| format!("storing the reconstructed {}", split.path))?
        .trim()
        .to_string();
    let mode = if split.mode.is_empty() {
        "100644"
    } else {
        split.mode.as_str()
    };
    git_ok(
        root,
        &[
            "update-index".into(),
            "--add".into(),
            "--cacheinfo".into(),
            format!("{mode},{oid},{}", split.path),
        ],
    )
    .map(|_| ())
}

/// What a staged review consists of: the stat a human reads, and the exact identity of the index
/// they read it from.
#[derive(Debug, Clone)]
pub struct Staged {
    /// `git diff --cached --stat`, for display.
    pub stat: String,
    /// Shared paths that were separated: the index holds this session's version, not the union on
    /// disk. Worth telling the user, because the commit will not match the working tree for these.
    pub separated: Vec<String>,
    /// Tree object of the index at review time. [`commit_staged`] refuses unless the index still
    /// hashes to this, so what gets committed is what was approved and nothing else.
    pub tree: String,
}

/// Take the workspace lease for one short coordinator step.
///
/// The reason a coordinator needs it at all: `git add` reads the working tree, and a peer window
/// mid-turn is writing to that same tree. Stage without the lease and the index can capture half of
/// another session's in-flight edit. Reentrancy is handled in [`crate::core::workspace_txn`], so this
/// is also correct when the coordinator window is itself inside a turn.
///
/// Deliberately NOT held across the approval prompt: blocking every other window for as long as a
/// human takes to decide is worse than the race it would close, and the tree digest below closes that
/// race without holding anything.
fn coordinator_lease(
    root: &Path,
    what: &str,
) -> Option<crate::core::workspace_txn::WorkspaceWriterLease> {
    crate::core::workspace_txn::WorkspaceWriterLease::acquire(
        root,
        Duration::from_secs(15),
        None,
        what,
    )
    .ok()
}

/// Hash of the current index, as a tree object. This is the review token: two indexes with the same
/// tree are byte-for-byte the same staged content.
fn index_tree(root: &Path) -> Result<String> {
    git_ok(root, &["write-tree".into()])
        .map(|s| s.trim().to_string())
        .context("hashing the staged index")
}

/// Stage exactly the plan's paths and return the review plus the identity of what was staged.
///
/// Deliberately additive: staging leaves the working tree untouched, so another window's in-flight
/// edits cannot be lost by a review that is later abandoned.
///
/// Files this session shares with another are not staged from disk — disk holds the union of both
/// sessions' work. Each is reconstructed by [`split_shared`] and staged index-only, so the commit
/// carries one session's version while the peer keeps editing the union. A file that cannot be
/// separated cleanly aborts the whole stage rather than landing a commit with someone else's work
/// (or conflict markers) in it.
pub fn stage_plan(plan: &CommitPlan) -> Result<Staged> {
    if plan.paths.is_empty() {
        bail!("nothing to stage");
    }
    let _lease = coordinator_lease(&plan.root, "coop stage");
    let mut args: Vec<String> = vec!["add".into(), "--".into()];
    args.extend(plan.paths.iter().cloned());
    // `git add` of a deleted path stages the deletion; `--` guards paths that look like flags.
    git_ok(&plan.root, &args).context("staging this session's files")?;

    let mut separated = Vec::new();
    if !plan.shared_paths.is_empty() {
        let view = resolve(&plan.session_id)?;
        let mut refused = Vec::new();
        for path in &plan.shared_paths {
            match split_shared(&view, path) {
                Ok(split) if split.usable() => {
                    stage_split(&plan.root, &split)?;
                    separated.push(path.clone());
                }
                Ok(split) => refused.push(match &split.unavailable {
                    Some(why) => format!("{path}: {why}"),
                    None => format!(
                        "{path}: this session's edits and another session's overlap in {} region(s) \
                         and cannot be separated automatically",
                        split.conflicts
                    ),
                }),
                Err(e) => refused.push(format!("{path}: {e:#}")),
            }
        }
        if !refused.is_empty() {
            // Roll the index back: a partially separated stage is the one state where a later
            // `--force` would commit a mix nobody chose.
            let _ = unstage_plan(plan);
            bail!(
                "cannot commit this session alone — {} shared file(s) could not be separated:\n  {}\n\
                 Resolve the overlap in the working tree (or commit both sessions together) and \
                 re-run.",
                refused.len(),
                refused.join("\n  ")
            );
        }
    }

    let stat = git_ok(
        &plan.root,
        &["diff".into(), "--cached".into(), "--stat".into()],
    )?;
    let tree = index_tree(&plan.root)?;
    Ok(Staged {
        stat,
        separated,
        tree,
    })
}

/// Commit what `stage_plan` staged. Caller is responsible for approval — this is the irreversible
/// step and it is never reached without one.
///
/// `staged` is the review the user approved. If the index no longer hashes to it, something changed
/// between review and approval — another window staged, or a hook ran — and committing would land
/// content nobody looked at. That is refused rather than reconciled: the coordinator re-runs and
/// reviews the new state.
pub fn commit_staged(plan: &CommitPlan, message: &str, staged: &Staged) -> Result<String> {
    let _lease = coordinator_lease(&plan.root, "coop commit");
    let now = index_tree(&plan.root)?;
    if now != staged.tree {
        bail!(
            "the staged changes are not the ones that were reviewed (index moved from {} to {}); \
             nothing was committed. Re-run /team commit to review the current state",
            &staged.tree[..staged.tree.len().min(8)],
            &now[..now.len().min(8)]
        );
    }
    let staged_names = git_ok(
        &plan.root,
        &["diff".into(), "--cached".into(), "--name-only".into()],
    )?;
    if staged_names.trim().is_empty() {
        bail!("nothing is staged — run the plan again");
    }
    let out = git_ok(
        &plan.root,
        &["commit".into(), "-m".into(), message.to_string()],
    )?;
    if let Some(repo_id) = current_repo_id() {
        let sid = plan.session_id.clone();
        let paths = plan.paths.clone();
        let _ = with_shared(&repo_id, |shared| {
            shared.committed.insert(sid.clone(), now_unix());
            // The claim is spent: a later session editing the same file is not overlapping with
            // work that has already landed.
            for p in &paths {
                if shared.owners.get(p).map(|c| c.session_id.as_str()) == Some(sid.as_str()) {
                    shared.owners.remove(p);
                }
            }
            shared
                .overlaps
                .retain(|o| o.first != sid && o.second != sid);
        });
    }
    Ok(out)
}

/// Unstage what a plan staged, for an abandoned review. `git restore --staged` touches the index
/// only — the working tree, and therefore every other window's live edits, is untouched.
pub fn unstage_plan(plan: &CommitPlan) -> Result<()> {
    if plan.paths.is_empty() {
        return Ok(());
    }
    let _lease = coordinator_lease(&plan.root, "coop unstage");
    let mut args: Vec<String> = vec!["restore".into(), "--staged".into(), "--".into()];
    args.extend(plan.paths.iter().cloned());
    git_ok(&plan.root, &args).map(|_| ())
}

// ---------------------------------------------------------------------------------------------
// Isolated worktree mode
// ---------------------------------------------------------------------------------------------

/// One linked worktree created for a session.
#[derive(Debug, Clone)]
pub struct WorkTree {
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
    /// Uncommitted changes present.
    pub dirty: bool,
    /// Commits on its branch that the base branch does not have.
    pub ahead: u64,
    /// Sessions currently registered in this worktree.
    pub sessions: usize,
}

fn worktrees_root(repo_id: &str) -> PathBuf {
    crate::core::config::aizen_home()
        .join("worktrees")
        .join(repo_id)
}

fn valid_worktree_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.');
    if !ok {
        bail!("worktree name must be 1-64 chars of letters, digits, '-', '_' or '.'");
    }
    Ok(())
}

/// Create a linked worktree plus its branch, and return where it landed.
pub fn work_new(name: &str) -> Result<WorkTree> {
    valid_worktree_name(name)?;
    let id = identity()?;
    if id.canonical_git_dir.is_none() {
        bail!("isolated worktrees need a git repository (run `git init` first)");
    }
    let dir = worktrees_root(&id.repo_id).join(name);
    if dir.exists() {
        bail!("{} already exists", dir.display());
    }
    fs::create_dir_all(worktrees_root(&id.repo_id))
        .with_context(|| format!("creating {}", worktrees_root(&id.repo_id).display()))?;
    let branch = format!("aizen/{name}");
    git_ok(
        &id.canonical_root,
        &[
            "worktree".into(),
            "add".into(),
            "-b".into(),
            branch.clone(),
            dir.display().to_string(),
        ],
    )
    .with_context(|| format!("creating worktree {name}"))?;
    Ok(WorkTree {
        name: name.to_string(),
        path: dir,
        branch,
        dirty: false,
        ahead: 0,
        sessions: 0,
    })
}

/// Every aizen-created worktree of this repository, with the state that decides whether removing it
/// would lose work.
pub fn work_list() -> Result<Vec<WorkTree>> {
    let id = identity()?;
    let root = worktrees_root(&id.repo_id);
    let sessions = list_in(&id.repo_id);
    let Ok(rd) = fs::read_dir(&root) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        let branch = current_branch(&path).unwrap_or_else(|| format!("aizen/{name}"));
        let dirty = git_ok(&path, &["status".into(), "--porcelain".into()])
            .map(|s| !s.trim().is_empty())
            .unwrap_or(true);
        let ahead = git_ok(
            &path,
            &[
                "rev-list".into(),
                "--count".into(),
                format!("{branch}@{{upstream}}..{branch}"),
            ],
        )
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .or_else(|| {
            // No upstream: compare against the repository's current HEAD branch instead, which is
            // what "has this worktree got commits nobody else has?" means locally.
            let base = current_branch(&id.canonical_root)?;
            git_ok(
                &path,
                &[
                    "rev-list".into(),
                    "--count".into(),
                    format!("{base}..{branch}"),
                ],
            )
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
        })
        .unwrap_or(0);
        let canon = fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        let session_count = sessions
            .iter()
            .filter(|s| {
                let sroot = fs::canonicalize(&s.manifest.root)
                    .unwrap_or_else(|_| PathBuf::from(&s.manifest.root));
                sroot == canon && s.alive
            })
            .count();
        out.push(WorkTree {
            name,
            path,
            branch,
            dirty,
            ahead,
            sessions: session_count,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Why removing a worktree would lose work. Empty means it is safe to remove.
pub fn work_remove_blockers(wt: &WorkTree) -> Vec<String> {
    let mut blockers = Vec::new();
    if wt.dirty {
        blockers.push("has uncommitted changes".to_string());
    }
    if wt.ahead > 0 {
        blockers.push(format!(
            "has {} commit(s) not present on the base branch",
            wt.ahead
        ));
    }
    if wt.sessions > 0 {
        blockers.push(format!(
            "{} aizen session(s) still running in it",
            wt.sessions
        ));
    }
    blockers
}

/// Remove a linked worktree. Refuses while [`work_remove_blockers`] is non-empty unless `force`,
/// and never rewrites or discards content in the tree it removes: the branch is left in place, so
/// any commits made there remain reachable by name.
pub fn work_remove(name: &str, force: bool) -> Result<String> {
    valid_worktree_name(name)?;
    let id = identity()?;
    let wt = work_list()?
        .into_iter()
        .find(|w| w.name == name)
        .with_context(|| format!("no aizen worktree named '{name}'"))?;
    let blockers = work_remove_blockers(&wt);
    if !blockers.is_empty() && !force {
        bail!(
            "refusing to remove '{name}': {}. Nothing was changed. Re-run with --force only if \
             that work is expendable (the branch {} is kept either way)",
            blockers.join("; "),
            wt.branch
        );
    }
    let mut args: Vec<String> = vec![
        "worktree".into(),
        "remove".into(),
        wt.path.display().to_string(),
    ];
    if force {
        args.push("--force".into());
    }
    git_ok(&id.canonical_root, &args).with_context(|| format!("removing worktree {name}"))?;
    Ok(format!(
        "removed worktree {} (branch {} kept)",
        wt.path.display(),
        wt.branch
    ))
}

// ---------------------------------------------------------------------------------------------
// Agent-facing tool
// ---------------------------------------------------------------------------------------------

/// Read-only view of the other windows, for the model.
///
/// Deliberately read-only: deciding what to commit, and committing it, stays with the human at
/// `/team commit`. What the model needs is the ability to NOTICE that another window owns the file
/// it was about to rewrite — that is a coordination fact it cannot otherwise get.
pub struct TeamStatus;

impl crate::agent::tools::Tool for TeamStatus {
    fn name(&self) -> &str {
        "team_status"
    }

    fn description(&self) -> &str {
        "List the other aizen sessions working in this repository right now: their task, state \
         (working/idle/done/abandoned), and which files each has changed. Use it BEFORE editing a \
         file that another session may own, and when the user asks what the other windows are \
         doing or whether their work is ready. Read-only — reviewing and committing another \
         session's work is the user's call via `/team commit`."
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "files": {
                    "type": "boolean",
                    "description": "include each session's changed file list (default false)"
                }
            },
            "additionalProperties": false
        })
    }

    fn execute(&self, args: &serde_json::Value) -> Result<String> {
        let want_files = args.get("files").and_then(|v| v.as_bool()).unwrap_or(false);
        let sessions = list();
        if sessions.is_empty() {
            return Ok("no aizen sessions are registered in this repository".to_string());
        }
        let mut out = String::new();
        for v in &sessions {
            let m = &v.manifest;
            out.push_str(&format!(
                "{}{} · {} · turns={} · files={}{}\n  task: {}\n",
                m.session_id,
                if v.is_self { " (this session)" } else { "" },
                v.effective_state(),
                m.turns,
                m.files.len(),
                if v.overlapping.is_empty() {
                    String::new()
                } else {
                    format!(" · shares {} file(s)", v.overlapping.len())
                },
                if m.task.is_empty() {
                    "(unstated)"
                } else {
                    &m.task
                },
            ));
            if let Some(reason) = &m.degraded {
                out.push_str(&format!("  no per-session diff: {reason}\n"));
            }
            if want_files {
                for f in &m.files {
                    out.push_str(&format!("  {} {}\n", f.status, f.path));
                }
                if m.truncated_files > 0 {
                    out.push_str(&format!(
                        "  (+{} path(s) beyond the tracking ceiling)\n",
                        m.truncated_files
                    ));
                }
            }
        }
        let overlaps = overlaps();
        if !overlaps.is_empty() {
            out.push_str(
                "\nfiles changed by more than one session (git cannot split these by line):\n",
            );
            for o in overlaps.iter().take(20) {
                out.push_str(&format!("  {} — {} then {}\n", o.path, o.first, o.second));
            }
        }
        Ok(out.trim_end().to_string())
    }
}

// ---------------------------------------------------------------------------------------------
// git plumbing
// ---------------------------------------------------------------------------------------------

/// Run git in `root`, bounded. `output_bounded` rather than `Command::output()`: a wedged git with a
/// live grandchild holding the pipe would otherwise hang the REPL thread for good.
fn git_ok(root: &Path, args: &[String]) -> Result<String> {
    let mut cmd = crate::core::gitx::command()?;
    cmd.current_dir(root);
    cmd.args(args);
    let out = crate::core::proctree::output_bounded(&mut cmd, GIT_TIMEOUT, GIT_DRAIN_GRACE)
        .context("running git")?;
    if out.timed_out {
        bail!(
            "git {} timed out",
            args.first().cloned().unwrap_or_default()
        );
    }
    if out.code != Some(0) {
        let stderr = out.stderr.trim();
        let detail = if stderr.is_empty() {
            out.stdout.trim()
        } else {
            stderr
        };
        bail!("git {} failed: {}", args.join(" "), detail);
    }
    Ok(out.stdout)
}

/// Run git in `root` and return its exit code.
///
/// For the commands whose exit STATUS is the answer rather than a failure: `merge-file` reports the
/// number of conflicts that way, so treating non-zero as an error would throw the result away.
fn git_code(root: &Path, args: &[String]) -> Option<i32> {
    let mut cmd = crate::core::gitx::command().ok()?;
    cmd.current_dir(root);
    cmd.args(args);
    let out = crate::core::proctree::output_bounded(&mut cmd, GIT_TIMEOUT, GIT_DRAIN_GRACE).ok()?;
    if out.timed_out {
        return None;
    }
    out.code
}

fn current_branch(root: &Path) -> Option<String> {
    git_ok(
        root,
        &["rev-parse".into(), "--abbrev-ref".into(), "HEAD".into()],
    )
    .ok()
    .map(|s| s.trim().to_string())
    .filter(|s| !s.is_empty() && s != "HEAD")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Point `aizen_home()` at a private temp tree. Holds `TEST_HOME_LOCK` because `AIZEN_HOME` is
    /// process-global, and creates the dir up front so nothing that canonicalizes it can flake.
    struct Home {
        _guard: std::sync::MutexGuard<'static, ()>,
        root: PathBuf,
    }

    impl Home {
        fn new(tag: &str) -> Self {
            let guard = crate::core::config::TEST_HOME_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let root = std::env::temp_dir().join(format!(
                "aizen-coop-{tag}-{}-{}",
                std::process::id(),
                crate::core::persist::unique_sequence()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            std::env::set_var("AIZEN_HOME", &root);
            Self {
                _guard: guard,
                root,
            }
        }
    }

    impl Drop for Home {
        fn drop(&mut self) {
            std::env::remove_var("AIZEN_HOME");
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn manifest(repo: &str, id: &str, state: SessionState) -> SessionManifest {
        SessionManifest {
            schema: SCHEMA,
            session_id: id.to_string(),
            pid: 4242,
            binary_version: env!("CARGO_PKG_VERSION").to_string(),
            repo_id: repo.to_string(),
            worktree_id: "wt".to_string(),
            root: "/tmp/x".to_string(),
            branch: Some("main".to_string()),
            session_name: None,
            task: format!("task of {id}"),
            task_pinned: true,
            state,
            turns: 1,
            base_checkpoint: None,
            files: Vec::new(),
            truncated_files: 0,
            degraded: None,
            // NOW, not a fixed epoch: `list_in` sweeps terminal manifests past `RETENTION_SECS`,
            // so a hardcoded old timestamp would make every Finished/Done/Failed fixture vanish
            // before the assertion ran. Tests that care about staleness set this explicitly.
            started_unix: now_unix(),
            updated_unix: now_unix(),
        }
    }

    #[test]
    fn a_session_whose_process_is_gone_reads_as_abandoned() {
        let _home = Home::new("liveness");
        let repo = "repo-a";
        fs::create_dir_all(sessions_dir(repo)).unwrap();

        // Alive: something still holds the liveness lock (stands in for the owning process).
        let alive_m = manifest(repo, "s-alive", SessionState::Working);
        write_manifest(&alive_m).unwrap();
        let _held = RepoTxnLock::acquire_exclusive(
            &liveness_lock_path(repo, "s-alive"),
            Duration::from_secs(1),
        )
        .unwrap();

        // Crashed: manifest says Working, but the lock file is free.
        let dead_m = manifest(repo, "s-dead", SessionState::Working);
        write_manifest(&dead_m).unwrap();
        fs::write(liveness_lock_path(repo, "s-dead"), b"").unwrap();

        // Closed cleanly: no live process expected, so it is not abandoned.
        let done_m = manifest(repo, "s-done", SessionState::Finished);
        write_manifest(&done_m).unwrap();
        fs::write(liveness_lock_path(repo, "s-done"), b"").unwrap();

        let views = list_in(repo);
        assert_eq!(views.len(), 3, "all three sessions are listed");
        let by_id = |id: &str| -> SessionView {
            views
                .iter()
                .find(|v| v.manifest.session_id == id)
                .cloned()
                .unwrap()
        };
        assert_eq!(by_id("s-alive").effective_state(), "working");
        assert!(by_id("s-alive").alive);
        assert_eq!(by_id("s-dead").effective_state(), "abandoned");
        assert!(!by_id("s-dead").alive);
        assert_eq!(by_id("s-done").effective_state(), "finished");
    }

    #[test]
    fn registry_is_repo_scoped_and_sweeps_unreadable_manifests() {
        let _home = Home::new("scope");
        fs::create_dir_all(sessions_dir("repo-a")).unwrap();
        fs::create_dir_all(sessions_dir("repo-b")).unwrap();
        write_manifest(&manifest("repo-a", "s-a", SessionState::Idle)).unwrap();
        write_manifest(&manifest("repo-b", "s-b", SessionState::Idle)).unwrap();
        let junk = sessions_dir("repo-a").join("broken.json");
        fs::write(&junk, b"{not json").unwrap();

        let a = list_in("repo-a");
        assert_eq!(a.len(), 1, "only this repository's sessions are visible");
        assert_eq!(a[0].manifest.session_id, "s-a");
        assert!(!junk.exists(), "unreadable manifest is swept");
        assert_eq!(list_in("repo-b").len(), 1, "the other repo is untouched");
    }

    #[test]
    fn finished_sessions_are_kept_until_retention_then_swept() {
        let _home = Home::new("retention");
        let repo = "repo-r";
        fs::create_dir_all(sessions_dir(repo)).unwrap();
        let mut recent = manifest(repo, "s-recent", SessionState::Finished);
        recent.updated_unix = now_unix();
        write_manifest(&recent).unwrap();
        let mut old = manifest(repo, "s-old", SessionState::Finished);
        old.updated_unix = now_unix().saturating_sub(RETENTION_SECS + 60);
        write_manifest(&old).unwrap();

        let views = list_in(repo);
        assert_eq!(
            views.len(),
            1,
            "the stale manifest is swept, the fresh one kept"
        );
        assert_eq!(views[0].manifest.session_id, "s-recent");
        assert!(
            !manifest_path(repo, "s-old").exists(),
            "a finished session past retention leaves no file behind"
        );
    }

    #[test]
    fn ledger_keeps_the_first_base_so_a_later_overwrite_cannot_narrow_the_diff() {
        let mut files = Vec::new();
        let dropped = merge_touched(&mut files, &[('M', "src/a.rs".into())], 5, 1);
        assert_eq!(dropped, 0);
        // Same file changed again two turns later, from a newer checkpoint.
        merge_touched(
            &mut files,
            &[('M', "src/a.rs".into()), ('A', "src/b.rs".into())],
            9,
            3,
        );
        assert_eq!(files.len(), 2);
        let a = files.iter().find(|f| f.path == "src/a.rs").unwrap();
        assert_eq!(
            a.base, 5,
            "pre-image stays the FIRST checkpoint this session saw"
        );
        assert_eq!(a.last_turn, 3);
        let b = files.iter().find(|f| f.path == "src/b.rs").unwrap();
        assert_eq!(
            b.base, 9,
            "a newly touched file bases on the turn that touched it"
        );
        assert_eq!(
            files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            vec!["src/a.rs", "src/b.rs"],
            "ledger stays sorted for stable rendering"
        );
    }

    #[test]
    fn ledger_reports_overflow_instead_of_silently_dropping_paths() {
        let mut files: Vec<TouchedFile> = (0..MAX_FILES)
            .map(|i| TouchedFile {
                path: format!("f{i}.rs"),
                base: 1,
                last_turn: 1,
                status: 'M',
                bases: vec![1],
            })
            .collect();
        let dropped = merge_touched(&mut files, &[('A', "overflow.rs".into())], 2, 2);
        assert_eq!(dropped, 1, "the ceiling is reported, not hidden");
        assert_eq!(files.len(), MAX_FILES);
        // An already-tracked path still updates once the ceiling is hit.
        let dropped = merge_touched(&mut files, &[('M', "f0.rs".into())], 2, 3);
        assert_eq!(dropped, 0);
        assert_eq!(
            files.iter().find(|f| f.path == "f0.rs").unwrap().last_turn,
            3
        );
    }

    #[test]
    fn second_session_on_one_file_is_warned_once_and_recorded_for_both() {
        let _home = Home::new("overlap");
        let repo = "repo-o";
        let paths = vec!["src/login.ts".to_string(), "src/token.ts".to_string()];
        let first = claim_paths(repo, "s-a", &paths).unwrap();
        assert!(first.is_empty(), "the first claim is not a conflict");

        let second = claim_paths(repo, "s-b", &["src/login.ts".to_string()]).unwrap();
        assert_eq!(second.len(), 1, "exactly one warning for the shared file");
        assert!(second[0].contains("src/login.ts"), "{:?}", second);
        assert!(
            second[0].contains("s-a"),
            "the warning names the other session"
        );

        let again = claim_paths(repo, "s-b", &["src/login.ts".to_string()]).unwrap();
        assert!(
            again.is_empty(),
            "the same overlap is not re-reported every turn"
        );

        let shared = load_shared(repo);
        assert_eq!(shared.overlaps.len(), 1);
        assert_eq!(shared.overlaps[0].first, "s-a");
        assert_eq!(shared.overlaps[0].second, "s-b");
        assert_eq!(
            shared.owners.get("src/login.ts").unwrap().session_id,
            "s-b",
            "the most recent writer owns the claim"
        );
        assert_eq!(
            shared.owners.get("src/token.ts").unwrap().session_id,
            "s-a",
            "a file only one session touched stays with it"
        );
    }

    #[test]
    fn overlaps_surface_on_both_sessions_views() {
        let _home = Home::new("overlap-view");
        let repo = "repo-ov";
        fs::create_dir_all(sessions_dir(repo)).unwrap();
        let mut a = manifest(repo, "s-a", SessionState::Finished);
        a.files = vec![TouchedFile {
            path: "src/login.ts".into(),
            base: 1,
            last_turn: 1,
            status: 'M',
            bases: vec![1],
        }];
        write_manifest(&a).unwrap();
        let b = manifest(repo, "s-b", SessionState::Finished);
        write_manifest(&b).unwrap();
        claim_paths(repo, "s-a", &["src/login.ts".to_string()]).unwrap();
        claim_paths(repo, "s-b", &["src/login.ts".to_string()]).unwrap();

        let views = list_in(repo);
        for v in &views {
            assert_eq!(
                v.overlapping,
                vec!["src/login.ts".to_string()],
                "both sides of an overlap can see it"
            );
        }
    }

    #[test]
    fn worktree_names_are_restricted_to_safe_path_segments() {
        assert!(valid_worktree_name("auth-fix").is_ok());
        assert!(valid_worktree_name("feature_1.2").is_ok());
        assert!(valid_worktree_name("").is_err());
        assert!(valid_worktree_name(".hidden").is_err());
        assert!(valid_worktree_name("../escape").is_err());
        assert!(valid_worktree_name("has space").is_err());
        assert!(valid_worktree_name(&"x".repeat(65)).is_err());
    }

    #[test]
    fn removing_a_worktree_is_refused_while_it_holds_work() {
        let dirty = WorkTree {
            name: "wt".into(),
            path: PathBuf::from("/tmp/wt"),
            branch: "aizen/wt".into(),
            dirty: true,
            ahead: 0,
            sessions: 0,
        };
        assert_eq!(work_remove_blockers(&dirty).len(), 1);
        let busy = WorkTree {
            dirty: false,
            ahead: 2,
            sessions: 1,
            ..dirty.clone()
        };
        let blockers = work_remove_blockers(&busy);
        assert_eq!(blockers.len(), 2, "{blockers:?}");
        let clean = WorkTree {
            dirty: false,
            ahead: 0,
            sessions: 0,
            ..dirty
        };
        assert!(work_remove_blockers(&clean).is_empty());
    }

    #[test]
    fn task_text_is_stripped_of_control_characters() {
        assert_eq!(safe_text("fix \u{1b}[31mauth\u{7}", 80), "fix [31mauth");
        assert_eq!(safe_text("  spaced  ", 80), "spaced");
        assert_eq!(safe_text(&"y".repeat(200), 10), "y".repeat(10));
    }

    #[test]
    fn a_running_session_is_not_committable_without_force() {
        let _home = Home::new("plan");
        let repo = "repo-p";
        fs::create_dir_all(sessions_dir(repo)).unwrap();
        let mut m = manifest(repo, "s-live", SessionState::Working);
        m.files = vec![TouchedFile {
            path: "src/a.rs".into(),
            base: 1,
            last_turn: 1,
            status: 'M',
            bases: vec![1],
        }];
        write_manifest(&m).unwrap();
        let _held = RepoTxnLock::acquire_exclusive(
            &liveness_lock_path(repo, "s-live"),
            Duration::from_secs(1),
        )
        .unwrap();
        let view = list_in(repo)
            .into_iter()
            .find(|v| v.manifest.session_id == "s-live")
            .unwrap();
        assert!(view.alive);
        assert!(!view.ready_to_commit(), "a working session is not ready");

        let mut finished = m.clone();
        finished.state = SessionState::Finished;
        write_manifest(&finished).unwrap();
        drop(_held);
        let view = list_in(repo)
            .into_iter()
            .find(|v| v.manifest.session_id == "s-live")
            .unwrap();
        assert!(
            view.ready_to_commit(),
            "a closed session is ready to commit"
        );
    }

    #[test]
    fn resolve_accepts_index_and_unique_suffix_but_refuses_ambiguity() {
        let sessions = ["1000-11", "1000-12"];
        // `resolve` reads the live registry; exercise the matching rule it uses directly so the
        // test does not depend on a discovered repository identity.
        let unique: Vec<&str> = sessions
            .iter()
            .copied()
            .filter(|s| s.ends_with("11"))
            .collect();
        assert_eq!(unique.len(), 1);
        let ambiguous: Vec<&str> = sessions
            .iter()
            .copied()
            .filter(|s| s.starts_with("1000"))
            .collect();
        assert_eq!(
            ambiguous.len(),
            2,
            "a shared prefix must not silently pick one"
        );
    }

    /// A throwaway git repository with one commit. `None` when git is unavailable, so the suite still
    /// passes on a machine without it rather than failing for the wrong reason.
    fn temp_repo(tag: &str) -> Option<PathBuf> {
        crate::core::gitx::git_exe()?;
        let root = std::env::temp_dir().join(format!(
            "aizen-coop-git-{tag}-{}-{}",
            std::process::id(),
            crate::core::persist::unique_sequence()
        ));
        fs::create_dir_all(&root).ok()?;
        let root = root.canonicalize().ok()?;
        let run = |args: &[&str]| {
            git_ok(
                &root,
                &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
            .ok()
        };
        run(&["init", "--initial-branch=main"])?;
        run(&["config", "user.email", "t@example.invalid"])?;
        run(&["config", "user.name", "t"])?;
        run(&["config", "commit.gpgsign", "false"])?;
        fs::write(root.join("a.txt"), "base\n").ok()?;
        run(&["add", "-A"])?;
        run(&["commit", "-m", "base"])?;
        Some(root)
    }

    #[test]
    fn a_commit_is_refused_when_the_index_moved_after_the_review() {
        // The window between "user reads the stat" and "user approves" is not held under any lock —
        // holding one across a human decision would freeze every other window. The tree digest is
        // what closes it: if anything staged in that window, the approved review no longer describes
        // what would land, and committing it would ship content nobody looked at.
        // Sandboxed home: `commit_staged` records the commit in `shared.json`, and that must land in
        // a temp tree rather than the developer's real `~/.aizen`.
        let _home = Home::new("treeguard");
        let Some(root) = temp_repo("treeguard") else {
            return;
        };
        fs::write(root.join("a.txt"), "session A's work\n").unwrap();
        let plan = CommitPlan {
            session_id: "s-a".into(),
            paths: vec!["a.txt".into()],
            shared_paths: Vec::new(),
            blockers: Vec::new(),
            root: root.clone(),
        };
        let review = stage_plan(&plan).expect("staging the plan");
        assert!(review.stat.contains("a.txt"), "{}", review.stat);

        // Another window stages something of its own between review and approval.
        fs::write(root.join("b.txt"), "session B's work\n").unwrap();
        git_ok(&root, &["add".into(), "--".into(), "b.txt".into()]).unwrap();

        let err = commit_staged(&plan, "should not land", &review)
            .expect_err("a moved index must refuse rather than commit the surprise");
        let msg = format!("{err:#}");
        assert!(msg.contains("not the ones that were reviewed"), "{msg}");
        // Nothing was committed: HEAD still points at the single base commit.
        let count = git_ok(&root, &["rev-list".into(), "--count".into(), "HEAD".into()]).unwrap();
        assert_eq!(count.trim(), "1", "the refused commit must not have landed");

        // Re-reviewing the current state succeeds — the guard refuses stale approvals, not progress.
        let fresh = stage_plan(&plan).expect("re-staging");
        commit_staged(&plan, "reviewed again", &fresh).expect("a current review commits");
        let count = git_ok(&root, &["rev-list".into(), "--count".into(), "HEAD".into()]).unwrap();
        assert_eq!(count.trim(), "2");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn every_turn_that_touches_a_path_is_remembered_not_just_the_first() {
        // `base` stays at the first turn (a whole-file diff measures from there), but a split has to
        // replay EVERY turn of this session. If later turns were dropped, a two-turn session would
        // silently reconstruct only its first turn's work.
        let mut files = Vec::new();
        merge_touched(&mut files, &[('M', "src/a.rs".into())], 4, 1);
        merge_touched(&mut files, &[('M', "src/a.rs".into())], 9, 2);
        merge_touched(&mut files, &[('M', "src/a.rs".into())], 9, 3);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].base, 4, "the first turn remains the diff base");
        assert_eq!(files[0].last_turn, 3);
        assert_eq!(
            files[0].bases,
            vec![4, 9],
            "each distinct turn interval is recorded exactly once"
        );
    }

    #[test]
    fn turn_boundaries_span_every_session_and_skip_pruned_checkpoints() {
        // A session's turn ends where the NEXT turn begins — anywhere in the worktree, not just in
        // the same session. Boundaries must therefore be collected across sessions, or an interval
        // would swallow a peer's turn and attribute their work to this session.
        let _home = Home::new("bounds");
        let repo = "repo-bounds";
        fs::create_dir_all(sessions_dir(repo)).unwrap();

        let mut a = manifest(repo, "s-a", SessionState::Idle);
        a.files = vec![TouchedFile {
            path: "shared.rs".into(),
            base: 2,
            last_turn: 2,
            status: 'M',
            bases: vec![2, 6],
        }];
        write_manifest(&a).unwrap();

        let mut b = manifest(repo, "s-b", SessionState::Working);
        b.files = vec![TouchedFile {
            path: "shared.rs".into(),
            base: 4,
            last_turn: 1,
            status: 'M',
            bases: vec![4],
        }];
        // Mid-turn: these writes are in flight and must bound the previous interval.
        b.base_checkpoint = Some(8);
        write_manifest(&b).unwrap();

        // Another worktree of the same repository: its checkpoints live in a different ledger.
        let mut other = manifest(repo, "s-other", SessionState::Idle);
        other.worktree_id = "wt-elsewhere".into();
        other.files = vec![TouchedFile {
            path: "shared.rs".into(),
            base: 5,
            last_turn: 1,
            status: 'M',
            bases: vec![5],
        }];
        write_manifest(&other).unwrap();

        // #6 has been pruned from the ledger; #99 was never a turn start.
        let existing: BTreeSet<u32> = [2, 4, 5, 8, 99].into_iter().collect();
        let bounds = turn_boundaries(repo, "wt", &existing);
        assert_eq!(
            bounds,
            vec![2, 4, 8],
            "both sessions' turns bound each other, the pruned and foreign ones are left out"
        );
    }

    #[test]
    fn replaying_one_turn_keeps_this_sessions_edit_and_drops_the_peers() {
        // The property the whole split rests on. Session A changed line 2; session B changed line 3
        // in the same file. Replaying A's interval onto the committed base must yield A's change and
        // NOT B's — even though every tree A was measured in already contained B's work.
        let Some(root) = temp_repo("replay") else {
            return;
        };
        let committed = "1\nA-target\n3\n4\n5\n6\n7\n8\nB-target\n10\n";
        // What A's turn saw before and after. BOTH trees already contain B's work on line 9 —
        // that is the whole difficulty: A was never measured against a tree free of B's edits.
        let before_turn = "1\nA-target\n3\n4\n5\n6\n7\n8\nB-EDITED\n10\n";
        let after_turn = "1\nA-EDITED\n3\n4\n5\n6\n7\n8\nB-EDITED\n10\n";
        let (merged, conflicts) = merge_three_way(
            &root,
            committed.as_bytes(),
            before_turn.as_bytes(),
            after_turn.as_bytes(),
            ("s-a", "before", "after"),
        )
        .expect("replaying a clean turn");
        assert_eq!(
            conflicts, 0,
            "the two sessions edited well-separated regions"
        );
        assert_eq!(
            String::from_utf8_lossy(&merged),
            "1\nA-EDITED\n3\n4\n5\n6\n7\n8\nB-target\n10\n",
            "A's edit is kept and B's is left behind, even though every tree A was measured \
             in already contained B's work"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn edits_on_adjacent_lines_are_reported_rather_than_merged() {
        // Worth pinning down, because it bounds what the feature can promise. Git's merge granularity
        // is the hunk, not the line: two sessions editing lines that fall in one hunk cannot be
        // separated, even though no single line was touched twice. That is reported as a conflict —
        // conservative in the safe direction, since the alternative is inventing a resolution.
        let Some(root) = temp_repo("adjacent") else {
            return;
        };
        let (_, conflicts) = merge_three_way(
            &root,
            b"one\ntwo\nB-EDITED\n",
            b"one\ntwo\nthree\n",
            b"one\nA-EDITED\nthree\n",
            ("s-a", "before", "after"),
        )
        .expect("merge-file runs");
        assert!(
            conflicts > 0,
            "adjacent-line edits share a hunk and must be reported, not silently resolved"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn two_sessions_editing_the_same_line_is_reported_not_guessed() {
        let Some(root) = temp_repo("collide") else {
            return;
        };
        // Both sessions rewrote line 2. There is no answer that keeps both.
        let (merged, conflicts) = merge_three_way(
            &root,
            b"one\nB's version\nthree\n",
            b"one\ntwo\nthree\n",
            b"one\nA's version\nthree\n",
            ("s-a", "before", "after"),
        )
        .expect("merge-file reports conflicts through its exit status");
        assert!(conflicts > 0, "an overlapping edit must not report success");
        let text = String::from_utf8_lossy(&merged);
        assert!(
            text.contains("<<<<<<<"),
            "conflict markers are present: {text}"
        );
        // A SplitFile in this shape is refused by `usable()`, so markers can never be staged.
        let split = SplitFile {
            path: "x.rs".into(),
            content: Some(merged),
            mode: "100644".into(),
            conflicts,
            replayed: 1,
            unavailable: None,
        };
        assert!(
            !split.usable(),
            "a conflicted reconstruction is never stageable"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_split_is_staged_without_disturbing_the_other_sessions_working_copy() {
        // The reason staging is index-only: disk holds BOTH sessions' work and the peer is still
        // editing it. Writing this session's version to disk would destroy the peer's live changes —
        // the exact loss this module exists to prevent.
        let Some(root) = temp_repo("indexonly") else {
            return;
        };
        let union = "shared: A's line and B's line\n";
        fs::write(root.join("a.txt"), union).unwrap();
        let split = SplitFile {
            path: "a.txt".into(),
            content: Some(b"shared: only A's line\n".to_vec()),
            mode: "100644".into(),
            conflicts: 0,
            replayed: 1,
            unavailable: None,
        };
        stage_split(&root, &split).expect("staging a reconstructed blob");

        let staged =
            git_ok(&root, &["show".into(), ":a.txt".into()]).expect("reading the staged version");
        assert_eq!(
            staged.trim_end(),
            "shared: only A's line",
            "the index holds this session's version alone"
        );
        assert_eq!(
            fs::read_to_string(root.join("a.txt")).unwrap(),
            union,
            "the working tree still holds the union — the peer's edits are untouched"
        );
        // And it is a real commit, not a staging trick: the committed tree carries the split version.
        git_ok(&root, &["commit".into(), "-m".into(), "A only".into()]).unwrap();
        let committed = git_ok(&root, &["show".into(), "HEAD:a.txt".into()]).unwrap();
        assert_eq!(committed.trim_end(), "shared: only A's line");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn a_split_that_removes_the_file_stages_a_deletion() {
        let Some(root) = temp_repo("removal") else {
            return;
        };
        let split = SplitFile {
            path: "a.txt".into(),
            content: None,
            mode: String::new(),
            conflicts: 0,
            replayed: 1,
            unavailable: None,
        };
        stage_split(&root, &split).expect("staging a removal");
        let staged = git_ok(
            &root,
            &["diff".into(), "--cached".into(), "--name-status".into()],
        )
        .unwrap();
        assert!(staged.starts_with('D'), "the deletion is staged: {staged}");
        assert!(
            root.join("a.txt").exists(),
            "the file itself is left on disk for the other session"
        );
        let _ = fs::remove_dir_all(root);
    }
}
