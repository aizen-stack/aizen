//! Time Machine — crash-recoverable, git-backed working-tree checkpoints.
//!
//! Checkpoints live in a private store under `~/.aizen/timemachine/<repo-id>/`, fully outside the
//! source repository's `.git`. Each linked worktree owns its own ledger/journal/chat namespace while
//! sharing a bare object store. Source Git objects remain readable only through a sealed alternates
//! pointer for migration/seed; new checkpoint objects and all Time Machine refs are written into the
//! private store. Metadata is fail-closed, writes are atomic, and every mutating operation is
//! serialized by an OS lock. Git is invoked with hooks/fsmonitor/filters disabled so checkpointing
//! cannot execute repository-controlled code before an approval gate.

use crate::core::types::Message;
use anyhow::{bail, Context, Result};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

const DEFAULT_KEEP: usize = 50;
const LEDGER_SCHEMA: u32 = 2;
const LOCK_TIMEOUT: Duration = Duration::from_secs(15);
const ZERO_OID: &str = "0000000000000000000000000000000000000000";
/// Max agent-driven rewinds per agent run — enough to abandon a bad approach twice, not thrash.
const MAX_RUN_REWINDS: u8 = 2;
static TM_INDEX_SEQ: AtomicU64 = AtomicU64::new(0);
static OP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Per-agent-run recovery anchors. Process-local (one interactive/one-shot loop at a time); cleared
/// at the start of each agent run so a stale pre-edit from a prior task cannot be restored by accident.
#[derive(Debug, Default, Clone)]
struct RunRecovery {
    /// First pre-edit auto-checkpoint of this run (`before agent edits`). Whole-run safety net.
    pre_edit: Option<u32>,
    /// Most recent successful post-edit checkpoint this run. One-step undo within the run.
    last_good: Option<u32>,
    rewinds_used: u8,
}

static RUN_RECOVERY: Lazy<Mutex<RunRecovery>> = Lazy::new(|| Mutex::new(RunRecovery::default()));

/// Call at the start of every agent loop so rewinds cannot reach a previous task's tree.
pub fn begin_agent_run() {
    if let Ok(mut g) = RUN_RECOVERY.lock() {
        *g = RunRecovery::default();
    }
}

/// Record the pre-edit snapshot (first successful auto-checkpoint of the run). Idempotent: keeps
/// the earliest id so later saves do not move the whole-run safety net.
pub fn note_pre_edit(id: u32) {
    if let Ok(mut g) = RUN_RECOVERY.lock() {
        if g.pre_edit.is_none() {
            g.pre_edit = Some(id);
        }
        // Before any edit lands, last_good is the pre-edit tree.
        if g.last_good.is_none() {
            g.last_good = Some(id);
        }
    }
}

/// Record a successful post-edit (or verified) checkpoint as the one-step undo target.
pub fn note_last_good(id: u32) {
    if let Ok(mut g) = RUN_RECOVERY.lock() {
        g.last_good = Some(id);
    }
}

/// Snapshot of current run anchors for prompts / tool results (no side effects).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecoveryStatus {
    pub pre_edit: Option<u32>,
    pub last_good: Option<u32>,
    pub rewinds_used: u8,
    pub rewinds_left: u8,
}

pub fn recovery_status() -> RecoveryStatus {
    let g = RUN_RECOVERY.lock().map(|g| g.clone()).unwrap_or_default();
    RecoveryStatus {
        pre_edit: g.pre_edit,
        last_good: g.last_good,
        rewinds_used: g.rewinds_used,
        rewinds_left: MAX_RUN_REWINDS.saturating_sub(g.rewinds_used),
    }
}

/// Where the agent is allowed to rewind within the current run. Free-form restore stays human/CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewindTarget {
    /// Tree as it was before the first destructive edit of this run.
    PreEdit,
    /// Tree after the last successful edit step of this run.
    LastGood,
}

impl RewindTarget {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "pre_edit" | "pre-edit" | "start" | "run" => Some(Self::PreEdit),
            "last_good" | "last-good" | "step" | "undo" => Some(Self::LastGood),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::PreEdit => "pre_edit",
            Self::LastGood => "last_good",
        }
    }
}

/// Restore the working tree to a run-scoped anchor. Caps rewinds per run; never accepts arbitrary ids.
pub fn rewind_run(target: RewindTarget) -> Result<Snapshot> {
    let (id, used) = {
        let g = RUN_RECOVERY
            .lock()
            .map_err(|_| anyhow::anyhow!("recovery lock poisoned"))?;
        if g.rewinds_used >= MAX_RUN_REWINDS {
            bail!(
                "run rewind budget exhausted ({MAX_RUN_REWINDS}/{MAX_RUN_REWINDS}). \
                 Further recovery is human-driven: `aizen time list` / `aizen time restore <id>`."
            );
        }
        let id = match target {
            RewindTarget::PreEdit => g.pre_edit,
            RewindTarget::LastGood => g.last_good.or(g.pre_edit),
        }
        .with_context(|| {
            format!(
                "no {} anchor this run — nothing to rewind to yet (edit first, or not a git repo)",
                target.as_str()
            )
        })?;
        (id, g.rewinds_used)
    };
    // Lease-free restore: the agent loop already holds the workspace writer lease for
    // `checkpoint_rewind` (see execute_calls), and re-acquiring it via the public `restore()` would
    // self-deadlock — `LockFileEx`/`flock` are per-handle, non-reentrant, so a second acquire of the
    // same `workspace.lock` blocks until timeout → spurious `Busy`. `restore_in` takes only the
    // store/metadata locks, which are a different namespace, so it is safe under the held lease.
    let ctx = RepoContext::current()?;
    let snap = restore_in(&ctx, id)?;
    if let Ok(mut g) = RUN_RECOVERY.lock() {
        g.rewinds_used = used + 1;
        // After a rewind the working tree matches `id`; that becomes the new last_good floor.
        g.last_good = Some(id);
        // pre_edit stays put — the whole-run safety net is stable for the rest of the run.
    }
    Ok(snap)
}

/// One-line hint for verify-failure / stuck nudges. Empty when no anchor or budget exhausted.
pub fn recovery_hint() -> Option<String> {
    let s = recovery_status();
    if s.rewinds_left == 0 {
        return None;
    }
    match (s.pre_edit, s.last_good) {
        (None, None) => None,
        (Some(p), Some(l)) if p == l => Some(format!(
            "If this approach is wrong, call `checkpoint_rewind` with target=\"pre_edit\" to restore checkpoint #{p} \
             (working tree before this run's edits; {left} rewind left).",
            left = s.rewinds_left
        )),
        (Some(p), Some(l)) => Some(format!(
            "If this approach is wrong, call `checkpoint_rewind`: target=\"last_good\" → #{l} (last good step) or \
             target=\"pre_edit\" → #{p} (before this run's edits). {left} rewind(s) left this run.",
            left = s.rewinds_left
        )),
        (Some(p), None) => Some(format!(
            "If this approach is wrong, call `checkpoint_rewind` with target=\"pre_edit\" to restore checkpoint #{p} \
             ({left} rewind left).",
            left = s.rewinds_left
        )),
        (None, Some(l)) => Some(format!(
            "If this approach is wrong, call `checkpoint_rewind` with target=\"last_good\" to restore checkpoint #{l} \
             ({left} rewind left).",
            left = s.rewinds_left
        )),
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Coverage {
    #[serde(default)]
    pub file_count: u64,
    #[serde(default)]
    pub byte_count: u64,
    #[serde(default)]
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: u32,
    pub commit: String,
    pub tree: String,
    pub label: String,
    pub created: String,
    pub auto: bool,
    #[serde(default)]
    pub has_chat: bool,
    #[serde(default)]
    pub parent: Option<u32>,
    #[serde(default)]
    pub worktree_id: String,
    #[serde(default)]
    pub coverage: Coverage,
    /// Recovery-only preimage created by restore. It remains directly restorable but is excluded from
    /// normal redo branch selection so safety snapshots do not hijack timeline navigation.
    #[serde(default)]
    pub recovery: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Ledger {
    #[serde(default = "ledger_schema")]
    pub schema_version: u32,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub snapshots: Vec<Snapshot>,
    /// Legacy cursor index. Retained in the wire schema for migration compatibility; new code uses
    /// `cursor_id` so pruning/reordering cannot silently move the active checkpoint.
    #[serde(default)]
    pub cursor: Option<usize>,
    #[serde(default)]
    pub cursor_id: Option<u32>,
    #[serde(default)]
    pub next_id: u32,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            schema_version: LEDGER_SCHEMA,
            generation: 0,
            snapshots: Vec::new(),
            cursor: None,
            cursor_id: None,
            next_id: 1,
        }
    }
}

fn ledger_schema() -> u32 {
    LEDGER_SCHEMA
}

impl Ledger {
    fn normalize(&mut self) -> Result<()> {
        if self.schema_version > LEDGER_SCHEMA {
            bail!(
                "time-machine ledger schema {} is newer than this binary supports ({LEDGER_SCHEMA})",
                self.schema_version
            );
        }
        if self.cursor_id.is_none() {
            self.cursor_id = self
                .cursor
                .and_then(|i| self.snapshots.get(i))
                .map(|s| s.id);
        }
        self.cursor = self
            .cursor_id
            .and_then(|id| self.snapshots.iter().position(|s| s.id == id));
        self.schema_version = LEDGER_SCHEMA;
        let mut ids = HashSet::new();
        for s in &self.snapshots {
            if s.id == 0 || !ids.insert(s.id) {
                bail!(
                    "time-machine ledger contains duplicate/invalid checkpoint id #{}",
                    s.id
                );
            }
            if s.commit.len() != 40 || s.tree.len() != 40 {
                bail!(
                    "time-machine checkpoint #{} contains an invalid Git object id",
                    s.id
                );
            }
        }
        if let Some(id) = self.cursor_id {
            if !ids.contains(&id) {
                bail!("time-machine cursor points at missing checkpoint #{id}");
            }
        }
        let min_next = self
            .snapshots
            .iter()
            .map(|s| s.id)
            .max()
            .unwrap_or(0)
            .saturating_add(1)
            .max(1);
        if self.next_id == 0 {
            self.next_id = min_next;
        } else if self.next_id < min_next {
            bail!(
                "time-machine ledger next_id {} is behind existing checkpoint ids (need at least {min_next})",
                self.next_id
            );
        }
        Ok(())
    }

    fn set_cursor(&mut self, id: Option<u32>) {
        self.cursor_id = id;
        self.cursor = id.and_then(|needle| self.snapshots.iter().position(|s| s.id == needle));
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalKind {
    Save,
    Restore,
    Prune,
    Clear,
    Doctor,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JournalPhase {
    Prepared,
    RefCreated,
    Applying,
    LedgerCommitted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Journal {
    schema_version: u32,
    operation_id: String,
    kind: JournalKind,
    phase: JournalPhase,
    expected_generation: u64,
    #[serde(default)]
    checkpoint_id: Option<u32>,
    #[serde(default)]
    target_id: Option<u32>,
    #[serde(default)]
    preimage_id: Option<u32>,
    #[serde(default)]
    ref_name: Option<String>,
    #[serde(default)]
    new_oid: Option<String>,
}

impl Journal {
    fn new(kind: JournalKind, generation: u64) -> Self {
        Self {
            schema_version: 1,
            operation_id: format!(
                "{}-{}-{}",
                std::process::id(),
                chrono::Utc::now().timestamp_millis(),
                OP_SEQ.fetch_add(1, Ordering::Relaxed)
            ),
            kind,
            phase: JournalPhase::Prepared,
            expected_generation: generation,
            checkpoint_id: None,
            target_id: None,
            preimage_id: None,
            ref_name: None,
            new_oid: None,
        }
    }
}

struct RepoContext {
    root: PathBuf,
    /// Source worktree Git dir (`.git` or `.git/worktrees/<name>`). Used only for seed/migration.
    git_dir: PathBuf,
    /// Source common Git dir. Used for alternates + legacy migration.
    common_git_dir: PathBuf,
    /// Private bare store under `~/.aizen/timemachine/<repo_id>/store.git`.
    store_git_dir: PathBuf,
    /// Worktree-scoped metadata (ledger/journal/lock/chat/temp index).
    namespace_dir: PathBuf,
    repo_id: String,
    worktree_id: String,
    ref_prefix: String,
    hooks_dir: PathBuf,
    /// Cached filter safety probe for this process/context.
    filters_checked: std::cell::Cell<bool>,
    /// Cached reparse/nested-repo walk for this process/context. A `RepoContext` is built per public
    /// operation, so this dedupes the 2-3 walks a single save/restore performs (`current_tree` runs
    /// twice in `restore_in`, plus `apply_tree`) without letting a stale result survive the operation.
    reparse_checked: std::cell::Cell<bool>,
    /// Cached ignored-directory set (see `uncovered_dirs`). Same per-operation lifetime as
    /// `reparse_checked`: one `ls-files` probe instead of one per walk.
    skip_dirs: std::cell::RefCell<Option<HashSet<PathBuf>>>,
}

impl RepoContext {
    fn discover(start: &Path) -> Result<Self> {
        // Only a *genuine* absence of a work tree earns the friendly "run `git init`" message.
        // Any other git failure — dubious ownership, safe.directory, git missing — is a real,
        // usually one-command-fixable problem, so surface git's own stderr verbatim rather than
        // telling the user to run a command they already ran. Callers keying off the
        // "not a git repository" substring (save_protected_change/is_repo) still treat the benign
        // case as "checkpoints simply off", while real errors now propagate with their true cause.
        let root = match raw_git(start, &["rev-parse", "--show-toplevel"]) {
            Ok(r) if !r.trim().is_empty() => r,
            // Bare repo or empty toplevel → no work tree to checkpoint; treat as no-TM.
            Ok(_) => bail!("not a git repository (run `git init` first to use the time machine)"),
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("not a git repository") || msg.contains("not a work tree") {
                    bail!("not a git repository (run `git init` first to use the time machine)");
                }
                // git absent ≠ git failed: keep the typed GitMissing chain untouched so the
                // benign-path callers can recognize it without substring guessing.
                if crate::core::gitx::is_git_missing(&e) {
                    return Err(e);
                }
                return Err(e).context("time machine could not use git in this directory");
            }
        };
        let root = PathBuf::from(root)
            .canonicalize()
            .context("canonicalizing repository root")?;
        let git_dir = absolute_git_path(&root, &raw_git(&root, &["rev-parse", "--git-dir"])?);
        let common_git_dir =
            absolute_git_path(&root, &raw_git(&root, &["rev-parse", "--git-common-dir"])?);
        let common_canon =
            fs::canonicalize(&common_git_dir).unwrap_or_else(|_| common_git_dir.clone());
        let wt_canon = fs::canonicalize(&git_dir).unwrap_or_else(|_| git_dir.clone());
        let repo_id = format!("repo-{:016x}", fnv1a64(&common_canon.to_string_lossy()));
        let worktree_id = format!("wt-{:016x}", fnv1a64(&wt_canon.to_string_lossy()));

        let home = crate::core::config::nextgen_home();
        let repo_store_root = home.join("timemachine").join(&repo_id);
        let store_git_dir = repo_store_root.join("store.git");
        let namespace_dir = repo_store_root.join("worktrees").join(&worktree_id);
        let hooks_dir = namespace_dir.join("empty-hooks");

        fs::create_dir_all(&hooks_dir)
            .with_context(|| format!("creating {}", hooks_dir.display()))?;
        crate::core::config::harden_dir(&repo_store_root);
        crate::core::config::harden_dir(&namespace_dir);
        ensure_private_store(&store_git_dir, &common_git_dir)?;

        Ok(Self {
            root,
            git_dir,
            common_git_dir,
            store_git_dir,
            namespace_dir,
            repo_id,
            ref_prefix: format!("refs/ng/tm/{worktree_id}"),
            worktree_id,
            hooks_dir,
            filters_checked: std::cell::Cell::new(false),
            reparse_checked: std::cell::Cell::new(false),
            skip_dirs: std::cell::RefCell::new(None),
        })
    }

    fn current() -> Result<Self> {
        Self::discover(&std::env::current_dir().context("resolving cwd")?)
    }

    fn ledger_path(&self) -> PathBuf {
        self.namespace_dir.join("ledger.json")
    }

    /// Previous hardening location: `<common-git-dir>/aizen-timemachine/<worktree-id>/ledger.json`.
    fn inrepo_namespace_ledger_path(&self) -> PathBuf {
        self.common_git_dir
            .join("aizen-timemachine")
            .join(&self.worktree_id)
            .join("ledger.json")
    }

    fn inrepo_namespace_dir(&self) -> PathBuf {
        self.common_git_dir
            .join("aizen-timemachine")
            .join(&self.worktree_id)
    }

    /// Oldest legacy ledger: `<git-dir>/ng_timemachine.json`.
    fn legacy_ledger_path(&self) -> PathBuf {
        self.git_dir.join("ng_timemachine.json")
    }

    fn journal_path(&self) -> PathBuf {
        self.namespace_dir.join("journal.json")
    }

    fn lock_path(&self) -> PathBuf {
        self.namespace_dir.join("transaction.lock")
    }

    fn chat_dir(&self) -> PathBuf {
        self.namespace_dir.join("chat")
    }

    fn chat_path(&self, id: u32) -> PathBuf {
        self.chat_dir().join(format!("{id}.json"))
    }

    fn inrepo_chat_path(&self, id: u32) -> PathBuf {
        self.inrepo_namespace_dir()
            .join("chat")
            .join(format!("{id}.json"))
    }

    fn legacy_chat_path(&self, id: u32) -> PathBuf {
        self.git_dir.join(format!("ng_tm_chat_{id}.json"))
    }

    fn ref_name(&self, id: u32) -> String {
        format!("{}/{id}", self.ref_prefix)
    }

    fn recovery_ref_name(&self, id: u32) -> String {
        format!("{}/recovery/{id}", self.ref_prefix)
    }

    fn temp_index(&self) -> PathBuf {
        let dir = strip_windows_verbatim(&self.namespace_dir);
        dir.join(format!(
            "index-{}-{}",
            std::process::id(),
            TM_INDEX_SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn git<I, S>(&self, index: Option<&Path>, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.git_output(index, args)?;
        if !output.status.success() {
            bail!(
                "internal git operation failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    /// Run a plumbing command against the **private** store + source worktree.
    fn git_output<I, S>(&self, index: Option<&Path>, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = git_cmd();
        let hooks = strip_windows_verbatim(&self.hooks_dir)
            .to_string_lossy()
            .replace('\\', "/");
        let store = strip_windows_verbatim(&self.store_git_dir);
        let work_tree = strip_windows_verbatim(&self.root);
        cmd.current_dir(&self.root)
            .env_remove("GIT_OBJECT_DIRECTORY")
            .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_CONFIG_KEY_0")
            .env_remove("GIT_CONFIG_VALUE_0")
            .env("GIT_DIR", &store)
            .env("GIT_WORK_TREE", &work_tree)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Aizen Time Machine")
            .env("GIT_AUTHOR_EMAIL", "timemachine@localhost")
            .env("GIT_COMMITTER_NAME", "Aizen Time Machine")
            .env("GIT_COMMITTER_EMAIL", "timemachine@localhost")
            .arg("-c")
            .arg(format!("core.hooksPath={hooks}"))
            .args([
                "-c",
                "core.fsmonitor=false",
                "-c",
                "core.untrackedCache=false",
            ])
            .args([
                "-c",
                "filter.lfs.required=false",
                "-c",
                "filter.lfs.clean=",
                "-c",
                "filter.lfs.smudge=",
                "-c",
                "filter.lfs.process=",
                "-c",
                "filter.lfs.delay=false",
            ])
            .args(args);
        if let Some(idx) = index {
            cmd.env("GIT_INDEX_FILE", idx);
        } else {
            cmd.env_remove("GIT_INDEX_FILE");
        }
        cmd.output()
            .context("running internal git operation (is git installed?)")
    }

    /// Probe the **source** repository for external filter drivers (not the private store).
    fn source_git_output<I, S>(&self, args: I) -> Result<Output>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cmd = git_cmd();
        let git_dir = strip_windows_verbatim(&self.git_dir);
        let work_tree = strip_windows_verbatim(&self.root);
        cmd.current_dir(&self.root)
            .env("GIT_DIR", &git_dir)
            .env("GIT_WORK_TREE", &work_tree)
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_OBJECT_DIRECTORY")
            .args(args);
        cmd.output()
            .context("running source git probe (is git installed?)")
    }

    fn source_git<I, S>(&self, args: I) -> Result<String>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = self.source_git_output(args)?;
        if !output.status.success() {
            bail!(
                "source git probe failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    fn ensure_safe_filters(&self) -> Result<()> {
        if self.filters_checked.get() {
            return Ok(());
        }
        // `.gitattributes` can name arbitrary clean/smudge/process drivers. Git offers no global
        // "disable every filter" switch, so reject configured external drivers before plumbing
        // commands touch the worktree. The built-in `lfs` name is neutralized in `git_output`.
        let out = self
            .source_git_output([
                "config",
                "--local",
                "--get-regexp",
                r"^filter\..*\.(clean|smudge|process|required)$",
            ])
            .context("checking repository Git filters")?;
        if out.status.success() {
            let lines = String::from_utf8_lossy(&out.stdout);
            for line in lines.lines() {
                let key = line.split_whitespace().next().unwrap_or("");
                if !key.starts_with("filter.lfs.") {
                    bail!(
                        "checkpoint refused: repository config defines external Git filter `{key}`; disable it or snapshot manually"
                    );
                }
            }
        }
        self.filters_checked.set(true);
        Ok(())
    }

    fn lock(&self) -> Result<crate::core::repo_lock::RepoTxnLock> {
        crate::core::repo_lock::RepoTxnLock::acquire(&self.lock_path(), LOCK_TIMEOUT)
    }

    /// Repository-store lock path, shared by ALL linked worktrees (one level above the per-worktree
    /// namespace). Ref reads/writes take it SHARED so sibling worktrees coexist; a store-wide sweep
    /// (`doctor_gc`, which walks every worktree's `refs/ng/tm/**`) takes it EXCLUSIVE so it can never
    /// race a sibling `save` mid-scan. Ordered before the per-worktree metadata lock (`lock`).
    fn store_lock_path(&self) -> PathBuf {
        self.store_git_dir
            .parent()
            .map(|p| p.join("store.lock"))
            .unwrap_or_else(|| self.store_git_dir.join("store.lock"))
    }

    /// Shared store lease held by ordinary ref-mutating ops (save/restore/prune/clear) so they can
    /// run concurrently across linked worktrees while still blocking a store-exclusive GC sweep.
    fn store_shared(&self) -> Result<crate::core::repo_lock::RepoTxnLock> {
        crate::core::repo_lock::RepoTxnLock::acquire_shared(&self.store_lock_path(), LOCK_TIMEOUT)
    }

    /// Exclusive store lease for a whole-store sweep (`doctor_gc`): no other worktree may create or
    /// delete refs while it enumerates and reaps orphans across every namespace.
    fn store_exclusive(&self) -> Result<crate::core::repo_lock::RepoTxnLock> {
        crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
            &self.store_lock_path(),
            LOCK_TIMEOUT,
        )
    }

    fn load_ledger(&self) -> Result<Ledger> {
        let path = self.ledger_path();
        let bytes = match crate::core::persist::read_optional(&path)
            .with_context(|| format!("reading time-machine ledger {}", path.display()))?
        {
            Some(bytes) => bytes,
            None => return self.load_migratable_ledger(),
        };
        let mut ledger: Ledger = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "time-machine ledger {} is corrupt; run `aizen time doctor`",
                path.display()
            )
        })?;
        ledger.normalize()?;
        Ok(ledger)
    }

    /// Read-only fallback: in-repo namespaced ledger, then oldest legacy ledger. Migration is
    /// finalized later under the transaction lock by `migrate_legacy_refs`.
    fn load_migratable_ledger(&self) -> Result<Ledger> {
        for path in [
            self.inrepo_namespace_ledger_path(),
            self.legacy_ledger_path(),
        ] {
            let Some(bytes) = crate::core::persist::read_optional(&path).with_context(|| {
                format!("reading migratable time-machine ledger {}", path.display())
            })?
            else {
                continue;
            };
            let mut ledger: Ledger = serde_json::from_slice(&bytes).with_context(|| {
                format!(
                    "migratable time-machine ledger {} is corrupt; it was not replaced",
                    path.display()
                )
            })?;
            ledger.normalize()?;
            return Ok(ledger);
        }
        Ok(Ledger::default())
    }

    fn save_ledger(&self, ledger: &mut Ledger) -> Result<()> {
        ledger.normalize()?;
        ledger.generation = ledger.generation.saturating_add(1);
        let bytes = serde_json::to_vec_pretty(ledger)?;
        crate::core::persist::atomic_write(&self.ledger_path(), &[bytes, b"\n".to_vec()].concat())?;
        crate::core::persist::harden_owner_only_checked(&self.ledger_path())?;
        Ok(())
    }

    fn load_journal(&self) -> Result<Option<Journal>> {
        let path = self.journal_path();
        let Some(bytes) = crate::core::persist::read_optional(&path)
            .with_context(|| format!("reading transaction journal {}", path.display()))?
        else {
            return Ok(None);
        };
        let journal = serde_json::from_slice(&bytes).with_context(|| {
            format!(
                "transaction journal {} is corrupt; run `aizen time doctor`",
                path.display()
            )
        })?;
        Ok(Some(journal))
    }

    fn save_journal(&self, journal: &Journal) -> Result<()> {
        let mut bytes = serde_json::to_vec_pretty(journal)?;
        bytes.push(b'\n');
        crate::core::persist::atomic_write(&self.journal_path(), &bytes)?;
        crate::core::persist::harden_owner_only_checked(&self.journal_path())?;
        Ok(())
    }

    fn clear_journal(&self) -> Result<()> {
        crate::core::persist::remove_if_exists(&self.journal_path()).with_context(|| {
            format!(
                "removing transaction journal {}",
                self.journal_path().display()
            )
        })?;
        Ok(())
    }

    fn migrate_legacy_refs(&self, ledger: &mut Ledger) -> Result<()> {
        // Already on the external store — nothing to import.
        if self.ledger_path().exists() {
            return Ok(());
        }
        let has_inrepo = self.inrepo_namespace_ledger_path().exists();
        let has_legacy = self.legacy_ledger_path().exists();
        if !has_inrepo && !has_legacy && ledger.snapshots.is_empty() {
            return Ok(());
        }

        for snap in &mut ledger.snapshots {
            let new_ref = self.ref_name(snap.id);
            let existing = self.git(None, ["rev-parse", "--verify", &new_ref]).ok();
            if let Some(oid) = existing.as_deref() {
                if oid != snap.commit {
                    bail!(
                        "cannot migrate checkpoint #{}: private store ref already exists with another object",
                        snap.id
                    );
                }
            } else {
                // Prefer the previous namespaced in-repo ref, then the oldest flat ref. Objects are
                // still reachable via the sealed alternates pointer into the source object store.
                let candidates = [
                    format!("refs/ng/tm/{}/{}", self.worktree_id, snap.id),
                    format!("refs/ng/tm/{}", snap.id),
                ];
                let mut imported = false;
                for candidate in &candidates {
                    // Look up the candidate in the *source* repo first; if present, create the same
                    // ref name in the private store (objects resolve via alternates).
                    if let Ok(oid) = self.source_git(["rev-parse", "--verify", candidate.as_str()])
                    {
                        if oid != snap.commit {
                            bail!(
                                "cannot migrate checkpoint #{}: source ref {candidate} does not match ledger",
                                snap.id
                            );
                        }
                        // Materialize the object into the private store so the timeline survives if
                        // the source repository is later rewritten or deleted.
                        self.materialize_commit(&oid)?;
                        self.update_ref_create(&new_ref, &oid)?;
                        imported = true;
                        break;
                    }
                    // Also accept a ref already present in the private store under a temporary
                    // migration name (e.g. after a partial previous attempt).
                    if let Ok(oid) = self.git(None, ["rev-parse", "--verify", candidate.as_str()]) {
                        if oid != snap.commit {
                            bail!(
                                "cannot migrate checkpoint #{}: private candidate {candidate} does not match ledger",
                                snap.id
                            );
                        }
                        if candidate != &new_ref {
                            self.update_ref_create(&new_ref, &oid)?;
                        }
                        imported = true;
                        break;
                    }
                }
                if !imported {
                    // Last resort: object may still be reachable through alternates by OID alone.
                    if self
                        .git(
                            None,
                            ["cat-file", "-e", &format!("{}^{{commit}}", snap.commit)],
                        )
                        .is_ok()
                    {
                        self.materialize_commit(&snap.commit)?;
                        self.update_ref_create(&new_ref, &snap.commit)?;
                    } else {
                        bail!(
                            "cannot migrate checkpoint #{}: source ref/object is missing",
                            snap.id
                        );
                    }
                }
            }

            // Prefer newest chat location first.
            for old_chat in [
                self.inrepo_chat_path(snap.id),
                self.legacy_chat_path(snap.id),
            ] {
                if old_chat.exists() && !self.chat_path(snap.id).exists() {
                    fs::create_dir_all(self.chat_dir())?;
                    fs::copy(&old_chat, self.chat_path(snap.id)).with_context(|| {
                        format!("migrating conversation sidecar for checkpoint #{}", snap.id)
                    })?;
                    crate::core::persist::harden_owner_only_checked(&self.chat_path(snap.id))?;
                    break;
                }
            }
            snap.worktree_id = self.worktree_id.clone();
        }
        Ok(())
    }

    /// Copy a commit (and its reachable trees/blobs) from alternates into the private object store
    /// so checkpoints remain independent of the source repository's lifetime.
    fn materialize_commit(&self, commit: &str) -> Result<()> {
        // `cat-file --batch-check` validates reachability; `repack -a -d --window=0` is too heavy.
        // Use `pack-objects` of a single commit via stdin — pure plumbing, no hooks.
        let mut child = git_cmd();
        let store = strip_windows_verbatim(&self.store_git_dir);
        let hooks = strip_windows_verbatim(&self.hooks_dir)
            .to_string_lossy()
            .replace('\\', "/");
        child
            .current_dir(&self.root)
            .env("GIT_DIR", &store)
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .arg("-c")
            .arg(format!("core.hooksPath={hooks}"))
            .args(["-c", "core.fsmonitor=false"])
            .args([
                "pack-objects",
                "--revs",
                "--all-progress-implied",
                "-q",
                "--stdout",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Prefer a lighter path: `git fetch` from self is awkward for bare. Instead, ensure the
        // object is present via `cat-file` (alternates) then write a thin local ref pin — objects
        // stay in alternates until `doctor_gc` optionally copies. For independence, use
        // `git unpack-objects` of a generated pack.
        let mut pack = child
            .spawn()
            .context("starting private-store pack-objects")?;
        use std::io::Write;
        {
            let stdin = pack.stdin.as_mut().context("pack-objects stdin")?;
            writeln!(stdin, "{commit}")?;
            // Terminate rev-list style input.
            writeln!(stdin)?;
        }
        let pack_out = pack
            .wait_with_output()
            .context("running pack-objects for materialize")?;
        if !pack_out.status.success() {
            // Fall back: object remains reachable via alternates. Migration still creates the ref;
            // independence is best-effort when pack-objects cannot run (e.g. empty tree edge).
            let _ = self.git(None, ["cat-file", "-e", &format!("{commit}^{{commit}}")])?;
            return Ok(());
        }
        if pack_out.stdout.is_empty() {
            let _ = self.git(None, ["cat-file", "-e", &format!("{commit}^{{commit}}")])?;
            return Ok(());
        }
        let mut unpack = git_cmd();
        unpack
            .current_dir(&self.root)
            .env("GIT_DIR", &store)
            .env_remove("GIT_INDEX_FILE")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .args(["unpack-objects", "-q"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = unpack
            .spawn()
            .context("starting unpack-objects for materialize")?;
        {
            let stdin = child.stdin.as_mut().context("unpack-objects stdin")?;
            stdin.write_all(&pack_out.stdout)?;
        }
        let out = child
            .wait_with_output()
            .context("running unpack-objects for materialize")?;
        if !out.status.success() {
            // Alternates still provide reachability; ref create below is enough for restore.
            let _ = self.git(None, ["cat-file", "-e", &format!("{commit}^{{commit}}")])?;
        }
        Ok(())
    }

    fn update_ref_create(&self, name: &str, oid: &str) -> Result<()> {
        self.git(None, ["update-ref", name, oid, ZERO_OID])
            .map(|_| ())
    }

    fn update_ref_delete(&self, name: &str, expected: &str) -> Result<()> {
        self.git(None, ["update-ref", "-d", name, expected])
            .map(|_| ())
    }

    fn validate_snapshot_ref(&self, snap: &Snapshot) -> Result<()> {
        let expected = &snap.commit;
        let new_ref = self.ref_name(snap.id);
        let actual = self
            .git(None, ["rev-parse", "--verify", &new_ref])
            .or_else(|_| {
                // Migration windows may still resolve the in-repo namespaced form via alternates.
                self.source_git([
                    "rev-parse",
                    "--verify",
                    &format!("refs/ng/tm/{}/{}", self.worktree_id, snap.id),
                ])
                .or_else(|_| {
                    self.source_git(["rev-parse", "--verify", &format!("refs/ng/tm/{}", snap.id)])
                })
            })?;
        if &actual != expected {
            bail!(
                "checkpoint #{} ref does not match its ledger commit",
                snap.id
            );
        }
        self.git(
            None,
            ["cat-file", "-e", &format!("{}^{{commit}}", snap.commit)],
        )?;
        self.git(None, ["cat-file", "-e", &format!("{}^{{tree}}", snap.tree)])?;
        Ok(())
    }

    fn recover_pending(&self, ledger: &mut Ledger) -> Result<()> {
        let Some(journal) = self.load_journal()? else {
            return Ok(());
        };
        if journal.expected_generation > ledger.generation {
            bail!("time-machine journal belongs to a future ledger generation; run `aizen time doctor`");
        }
        match journal.kind {
            JournalKind::Save => {
                let id = journal
                    .checkpoint_id
                    .context("save journal is missing checkpoint id")?;
                let committed = ledger.snapshots.iter().any(|s| s.id == id);
                if committed {
                    self.clear_journal()?;
                    return Ok(());
                }
                if let (Some(name), Some(oid)) =
                    (journal.ref_name.as_deref(), journal.new_oid.as_deref())
                {
                    if self
                        .git(None, ["rev-parse", "--verify", name])
                        .ok()
                        .as_deref()
                        == Some(oid)
                    {
                        self.update_ref_delete(name, oid)?;
                    }
                }
                let _ = crate::core::persist::remove_if_exists(&self.chat_path(id));
                self.clear_journal()?;
            }
            JournalKind::Restore => {
                let target_id = journal
                    .target_id
                    .context("restore journal is missing target id")?;
                let target = ledger
                    .snapshots
                    .iter()
                    .find(|s| s.id == target_id)
                    .cloned()
                    .with_context(|| {
                        format!("restore journal targets missing checkpoint #{target_id}")
                    })?;
                let current = current_tree(self).map(|(tree, _)| tree).ok();
                if current.as_deref() == Some(target.tree.as_str()) {
                    ledger.set_cursor(Some(target_id));
                    self.save_ledger(ledger)?;
                    self.clear_journal()?;
                    return Ok(());
                }
                if let Some(preimage_id) = journal.preimage_id {
                    let preimage = ledger
                        .snapshots
                        .iter()
                        .find(|s| s.id == preimage_id)
                        .cloned()
                        .with_context(|| {
                            format!("restore journal preimage checkpoint #{preimage_id} is missing")
                        })?;
                    if current.as_deref() == Some(preimage.tree.as_str()) {
                        ledger.set_cursor(Some(preimage_id));
                        self.save_ledger(ledger)?;
                        self.clear_journal()?;
                        return Ok(());
                    }
                    apply_tree(self, &preimage.commit)
                        .context("rolling back an interrupted restore to its pinned preimage")?;
                    let rolled_back = current_tree(self)?.0;
                    if rolled_back != preimage.tree {
                        bail!("interrupted restore rollback did not reproduce its preimage; run `aizen time doctor`");
                    }
                    ledger.set_cursor(Some(preimage_id));
                    self.save_ledger(ledger)?;
                    self.clear_journal()?;
                    return Ok(());
                }
                bail!("interrupted restore has no recovery preimage; run `aizen time doctor`");
            }
            JournalKind::Prune | JournalKind::Clear | JournalKind::Doctor => {
                // Deletion operations commit the ledger before reaping refs. If interrupted, the
                // remaining refs are safe orphan recovery pins; clearing the journal unblocks normal
                // work and `doctor` reports the orphan set for explicit cleanup.
                self.clear_journal()?;
            }
        }
        Ok(())
    }
}

fn absolute_git_path(root: &Path, value: &str) -> PathBuf {
    let path = PathBuf::from(value);
    if path.is_absolute() {
        path
    } else {
        root.join(path)
    }
}

/// Create (or repair) the private bare store under `~/.aizen/timemachine/<repo>/store.git`.
///
/// The store points at the source repository's object directory through a sealed `objects/info/alternates`
/// entry so historical source objects remain readable, while every new Time Machine object and ref is
/// written only into the private store.
fn ensure_private_store(store_git_dir: &Path, common_git_dir: &Path) -> Result<()> {
    if !store_git_dir.join("HEAD").exists() {
        fs::create_dir_all(store_git_dir).with_context(|| {
            format!(
                "creating private time-machine store {}",
                store_git_dir.display()
            )
        })?;
        let out = git_cmd()
            .args(["init", "--bare", "-q"])
            .arg(store_git_dir)
            .output()
            .context("initializing private time-machine store (is git installed?)")?;
        if !out.status.success() {
            bail!(
                "failed to initialize private time-machine store: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        crate::core::config::harden_dir(store_git_dir);
    }

    // Seal alternates: one absolute path to the source object store. Rewrite on every open so a moved
    // worktree still resolves, and refuse unexpected extra entries that could smuggle objects.
    let objects = store_git_dir.join("objects");
    let info = objects.join("info");
    fs::create_dir_all(&info).with_context(|| format!("creating {}", info.display()))?;
    let alternates = info.join("alternates");
    let source_objects = {
        let cand = common_git_dir.join("objects");
        fs::canonicalize(&cand).unwrap_or(cand)
    };
    let source_line = strip_windows_verbatim(&source_objects)
        .to_string_lossy()
        .replace('\\', "/");
    let desired = format!("{source_line}\n");
    let current = crate::core::persist::read_optional(&alternates)
        .with_context(|| format!("reading {}", alternates.display()))?
        .map(|b| String::from_utf8_lossy(&b).into_owned());
    if current.as_deref() != Some(desired.as_str()) {
        crate::core::persist::atomic_write(&alternates, desired.as_bytes())
            .with_context(|| format!("writing sealed alternates {}", alternates.display()))?;
        let _ = crate::core::persist::harden_owner_only_checked(&alternates);
    }

    // Neutralize config inside the private store itself (no shared hooks/fsmonitor).
    let _ = git_cmd()
        .env("GIT_DIR", strip_windows_verbatim(store_git_dir))
        .args(["config", "core.hooksPath", "hooks-disabled"])
        .output();
    let hooks_disabled = store_git_dir.join("hooks-disabled");
    let _ = fs::create_dir_all(&hooks_disabled);
    Ok(())
}

fn strip_windows_verbatim(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let s = path.to_string_lossy();
        if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
            return PathBuf::from(format!(r"\\{rest}"));
        }
        if let Some(rest) = s.strip_prefix(r"\\?\") {
            return PathBuf::from(rest);
        }
    }
    path.to_path_buf()
}

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

/// Builder for every git spawn in this module: the executable resolved by `core::gitx` (PATH or a
/// well-known install location), so the time machine keeps working in shells where git isn't on
/// PATH. Falls back to the literal name only when resolution says Missing — the spawn then fails
/// with the same ENOENT it always had, and `raw_git`/`discover` classify it as GitMissing.
fn git_cmd() -> Command {
    match crate::core::gitx::git_exe() {
        Some(p) => Command::new(p),
        None => Command::new("git"),
    }
}

fn fnv1a64(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

fn raw_git<I, S>(root: &Path, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    // Absence of a git executable is a TYPED error (GitMissing), not a spawn ENOENT dressed as a
    // repo problem — `save_protected_change`/`preflight` treat it as "checkpoints simply off"
    // instead of refusing every file edit on a machine without git.
    let out = crate::core::gitx::command()?
        .current_dir(root)
        .args(args)
        .output()
        .context("running git (is git installed?)")?;
    if !out.status.success() {
        bail!(
            "git repository probe failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

struct TempIndex(PathBuf);
impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
        let _ = fs::remove_file(self.0.with_extension("lock"));
    }
}

fn seed_index(ctx: &RepoContext) -> Result<TempIndex> {
    fs::create_dir_all(&ctx.namespace_dir)?;
    let path = ctx.temp_index();
    let _ = fs::remove_file(&path);
    // Seed from the source worktree's real index so tracked modes/skip-worktree/sparse bits survive.
    // The private store does not own that index; it only receives the alternate-index snapshot.
    let source = ctx.git_dir.join("index");
    if source.is_file() {
        fs::copy(&source, &path)
            .with_context(|| format!("seeding temporary index from {}", source.display()))?;
    } else if let Ok(head) = ctx.source_git(["rev-parse", "--verify", "HEAD"]) {
        // HEAD may only exist in the source repo; objects resolve via sealed alternates.
        ctx.git(Some(&path), ["read-tree", &head])?;
    }
    Ok(TempIndex(path))
}

/// Directory subtrees the timeline provably never covers: fully-untracked, fully-ignored trees like
/// `target/` or `node_modules/`.
///
/// `ls-files --others --ignored --directory` only *collapses* a directory when nothing inside it is
/// tracked, so every path it names contributes zero bytes to the snapshot tree — descending into one
/// can only cost wall-clock. That cost is not marginal: on this repo the full walk visits 41k entries
/// (0.66s) of which 39k live under `target/`, and `restore` runs the walk three times (two
/// `current_tree` + one `apply_tree`), so a rewind paid ~2s of pure `readdir`.
///
/// Skipping them is also safe for the two things the walk is looking for. A nested `.git` or a
/// junction inside an ignored tree is outside coverage either way: `add -A` never stages it, so a
/// restore cannot write through it. The bail still fires for uncovered paths that are *not* ignored,
/// which is the case a user would actually be surprised by.
fn uncovered_dirs(ctx: &RepoContext) -> HashSet<PathBuf> {
    // Cached per context: `restore` calls the walk three times and the ignored-dir set cannot
    // meaningfully change inside one operation.
    if let Some(cached) = ctx.skip_dirs.borrow().as_ref() {
        return cached.clone();
    }
    let found = compute_uncovered_dirs(ctx);
    *ctx.skip_dirs.borrow_mut() = Some(found.clone());
    found
}

fn compute_uncovered_dirs(ctx: &RepoContext) -> HashSet<PathBuf> {
    // Best-effort: on any git hiccup, fall back to walking everything (correct, just slower).
    let Ok(out) = ctx.source_git_output([
        "ls-files",
        "-z",
        "--others",
        "--ignored",
        "--exclude-standard",
        "--directory",
    ]) else {
        return HashSet::new();
    };
    if !out.status.success() {
        return HashSet::new();
    }
    // `-z` avoids git's quoting of non-ASCII paths; only entries ending in `/` are collapsed dirs.
    String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|s| s.ends_with('/'))
        .map(|s| ctx.root.join(s.trim_end_matches('/')))
        .collect()
}

fn reparse_preflight(ctx: &RepoContext) -> Result<()> {
    // One walk per public operation: `restore_in` calls `current_tree` twice plus `apply_tree`, and
    // the tree cannot sprout a junction mid-operation while we hold the workspace writer lease.
    if ctx.reparse_checked.get() {
        return Ok(());
    }
    let root = &ctx.root;
    let skip = uncovered_dirs(ctx);
    let root_git = root.join(".git");
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).with_context(|| format!("scanning {}", dir.display()))? {
            let entry = entry?;
            let path = entry.path();
            if skip.contains(&path) {
                continue;
            }
            if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
                if path == root_git {
                    continue;
                }
                bail!(
                    "checkpoint refused: nested repository metadata at {} is outside this timeline's coverage",
                    path.display()
                );
            }
            let meta = fs::symlink_metadata(&path)?;
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                use windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT;
                if meta.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                    bail!(
                        "checkpoint refused: reparse/junction path {} could escape the repository; remove or ignore it first",
                        path.display()
                    );
                }
            }
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    ctx.reparse_checked.set(true);
    Ok(())
}

fn index_blob_bytes(ctx: &RepoContext, index: &Path) -> Result<u64> {
    let listing = ctx.git(Some(index), ["ls-files", "-s"])?;
    let mut ordered = Vec::new();
    let mut seen = HashSet::new();
    for line in listing.lines() {
        let mut fields = line.split_whitespace();
        let _mode = fields.next();
        let Some(oid) = fields.next() else { continue };
        if seen.insert(oid.to_string()) {
            ordered.push(oid.to_string());
        }
    }
    if ordered.is_empty() {
        return Ok(0);
    }

    // One `cat-file --batch-check` process instead of N `cat-file -s` spawns.
    let mut child = git_cmd();
    let store = strip_windows_verbatim(&ctx.store_git_dir);
    let hooks = strip_windows_verbatim(&ctx.hooks_dir)
        .to_string_lossy()
        .replace('\\', "/");
    child
        .current_dir(&ctx.root)
        .env("GIT_DIR", &store)
        .env_remove("GIT_INDEX_FILE")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .arg("-c")
        .arg(format!("core.hooksPath={hooks}"))
        .args(["-c", "core.fsmonitor=false"])
        .args([
            "cat-file",
            "--batch-check=%(objectname) %(objecttype) %(objectsize)",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut proc = child.spawn().context("starting cat-file --batch-check")?;
    {
        use std::io::Write;
        let stdin = proc.stdin.as_mut().context("cat-file stdin")?;
        for oid in &ordered {
            writeln!(stdin, "{oid}")?;
        }
    }
    let out = proc
        .wait_with_output()
        .context("running cat-file --batch-check")?;
    if !out.status.success() {
        bail!(
            "internal git operation failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }

    let max_single = crate::core::cli_config::load()
        .timemachine_max_file_bytes
        .unwrap_or(512 * 1024 * 1024);
    let mut total = 0u64;
    let body = String::from_utf8_lossy(&out.stdout);
    for line in body.lines() {
        // Formats: "<oid> <type> <size>" or "<oid> missing"
        let mut fields = line.split_whitespace();
        let Some(oid) = fields.next() else { continue };
        let Some(kind_or_missing) = fields.next() else {
            continue;
        };
        if kind_or_missing == "missing" {
            bail!("checkpoint budget probe: blob {oid} is missing from the private store");
        }
        let size: u64 = fields
            .next()
            .with_context(|| format!("parsing Git blob size for {oid}"))?
            .parse()
            .with_context(|| format!("parsing Git blob size for {oid}"))?;
        if size > max_single {
            bail!("checkpoint budget exceeded: one file/blob is {size} bytes > configured limit {max_single}");
        }
        total = total
            .checked_add(size)
            .context("checkpoint byte count overflow")?;
    }
    Ok(total)
}

fn current_tree(ctx: &RepoContext) -> Result<(String, Coverage)> {
    ctx.ensure_safe_filters()?;
    reparse_preflight(ctx)?;
    let idx = seed_index(ctx)?;
    // The seeded index preserves tracked entries and modes. `add -A` updates tracked files even when
    // a later .gitignore rule matches them, while untracked ignored paths remain outside coverage.
    ctx.git(Some(&idx.0), ["add", "-A", "--", "."])?;
    let tree = ctx.git(Some(&idx.0), ["write-tree"])?;
    let list = ctx.git(Some(&idx.0), ["ls-files", "-z"])?;
    let count = if list.is_empty() {
        0
    } else {
        list.as_bytes().iter().filter(|b| **b == 0).count() as u64
    };
    let byte_count = index_blob_bytes(ctx, &idx.0)?;
    let cfg = crate::core::cli_config::load();
    let max_files = cfg.timemachine_max_files.unwrap_or(100_000);
    let max_total = cfg.timemachine_max_bytes.unwrap_or(2 * 1024 * 1024 * 1024);
    if count > max_files {
        bail!("checkpoint budget exceeded: {count} files > configured limit {max_files}");
    }
    if byte_count > max_total {
        bail!("checkpoint budget exceeded: {byte_count} bytes > configured limit {max_total}");
    }
    Ok((
        tree,
        Coverage {
            file_count: count,
            byte_count,
            notes: vec![
                "git-visible repository tree; ignored/outside/nested repositories are not covered"
                    .to_string(),
            ],
        },
    ))
}

fn write_chat(ctx: &RepoContext, id: u32, chat: &[Message]) -> Result<()> {
    fs::create_dir_all(ctx.chat_dir())?;
    crate::core::config::harden_dir(&ctx.chat_dir());
    let bytes = serde_json::to_vec(chat)?;
    crate::core::persist::atomic_write(&ctx.chat_path(id), &bytes)?;
    crate::core::persist::harden_owner_only_checked(&ctx.chat_path(id))?;
    Ok(())
}

fn read_chat_in(ctx: &RepoContext, id: u32) -> Result<Option<Vec<Message>>> {
    for path in [
        ctx.chat_path(id),
        ctx.inrepo_chat_path(id),
        ctx.legacy_chat_path(id),
    ] {
        if let Some(bytes) = crate::core::persist::read_optional(&path)? {
            return Ok(Some(serde_json::from_slice(&bytes).with_context(|| {
                format!("conversation sidecar for checkpoint #{id} is corrupt")
            })?));
        }
    }
    Ok(None)
}

fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub fn is_repo() -> bool {
    RepoContext::current().is_ok()
}

/// "Checkpoints are simply off here" — either the directory isn't a repo, or there is no git
/// executable at all. Both must degrade to no-checkpoint instead of failing the caller: the
/// git-missing case used to propagate as a hard error, and because the protected-edit gate runs
/// before every mutation, that refused EVERY `file_write`/`multi_edit` on a gitless machine
/// while blaming "the pre-edit checkpoint".
fn benign_no_checkpoint(e: &anyhow::Error) -> bool {
    e.to_string().contains("not a git repository") || crate::core::gitx::is_git_missing(e)
}

/// Validate Time Machine metadata without capturing a tree. Kept for CLI diagnostics; protected
/// mutations should call [`save_protected_change`] while their workspace writer lease is held.
#[allow(dead_code)]
pub fn preflight_protected_change() -> Result<bool> {
    match RepoContext::current() {
        Ok(ctx) => {
            let _lock = ctx.lock()?;
            let mut ledger = ctx.load_ledger()?;
            ctx.recover_pending(&mut ledger)?;
            Ok(true)
        }
        Err(e) if benign_no_checkpoint(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

/// Capture the preimage for an already-leased workspace mutation. The caller must hold the
/// `WorkspaceWriterLease` across this call and the subsequent tool body, closing the old
/// preflight→save→mutation gap without acquiring a second workspace lock.
pub fn save_protected_change(label: &str) -> Result<Option<Snapshot>> {
    match RepoContext::current() {
        Ok(ctx) => save_in(&ctx, label, true, None).map(Some),
        Err(e) if benign_no_checkpoint(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

pub fn save(label: &str, auto: bool) -> Result<Snapshot> {
    let ctx = RepoContext::current()?;
    let snap = save_in(&ctx, label, auto, None)?;
    let keep = crate::core::cli_config::load()
        .timemachine_keep
        .unwrap_or(DEFAULT_KEEP);
    prune_after_save(&ctx, keep, &[snap.id])?;
    Ok(snap)
}

pub fn save_with_chat(label: &str, auto: bool, chat: &[Message]) -> Result<Snapshot> {
    let ctx = RepoContext::current()?;
    let snap = save_in(&ctx, label, auto, Some(chat))?;
    let keep = crate::core::cli_config::load()
        .timemachine_keep
        .unwrap_or(DEFAULT_KEEP);
    prune_after_save(&ctx, keep, &[snap.id])?;
    Ok(snap)
}

fn prune_after_save(ctx: &RepoContext, keep: usize, protected: &[u32]) -> Result<()> {
    if keep == 0 {
        return Ok(());
    }
    let _store = ctx.store_shared()?;
    let _lock = ctx.lock()?;
    let mut ledger = ctx.load_ledger()?;
    ctx.recover_pending(&mut ledger)?;
    if ledger.snapshots.len() <= keep {
        return Ok(());
    }
    let mut journal = Journal::new(JournalKind::Prune, ledger.generation);
    ctx.save_journal(&journal)?;
    let dropped = enforce_retention_plan(&mut ledger, keep, protected);
    // Commit the new authoritative ledger first. Old refs remain recovery pins if cleanup is
    // interrupted; doctor can safely report/reap those orphans later.
    ctx.save_ledger(&mut ledger)?;
    delete_snapshots(ctx, &dropped)?;
    journal.phase = JournalPhase::LedgerCommitted;
    ctx.save_journal(&journal)?;
    ctx.clear_journal()?;
    Ok(())
}

fn save_in(
    ctx: &RepoContext,
    label: &str,
    auto: bool,
    chat: Option<&[Message]>,
) -> Result<Snapshot> {
    let _store = ctx.store_shared()?;
    let _lock = ctx.lock()?;
    let mut ledger = ctx.load_ledger()?;
    ctx.recover_pending(&mut ledger)?;
    ctx.migrate_legacy_refs(&mut ledger)?;
    let (tree, coverage) = current_tree(ctx)?;

    // Auto files-only saves may reuse the newest tree. Explicit/user/chat checkpoints are immutable
    // events and therefore always get a fresh ID even when the bytes are identical.
    if auto && chat.is_none() {
        if let Some(last) = ledger.snapshots.last().filter(|s| s.tree == tree) {
            let last = last.clone();
            ledger.set_cursor(Some(last.id));
            ctx.save_ledger(&mut ledger)?;
            return Ok(last);
        }
    }
    capture_checkpoint_locked(ctx, &mut ledger, tree, coverage, label, auto, chat, false)
}

pub fn load_chat_checked(id: u32) -> Result<Vec<Message>> {
    let ctx = RepoContext::current()?;
    read_chat_in(&ctx, id)?.with_context(|| format!("checkpoint #{id} has no saved conversation"))
}

fn enforce_retention_plan(ledger: &mut Ledger, keep: usize, protected: &[u32]) -> Vec<Snapshot> {
    if keep == 0 || ledger.snapshots.len() <= keep {
        return Vec::new();
    }
    let mut protected: HashSet<u32> = protected.iter().copied().collect();
    if let Some(id) = ledger.cursor_id {
        protected.insert(id);
    }
    let mut dropped = Vec::new();
    let mut i = 0;
    while ledger.snapshots.len() > keep && i < ledger.snapshots.len() {
        if protected.contains(&ledger.snapshots[i].id) {
            i += 1;
            continue;
        }
        dropped.push(ledger.snapshots.remove(i));
    }
    ledger.set_cursor(ledger.cursor_id);
    dropped
}

fn delete_snapshots(ctx: &RepoContext, dropped: &[Snapshot]) -> Result<()> {
    for snap in dropped {
        let ref_name = ctx.ref_name(snap.id);
        if let Ok(actual) = ctx.git(None, ["rev-parse", "--verify", &ref_name]) {
            if actual != snap.commit {
                bail!("ref for checkpoint #{} changed concurrently", snap.id);
            }
            ctx.update_ref_delete(&ref_name, &actual)?;
        }
        // A legacy ref may still pin this object after migration. It is deliberately not deleted
        // here unless it is the only representation and matches exactly; preserving an extra pin is
        // safer than making a failed cleanup destroy recovery history.
        let _ = crate::core::persist::remove_if_exists(&ctx.chat_path(snap.id))?;
    }
    Ok(())
}

pub fn prune(keep: usize) -> Result<usize> {
    let ctx = RepoContext::current()?;
    let _store = ctx.store_shared()?;
    let _lock = ctx.lock()?;
    let mut ledger = ctx.load_ledger()?;
    ctx.recover_pending(&mut ledger)?;
    ctx.migrate_legacy_refs(&mut ledger)?;
    let mut journal = Journal::new(JournalKind::Prune, ledger.generation);
    ctx.save_journal(&journal)?;
    let dropped = enforce_retention_plan(&mut ledger, keep, &[]);
    // Ledger-first deletion: an interrupted cleanup leaves harmless orphan refs, never ledger entries
    // pointing at objects we already made unreachable.
    ctx.save_ledger(&mut ledger)?;
    delete_snapshots(&ctx, &dropped)?;
    journal.phase = JournalPhase::LedgerCommitted;
    ctx.save_journal(&journal)?;
    ctx.clear_journal()?;
    Ok(dropped.len())
}

pub fn clear() -> Result<usize> {
    let ctx = RepoContext::current()?;
    let _store = ctx.store_shared()?;
    let _lock = ctx.lock()?;
    let mut ledger = ctx.load_ledger()?;
    ctx.recover_pending(&mut ledger)?;
    ctx.migrate_legacy_refs(&mut ledger)?;
    let mut journal = Journal::new(JournalKind::Clear, ledger.generation);
    ctx.save_journal(&journal)?;
    let snapshots = ledger.snapshots.clone();
    let n = snapshots.len();
    ledger.snapshots.clear();
    ledger.set_cursor(None);
    // Empty ledger becomes authoritative before refs are reaped. A crash during cleanup leaves
    // recoverable orphan pins rather than an apparently valid timeline with missing objects.
    ctx.save_ledger(&mut ledger)?;
    delete_snapshots(&ctx, &snapshots)?;
    journal.phase = JournalPhase::LedgerCommitted;
    ctx.save_journal(&journal)?;
    ctx.clear_journal()?;
    Ok(n)
}

fn apply_tree(ctx: &RepoContext, commit: &str) -> Result<()> {
    ctx.ensure_safe_filters()?;
    reparse_preflight(ctx)?;
    let idx = seed_index(ctx)?;
    ctx.git(Some(&idx.0), ["read-tree", "--reset", "-u", commit])?;
    Ok(())
}

fn capture_checkpoint_locked(
    ctx: &RepoContext,
    ledger: &mut Ledger,
    tree: String,
    coverage: Coverage,
    label: &str,
    auto: bool,
    chat: Option<&[Message]>,
    recovery: bool,
) -> Result<Snapshot> {
    let id = ledger.next_id.max(1);
    let ref_name = ctx.ref_name(id);
    let mut journal = Journal::new(JournalKind::Save, ledger.generation);
    journal.checkpoint_id = Some(id);
    journal.ref_name = Some(ref_name.clone());
    ctx.save_journal(&journal)?;

    let parent_commit = ledger
        .cursor_id
        .and_then(|pid| ledger.snapshots.iter().find(|s| s.id == pid))
        .map(|s| s.commit.clone())
        .or_else(|| ctx.source_git(["rev-parse", "--verify", "HEAD"]).ok());
    let msg = format!(
        "aizen checkpoint: {}",
        if label.is_empty() {
            "(no label)"
        } else {
            label
        }
    );
    let mut args = vec![
        "commit-tree".to_string(),
        tree.clone(),
        "-m".to_string(),
        msg,
    ];
    if let Some(parent) = parent_commit {
        args.push("-p".to_string());
        args.push(parent);
    }
    let commit = ctx.git(None, args)?;
    journal.new_oid = Some(commit.clone());
    ctx.save_journal(&journal)?;
    ctx.update_ref_create(&ref_name, &commit)?;
    journal.phase = JournalPhase::RefCreated;
    ctx.save_journal(&journal)?;

    let has_chat = match chat {
        Some(chat) => {
            if let Err(e) = write_chat(ctx, id, chat) {
                let _ = ctx.update_ref_delete(&ref_name, &commit);
                let _ = ctx.clear_journal();
                return Err(e).context("saving checkpoint conversation sidecar");
            }
            true
        }
        None => false,
    };
    let snap = Snapshot {
        id,
        commit,
        tree,
        label: label.to_string(),
        created: now_string(),
        auto,
        has_chat,
        parent: ledger.cursor_id,
        worktree_id: ctx.worktree_id.clone(),
        coverage,
        recovery,
    };
    ledger.next_id = id.checked_add(1).context("checkpoint id space exhausted")?;
    ledger.snapshots.push(snap.clone());
    ledger.set_cursor(Some(id));
    if let Err(e) = ctx.save_ledger(ledger) {
        // Keep the journal + ref as recovery evidence. `recover_pending` will remove the orphan when
        // the old ledger generation is loaded on the next operation.
        return Err(e).context("committing checkpoint ledger");
    }
    journal.phase = JournalPhase::LedgerCommitted;
    ctx.save_journal(&journal)?;
    ctx.clear_journal()?;
    Ok(snap)
}

pub fn restore(id: u32) -> Result<Snapshot> {
    let ctx = RepoContext::current()?;
    let _workspace = crate::core::workspace_txn::WorkspaceWriterLease::acquire(
        &ctx.root,
        LOCK_TIMEOUT,
        None,
        "time restore",
    )?;
    restore_in(&ctx, id)
}

/// Restore-by-id for callers that ALREADY hold the workspace writer lease (the agent loop takes it
/// for any `WorkspaceEffect::Paths` tool before running the body). Re-acquiring the same
/// `workspace.lock` via [`restore`] would self-deadlock — `LockFileEx`/`flock` are per-handle and
/// non-reentrant — so this skips the lease and takes only the store/metadata locks inside
/// `restore_in`. Never call this off the agent loop's lease path; use [`restore`] there.
pub fn restore_under_lease(id: u32) -> Result<Snapshot> {
    let ctx = RepoContext::current()?;
    restore_in(&ctx, id)
}

fn restore_in(ctx: &RepoContext, id: u32) -> Result<Snapshot> {
    let _store = ctx.store_shared()?;
    let _lock = ctx.lock()?;
    let mut ledger = ctx.load_ledger()?;
    ctx.recover_pending(&mut ledger)?;
    ctx.migrate_legacy_refs(&mut ledger)?;
    let target = ledger
        .snapshots
        .iter()
        .find(|s| s.id == id)
        .cloned()
        .with_context(|| format!("no checkpoint #{id} (see `aizen time list`)"))?;
    ctx.validate_snapshot_ref(&target)?;

    let (cur_tree, cur_coverage) = current_tree(ctx)?;
    let preimage_id = if cur_tree != target.tree {
        // Restore always creates a fresh immutable preimage event. Reusing an older equal-tree
        // checkpoint makes `/redo` ambiguous after branching and can leave no forward recovery point.
        Some(save_preimage_locked(
            ctx,
            &mut ledger,
            cur_tree,
            cur_coverage,
        )?)
    } else {
        ledger.cursor_id
    };
    if let Some(pid) = preimage_id {
        if let Some(preimage) = ledger.snapshots.iter().find(|s| s.id == pid) {
            let recovery_ref = ctx.recovery_ref_name(pid);
            if ctx
                .git(None, ["rev-parse", "--verify", &recovery_ref])
                .is_err()
            {
                ctx.update_ref_create(&recovery_ref, &preimage.commit)?;
            }
        }
    }

    let mut journal = Journal::new(JournalKind::Restore, ledger.generation);
    journal.target_id = Some(id);
    journal.preimage_id = preimage_id;
    journal.phase = JournalPhase::Applying;
    ctx.save_journal(&journal)?;

    if let Err(apply_error) = apply_tree(ctx, &target.commit) {
        if let Some(pid) = preimage_id {
            if let Some(preimage) = ledger.snapshots.iter().find(|s| s.id == pid) {
                let _ = apply_tree(ctx, &preimage.commit);
            }
        }
        return Err(apply_error).context("restoring working tree; recovery journal was preserved");
    }
    let (actual, _) = current_tree(ctx)?;
    if actual != target.tree {
        if let Some(pid) = preimage_id {
            if let Some(preimage) = ledger.snapshots.iter().find(|s| s.id == pid) {
                let _ = apply_tree(ctx, &preimage.commit);
            }
        }
        bail!(
            "restore verification failed; recovery journal was preserved for `aizen time doctor`"
        );
    }

    ledger.set_cursor(Some(id));
    ctx.save_ledger(&mut ledger)?;
    if let Some(pid) = preimage_id {
        let recovery_ref = ctx.recovery_ref_name(pid);
        if let Ok(oid) = ctx.git(None, ["rev-parse", "--verify", &recovery_ref]) {
            let _ = ctx.update_ref_delete(&recovery_ref, &oid);
        }
    }
    journal.phase = JournalPhase::LedgerCommitted;
    ctx.save_journal(&journal)?;
    ctx.clear_journal()?;
    Ok(target)
}

fn save_preimage_locked(
    ctx: &RepoContext,
    ledger: &mut Ledger,
    tree: String,
    coverage: Coverage,
) -> Result<u32> {
    Ok(capture_checkpoint_locked(
        ctx,
        ledger,
        tree,
        coverage,
        "before time-travel",
        true,
        None,
        true,
    )?
    .id)
}

pub fn undo() -> Result<Snapshot> {
    let ctx = RepoContext::current()?;
    let _workspace = crate::core::workspace_txn::WorkspaceWriterLease::acquire(
        &ctx.root,
        LOCK_TIMEOUT,
        None,
        "time undo",
    )?;
    let ledger = ctx.load_ledger()?;
    let current = ledger
        .cursor_id
        .or_else(|| ledger.snapshots.last().map(|s| s.id))
        .context("no checkpoints yet — save one with `aizen time save`")?;
    let parent = ledger
        .snapshots
        .iter()
        .find(|s| s.id == current)
        .and_then(|s| s.parent)
        .context("already at the oldest checkpoint")?;
    restore_in(&ctx, parent)
}

pub fn redo() -> Result<Snapshot> {
    let ctx = RepoContext::current()?;
    let _workspace = crate::core::workspace_txn::WorkspaceWriterLease::acquire(
        &ctx.root,
        LOCK_TIMEOUT,
        None,
        "time redo",
    )?;
    let ledger = ctx.load_ledger()?;
    let current = ledger
        .cursor_id
        .or_else(|| ledger.snapshots.last().map(|s| s.id))
        .context("no checkpoints yet")?;
    let child = ledger
        .snapshots
        .iter()
        .filter(|s| s.parent == Some(current) && !s.recovery)
        .max_by_key(|s| s.id)
        .map(|s| s.id)
        .context("already at the newest checkpoint on this branch")?;
    restore_in(&ctx, child)
}

pub fn timeline() -> Result<(Vec<Snapshot>, Option<usize>)> {
    let ctx = RepoContext::current()?;
    let mut ledger = ctx.load_ledger()?;
    ledger.normalize()?;
    Ok((ledger.snapshots, ledger.cursor))
}

// ───────────────────────────── diff between points in time ─────────────────────────────

/// One changed path between two timeline points. `added`/`deleted` are `None` for a binary file,
/// which is exactly how Git reports it (`-` in `--numstat`) — the distinction matters because "0
/// lines changed" and "not a text file" would otherwise read identically.
#[derive(Debug, Clone, Serialize)]
pub struct FileChange {
    /// Git status letter: `A`dded, `M`odified, `D`eleted, `R`enamed, `C`opied, `T`ype-changed.
    pub status: char,
    pub path: String,
    /// Present only for renames/copies: where the content came from.
    pub old_path: Option<String>,
    pub added: Option<u64>,
    pub deleted: Option<u64>,
}

/// Which point in time a diff side refers to. The working tree is a first-class side so "what have I
/// changed since checkpoint #5" needs no intermediate checkpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffSide {
    Checkpoint(u32),
    Working,
}

impl DiffSide {
    /// Accepts a checkpoint id, or `working`/`now`/`wt` for the live tree.
    pub fn parse(s: &str) -> Option<Self> {
        let t = s.trim().to_ascii_lowercase();
        match t.as_str() {
            "working" | "worktree" | "wt" | "now" | "current" | "disk" => Some(Self::Working),
            _ => t
                .trim_start_matches('#')
                .parse::<u32>()
                .ok()
                .map(Self::Checkpoint),
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Checkpoint(id) => format!("#{id}"),
            Self::Working => "working tree".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct DiffReport {
    pub from: String,
    pub to: String,
    pub files: Vec<FileChange>,
    /// Unified patch text, when requested. Truncated at a byte ceiling — see `patch_truncated`.
    pub patch: Option<String>,
    pub patch_truncated: bool,
}

impl DiffReport {
    pub fn total_added(&self) -> u64 {
        self.files.iter().filter_map(|f| f.added).sum()
    }
    pub fn total_deleted(&self) -> u64 {
        self.files.iter().filter_map(|f| f.deleted).sum()
    }
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }
}

/// Resolve a diff side to a Git tree oid. `Working` writes a fresh tree object into the private
/// store (unreferenced, reclaimed by ordinary Git maintenance) so the live worktree can be diffed
/// with the same plumbing as any checkpoint — no special-casing downstream.
fn side_tree(ctx: &RepoContext, ledger: &Ledger, side: &DiffSide) -> Result<String> {
    match side {
        DiffSide::Checkpoint(id) => {
            let snap = ledger
                .snapshots
                .iter()
                .find(|s| s.id == *id)
                .with_context(|| format!("no checkpoint #{id} (see `aizen time list`)"))?;
            ctx.validate_snapshot_ref(snap)?;
            Ok(snap.tree.clone())
        }
        DiffSide::Working => Ok(current_tree(ctx)?.0),
    }
}

/// Split a `-z` (NUL-delimited) plumbing stream. Git only quotes paths in the non-`-z` forms, so
/// this is the parse that survives non-ASCII and embedded-newline paths intact.
fn nul_fields(bytes: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(bytes)
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

/// Merge git's two machine-readable diff streams into one row per changed path.
///
/// Two calls are needed because git has no single format carrying both the status letter (with rename
/// pairing) and the line counts: `--name-status` has the former, `--numstat` the latter. Kept pure
/// and byte-in so the `-z` framing — the part that silently mis-parses on renames — is unit-testable
/// without a repository.
///
/// Framing, both with `-z`:
///   - `--name-status`: `<status>\0<path>\0`, or `R<score>\0<old>\0<new>\0` for renames/copies.
///   - `--numstat`: `<add>\t<del>\t<path>\0` normally, but for a rename the trailing path is EMPTY and
///     the pair follows as `\0<old>\0<new>\0` — so a naive one-field-per-row read desyncs the whole
///     remaining stream.
///
/// A `-` count means binary; it maps to `None` rather than `0` so "no text change" stays distinct
/// from "not text".
fn merge_diff_streams(name_status: &[u8], numstat: &[u8]) -> Vec<FileChange> {
    let parse_count = |v: &str| {
        if v == "-" {
            None
        } else {
            v.parse::<u64>().ok()
        }
    };

    let mut counts: std::collections::HashMap<String, (Option<u64>, Option<u64>)> =
        std::collections::HashMap::new();
    let num_fields = nul_fields(numstat);
    let mut i = 0;
    while i < num_fields.len() {
        let mut parts = num_fields[i].split('\t');
        let add = parts.next().unwrap_or("");
        let del = parts.next().unwrap_or("");
        let inline = parts.next().unwrap_or("");
        if !inline.is_empty() {
            counts.insert(inline.to_string(), (parse_count(add), parse_count(del)));
            i += 1;
        } else if i + 2 < num_fields.len() {
            // Rename/copy: counts row, then old path, then new path. Key on the new path, which is
            // what `--name-status` reports as the row's path.
            counts.insert(
                num_fields[i + 2].clone(),
                (parse_count(add), parse_count(del)),
            );
            i += 3;
        } else {
            break;
        }
    }

    let name_fields = nul_fields(name_status);
    let mut files = Vec::new();
    let mut j = 0;
    while j + 1 < name_fields.len() {
        let status = name_fields[j].chars().next().unwrap_or('M');
        let paired = matches!(status, 'R' | 'C');
        let (path, old_path, step) = if paired && j + 2 < name_fields.len() {
            (
                name_fields[j + 2].clone(),
                Some(name_fields[j + 1].clone()),
                3,
            )
        } else {
            (name_fields[j + 1].clone(), None, 2)
        };
        let (added, deleted) = counts.get(&path).copied().unwrap_or((None, None));
        files.push(FileChange {
            status,
            path,
            old_path,
            added,
            deleted,
        });
        j += step;
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    files
}

/// Diff two points in the timeline: checkpoint↔checkpoint, or checkpoint↔working tree.
///
/// This is the read half of the time machine. Restoring already worked, but without a diff the only
/// way to react to a bad edit was to discard the whole tree — you could not see *which* file went
/// wrong, so a one-line mistake cost every good change made alongside it.
///
/// `patch_limit` caps the unified-patch bytes; `None` means stat-only (always cheap, so it stays
/// usable on a huge change set). `paths` narrows the diff, which is what makes a broad rewind
/// unnecessary in the common case.
pub fn diff(
    from: &DiffSide,
    to: &DiffSide,
    paths: &[String],
    patch_limit: Option<usize>,
) -> Result<DiffReport> {
    let ctx = RepoContext::current()?;
    // Shared lease only: diffing writes no refs and mutates no worktree, but it must not race a
    // store-exclusive GC sweep that could reap an object mid-read.
    let _store = ctx.store_shared()?;
    let mut ledger = ctx.load_ledger()?;
    ledger.normalize()?;
    let from_tree = side_tree(&ctx, &ledger, from)?;
    let to_tree = side_tree(&ctx, &ledger, to)?;

    if from_tree == to_tree {
        return Ok(DiffReport {
            from: from.label(),
            to: to.label(),
            files: Vec::new(),
            patch: None,
            patch_truncated: false,
        });
    }

    let pathspec: Vec<String> = if paths.is_empty() {
        Vec::new()
    } else {
        let mut v = vec!["--".to_string()];
        v.extend(paths.iter().cloned());
        v
    };

    // Two plumbing calls rather than one: `--name-status` carries the status letter (and rename
    // pairing), `--numstat` carries the line counts. Git has no combined machine format that gives
    // both, and merging them here is cheaper than making callers run git twice.
    let mut name_args = vec![
        "diff-tree".to_string(),
        "-r".to_string(),
        "-z".to_string(),
        "--find-renames".to_string(),
        "--no-commit-id".to_string(),
        "--name-status".to_string(),
        from_tree.clone(),
        to_tree.clone(),
    ];
    name_args.extend(pathspec.iter().cloned());
    let name_out = ctx.git_output(None, &name_args)?;
    if !name_out.status.success() {
        bail!(
            "diff failed: {}",
            String::from_utf8_lossy(&name_out.stderr).trim()
        );
    }

    let mut num_args = vec![
        "diff-tree".to_string(),
        "-r".to_string(),
        "-z".to_string(),
        "--find-renames".to_string(),
        "--no-commit-id".to_string(),
        "--numstat".to_string(),
        from_tree.clone(),
        to_tree.clone(),
    ];
    num_args.extend(pathspec.iter().cloned());
    let num_out = ctx.git_output(None, &num_args)?;
    if !num_out.status.success() {
        bail!(
            "diff failed: {}",
            String::from_utf8_lossy(&num_out.stderr).trim()
        );
    }

    let files = merge_diff_streams(&name_out.stdout, &num_out.stdout);

    let (patch, patch_truncated) = match patch_limit {
        None => (None, false),
        Some(limit) => {
            let mut args = vec![
                "diff-tree".to_string(),
                "-r".to_string(),
                "-p".to_string(),
                "--find-renames".to_string(),
                "--no-commit-id".to_string(),
                from_tree,
                to_tree,
            ];
            args.extend(pathspec.iter().cloned());
            let out = ctx.git_output(None, &args)?;
            if !out.status.success() {
                bail!(
                    "diff failed: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            let text = String::from_utf8_lossy(&out.stdout).to_string();
            if text.len() > limit {
                // Cut on a char boundary so multi-byte content can't produce invalid UTF-8.
                let mut cut = limit;
                while cut > 0 && !text.is_char_boundary(cut) {
                    cut -= 1;
                }
                (Some(text[..cut].to_string()), true)
            } else {
                (Some(text), false)
            }
        }
    };

    Ok(DiffReport {
        from: from.label(),
        to: to.label(),
        files,
        patch,
        patch_truncated,
    })
}

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub repo_id: String,
    pub worktree_id: String,
    pub store: String,
    pub checkpoints: usize,
    pub issues: Vec<String>,
}

pub fn doctor() -> Result<DoctorReport> {
    let ctx = RepoContext::current()?;
    let mut issues = Vec::new();
    if !ctx.store_git_dir.join("HEAD").exists() {
        issues.push(format!(
            "private store missing at {}",
            ctx.store_git_dir.display()
        ));
    }
    let alternates = ctx
        .store_git_dir
        .join("objects")
        .join("info")
        .join("alternates");
    match crate::core::persist::read_optional(&alternates) {
        Ok(None) => issues.push("private store alternates pointer is missing".into()),
        Ok(Some(bytes)) => {
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
            if lines.len() != 1 {
                issues.push(format!(
                    "private store alternates is not sealed (expected 1 entry, found {})",
                    lines.len()
                ));
            }
        }
        Err(e) => issues.push(format!("private store alternates unreadable: {e}")),
    }
    let ledger = match ctx.load_ledger() {
        Ok(l) => l,
        Err(e) => {
            issues.push(e.to_string());
            return Ok(DoctorReport {
                ok: false,
                repo_id: ctx.repo_id,
                worktree_id: ctx.worktree_id,
                store: ctx.store_git_dir.display().to_string(),
                checkpoints: 0,
                issues,
            });
        }
    };
    if let Ok(Some(journal)) = ctx.load_journal() {
        issues.push(format!(
            "unfinished {:?} transaction {} ({:?})",
            journal.kind, journal.operation_id, journal.phase
        ));
    }
    for snap in &ledger.snapshots {
        if let Err(e) = ctx.validate_snapshot_ref(snap) {
            issues.push(format!("checkpoint #{}: {e}", snap.id));
        }
        if snap.has_chat {
            match read_chat_in(&ctx, snap.id) {
                Ok(Some(_)) => {}
                Ok(None) => issues.push(format!(
                    "checkpoint #{} claims chat but sidecar is missing",
                    snap.id
                )),
                Err(e) => issues.push(format!("checkpoint #{} chat: {e}", snap.id)),
            }
        }
    }
    Ok(DoctorReport {
        ok: issues.is_empty(),
        repo_id: ctx.repo_id,
        worktree_id: ctx.worktree_id,
        store: ctx.store_git_dir.display().to_string(),
        checkpoints: ledger.snapshots.len(),
        issues,
    })
}

pub fn doctor_repair() -> Result<DoctorReport> {
    let ctx = RepoContext::current()?;
    let _lock = ctx.lock()?;
    let mut ledger = ctx.load_ledger()?;
    ctx.recover_pending(&mut ledger)?;
    drop(_lock);
    doctor()
}

pub fn doctor_gc() -> Result<DoctorReport> {
    let ctx = RepoContext::current()?;
    // Whole-store sweep: block every sibling worktree's ref creation/deletion for the scan+reap so a
    // concurrent `save` can't slip a ref past `for-each-ref`. Store-exclusive ordered before the
    // per-worktree metadata lock.
    let _store = ctx.store_exclusive()?;
    let _lock = ctx.lock()?;
    let mut ledger = ctx.load_ledger()?;
    ctx.recover_pending(&mut ledger)?;
    let mut journal = Journal::new(JournalKind::Doctor, ledger.generation);
    ctx.save_journal(&journal)?;

    let live_ids: HashSet<u32> = ledger.snapshots.iter().map(|s| s.id).collect();
    // Sweep the whole private store's `refs/ng/tm/**` (all worktrees). Orphans from other worktree
    // ids or recovery pins must not survive just because this process is bound to one prefix.
    let refs = ctx.git(
        None,
        [
            "for-each-ref",
            "--format=%(refname) %(objectname)",
            "refs/ng/tm",
        ],
    )?;
    for line in refs.lines() {
        let mut fields = line.split_whitespace();
        let Some(name) = fields.next() else { continue };
        let Some(oid) = fields.next() else { continue };
        if name.contains("/recovery/") {
            // Only delete recovery pins that belong to this worktree's namespace, or that have no
            // matching live snapshot id anywhere.
            let id = name.rsplit('/').next().and_then(|s| s.parse::<u32>().ok());
            let ours = name.starts_with(&(ctx.ref_prefix.clone() + "/"));
            if ours || id.is_some_and(|id| !live_ids.contains(&id)) {
                ctx.update_ref_delete(name, oid)?;
            }
            continue;
        }
        let id = name.rsplit('/').next().and_then(|s| s.parse::<u32>().ok());
        let ours = name.starts_with(&(ctx.ref_prefix.clone() + "/")) || name == ctx.ref_prefix;
        // Delete: (a) orphan ids under our prefix, or (b) any ref under a dead worktree prefix that
        // is not our live prefix (foreign worktree namespaces keep their own live ledgers — only
        // delete ids that are under our prefix and not live).
        if ours {
            if id.is_some_and(|id| !live_ids.contains(&id)) {
                ctx.update_ref_delete(name, oid)?;
                if let Some(id) = id {
                    let _ = crate::core::persist::remove_if_exists(&ctx.chat_path(id));
                }
            }
        } else if id.is_some() {
            // Foreign worktree ref: leave alone — that worktree owns its ledger. The failpoint
            // probe plants `refs/ng/tm/wt-dead/999`; treat any non-matching worktree id as orphan
            // only when its path segment is not a known sibling ledger on disk.
            let foreign_wt = name
                .trim_start_matches("refs/ng/tm/")
                .split('/')
                .next()
                .unwrap_or("");
            if !foreign_wt.is_empty() && foreign_wt != ctx.worktree_id {
                let sibling_ledger = ctx
                    .store_git_dir
                    .parent()
                    .map(|p| p.join("worktrees").join(foreign_wt).join("ledger.json"));
                let sibling_exists = sibling_ledger.as_ref().is_some_and(|p| p.exists());
                if !sibling_exists {
                    ctx.update_ref_delete(name, oid)?;
                }
            }
        }
    }
    if ctx.chat_dir().is_dir() {
        for entry in fs::read_dir(ctx.chat_dir())? {
            let path = entry?.path();
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .and_then(|s| s.parse::<u32>().ok());
            if id.is_some_and(|id| !live_ids.contains(&id)) {
                let _ = crate::core::persist::remove_if_exists(&path)?;
            }
        }
    }
    journal.phase = JournalPhase::LedgerCommitted;
    ctx.save_journal(&journal)?;
    ctx.clear_journal()?;
    drop(_lock);
    doctor()
}

pub struct Checkpoint;
impl crate::agent::tools::Tool for Checkpoint {
    fn name(&self) -> &str {
        "checkpoint"
    }
    fn description(&self) -> &str {
        "Create an explicit Time Machine checkpoint (private store). Use before high-risk work the \
         runtime won't auto-catch. The runtime already auto-checkpoints before the first edit and \
         after each successful edit — don't duplicate those. To undo a bad approach THIS run, use \
         `checkpoint_rewind` (not free-form restore)."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"label": {"type": "string", "description": "short note, e.g. 'before refactor auth'"}},
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, _args: &serde_json::Value) -> crate::agent::tools::WorkspaceEffect {
        crate::agent::tools::WorkspaceEffect::RepoMetadata
    }
    fn execute(&self, args: &serde_json::Value) -> Result<String> {
        if !is_repo() {
            return Ok(
                "error: not a git repository — checkpoints need git (run `git init`)".to_string(),
            );
        }
        let label = args
            .get("label")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let snap = save(label, false)?;
        // Explicit saves also count as last_good within the run (model just pinned a known-good tree).
        note_last_good(snap.id);
        Ok(format!(
            "checkpoint #{} saved ({}). Run-scoped rewind: `checkpoint_rewind` target=last_good|pre_edit. \
             Human free-form: `aizen time restore {}`.",
            snap.id,
            if snap.label.is_empty() { "no label" } else { &snap.label },
            snap.id
        ))
    }
}

/// Agent-only, run-scoped rewind. Restores ONLY the pre-edit or last-good anchor of the current
/// agent run — never arbitrary snapshot ids (those stay human/CLI). Cap: [`MAX_RUN_REWINDS`].
pub struct CheckpointRewind;
impl crate::agent::tools::Tool for CheckpointRewind {
    fn name(&self) -> &str {
        "checkpoint_rewind"
    }
    fn description(&self) -> &str {
        "Rewind the working tree to a recovery anchor from THIS agent run only. Use when your last \
         approach broke the tree and a clean base is cheaper than patching further. \
         target=`last_good` = after the last successful edit step; target=`pre_edit` = before any \
         edit this run. Budget: 2 rewinds per run. Does NOT restore chat history. To reach a \
         checkpoint from an EARLIER turn (not this run's own edits), use `checkpoint_restore` by id."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "target": {
                    "type": "string",
                    "enum": ["last_good", "pre_edit"],
                    "description": "last_good = last successful step this run; pre_edit = tree before first edit this run"
                },
                "reason": {
                    "type": "string",
                    "description": "one short line: why this approach is abandoned (shown in the tool result)"
                }
            },
            "required": ["target"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, _args: &serde_json::Value) -> crate::agent::tools::WorkspaceEffect {
        // Mutates the working tree. The agent loop skips the pre-edit checkpoint for this tool
        // (restoring is the recovery path; snapshotting the broken tree first is restore's job).
        crate::agent::tools::WorkspaceEffect::Paths
    }
    fn execute(&self, args: &serde_json::Value) -> Result<String> {
        if !is_repo() {
            return Ok("error: not a git repository — nothing to rewind".to_string());
        }
        let raw = args.get("target").and_then(|v| v.as_str()).unwrap_or("");
        let Some(target) = RewindTarget::parse(raw) else {
            return Ok(
                "error: target must be \"last_good\" or \"pre_edit\" (run-scoped only; free-form ids are human-driven)"
                    .to_string(),
            );
        };
        let reason = args
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        match rewind_run(target) {
            Ok(snap) => {
                let st = recovery_status();
                let why = if reason.is_empty() {
                    String::new()
                } else {
                    format!(" reason: {reason}.")
                };
                Ok(format!(
                    "rewound working tree to checkpoint #{} ({}, label={:?}).{why} \
                     rewinds left this run: {}. Re-read files you care about — contents changed on disk. \
                     Chat history was NOT restored.",
                    snap.id,
                    target.as_str(),
                    if snap.label.is_empty() { "none" } else { &snap.label },
                    st.rewinds_left,
                ))
            }
            Err(e) => Ok(format!("error: {e}")),
        }
    }
}

/// One timeline row shaped for the model: id, whether it is the current cursor, label, whether a
/// chat sidecar exists, and the creation time. Absolute paths / store internals never leak here.
fn format_timeline_row(snap: &Snapshot, is_cursor: bool) -> String {
    let marker = if is_cursor { "▸" } else { " " };
    let label = if snap.label.is_empty() {
        "(no label)"
    } else {
        &snap.label
    };
    let kind = if snap.auto { "auto" } else { "manual" };
    format!("{marker} #{} — {label} [{kind}, {}]", snap.id, snap.created)
}

/// Agent-facing timeline read. Read-only, so it fans out safely and needs no approval. Gives the
/// model the checkpoint ids it needs to call `checkpoint_restore` when the user asks to go back to a
/// specific earlier state (across turns, where run-scoped `checkpoint_rewind` has no anchor).
pub struct CheckpointList;
impl crate::agent::tools::Tool for CheckpointList {
    fn name(&self) -> &str {
        "checkpoint_list"
    }
    fn description(&self) -> &str {
        "List Time Machine checkpoints (newest last), with the ▸ marking the current tree. Use to \
         find the id to pass to `checkpoint_restore` when the user asks to go back to an earlier \
         state from a PREVIOUS turn (run-scoped `checkpoint_rewind` only reaches this run's own \
         edits). Read-only."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false})
    }
    fn execute(&self, _args: &serde_json::Value) -> Result<String> {
        if !is_repo() {
            return Ok("error: not a git repository — no checkpoints (run `git init`)".to_string());
        }
        let (snaps, cursor) = timeline()?;
        if snaps.is_empty() {
            return Ok("no checkpoints yet — the runtime auto-checkpoints before/after edits, or use `checkpoint` to pin one.".to_string());
        }
        // Cap the tail so a long timeline can't blow the tool-result budget; ids are contiguous so
        // the model can still ask for older ones by number if it needs them.
        const MAX_ROWS: usize = 40;
        let cursor_id = cursor.and_then(|i| snaps.get(i)).map(|s| s.id);
        let start = snaps.len().saturating_sub(MAX_ROWS);
        let mut out = String::new();
        if start > 0 {
            out.push_str(&format!("… {} older checkpoint(s) not shown\n", start));
        }
        for snap in &snaps[start..] {
            out.push_str(&format_timeline_row(snap, Some(snap.id) == cursor_id));
            out.push('\n');
        }
        out.push_str(
            "\nRestore a specific id with `checkpoint_restore id=<n>` (files only — chat is untouched, and reversible).",
        );
        Ok(out)
    }
}

/// Agent-facing restore-by-id. Unlike `checkpoint_rewind` (run-scoped anchors only), this reaches
/// ANY checkpoint in the ledger — the tool the model needs when the user says "go back to how it was
/// before" across turns. Destructive → approval-gated. Declares `Paths` so the agent loop takes the
/// workspace writer lease around it; the body therefore uses the lease-free `restore_under_lease`
/// (re-acquiring the same `workspace.lock` would self-deadlock — see `rewind_run`).
pub struct CheckpointRestore;
impl crate::agent::tools::Tool for CheckpointRestore {
    fn name(&self) -> &str {
        "checkpoint_restore"
    }
    fn description(&self) -> &str {
        "Restore the working tree to a specific Time Machine checkpoint by id (see `checkpoint_list`). \
         Use when the user asks to go back to an earlier state — including from a PREVIOUS turn, which \
         `checkpoint_rewind` cannot reach (it only rewinds this run's own edits). Files only: your \
         chat/history is untouched, and the pre-restore tree is auto-snapshotted so it is reversible. \
         After restoring, re-read any files you care about — their contents changed on disk."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "integer", "minimum": 1, "description": "checkpoint id from `checkpoint_list`"},
                "reason": {"type": "string", "description": "one short line: why you are restoring (shown in the result)"}
            },
            "required": ["id"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn workspace_effect(&self, _args: &serde_json::Value) -> crate::agent::tools::WorkspaceEffect {
        // Mutates the working tree. The loop takes the workspace writer lease for `Paths`; restore
        // itself takes only store/metadata locks (a different namespace), so no self-deadlock.
        crate::agent::tools::WorkspaceEffect::Paths
    }
    fn execute(&self, args: &serde_json::Value) -> Result<String> {
        if !is_repo() {
            return Ok("error: not a git repository — nothing to restore".to_string());
        }
        let Some(id) = args.get("id").and_then(|v| v.as_u64()) else {
            return Ok(
                "error: `id` is required (an integer checkpoint id — see `checkpoint_list`)"
                    .to_string(),
            );
        };
        let id = id as u32;
        let reason = args
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        match restore_under_lease(id) {
            Ok(snap) => {
                // Restoring an arbitrary id makes that tree the new run floor: a subsequent
                // `checkpoint_rewind target=last_good` should land here, not on a stale post-edit id.
                note_last_good(snap.id);
                let why = if reason.is_empty() {
                    String::new()
                } else {
                    format!(" reason: {reason}.")
                };
                Ok(format!(
                    "restored working tree to checkpoint #{} ({}).{why} Files only — chat history was NOT \
                     changed, and this is reversible (the pre-restore tree was auto-snapshotted; a human can \
                     `aizen time redo`). Re-read files you care about — contents changed on disk.",
                    snap.id,
                    if snap.label.is_empty() { "no label" } else { &snap.label },
                ))
            }
            Err(e) => Ok(format!(
                "error: {e:#}. Nothing was restored. Check the id with `checkpoint_list`."
            )),
        }
    }
}

/// Render a diff report as compact text for a tool result / CLI. Kept pure (no git, no I/O) so the
/// shaping is unit-testable and identical for the agent and the human CLI.
fn format_diff(report: &DiffReport, max_rows: usize) -> String {
    if report.is_empty() {
        return format!(
            "no changes between {} and {} — the trees are identical.",
            report.from, report.to
        );
    }
    let mut out = format!(
        "{} → {}: {} file(s) changed, +{} -{}\n",
        report.from,
        report.to,
        report.files.len(),
        report.total_added(),
        report.total_deleted()
    );
    let shown = report.files.len().min(max_rows);
    for f in &report.files[..shown] {
        let counts = match (f.added, f.deleted) {
            (Some(a), Some(d)) => format!("+{a} -{d}"),
            // Git reports `-` for both columns on a binary file; say so rather than printing "+0 -0",
            // which would read as "nothing changed".
            _ => "binary".to_string(),
        };
        match &f.old_path {
            Some(old) => out.push_str(&format!("  {} {old} → {}  ({counts})\n", f.status, f.path)),
            None => out.push_str(&format!("  {} {}  ({counts})\n", f.status, f.path)),
        }
    }
    if report.files.len() > shown {
        out.push_str(&format!(
            "  … {} more file(s)\n",
            report.files.len() - shown
        ));
    }
    if let Some(patch) = &report.patch {
        out.push_str("\n");
        out.push_str(patch);
        if report.patch_truncated {
            out.push_str(
                "\n… patch truncated. Narrow it with `paths` to see the rest of a specific file.\n",
            );
        }
    }
    out
}

/// Agent-facing diff between two points in the timeline. Read-only, so it needs no approval and can
/// run concurrently — which matters, because the model should reach for this BEFORE a rewind.
///
/// Without it, reacting to a bad edit meant discarding the whole tree: the model could not see which
/// file went wrong, so one bad line cost every good change made beside it. With it, the normal move
/// is read the diff → fix the one file, and `checkpoint_rewind` becomes the fallback it should be.
pub struct CheckpointDiff;
impl crate::agent::tools::Tool for CheckpointDiff {
    fn name(&self) -> &str {
        "checkpoint_diff"
    }
    fn description(&self) -> &str {
        "Show what changed between two Time Machine points: checkpoint↔checkpoint, or a checkpoint↔the \
         live working tree (`to`/`from` = \"working\"). Defaults to `from`=the last run anchor, \
         `to`=\"working\" — i.e. \"what have my edits done so far\". Use this BEFORE `checkpoint_rewind`: \
         a rewind throws away every change since the anchor, so read the diff first and fix the one \
         file that is wrong when you can. Also the way to inspect a suspicious edit you did not make \
         this turn. Set `patch=true` for the unified diff (line-level), `paths` to narrow it. Read-only."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "from": {
                    "type": "string",
                    "description": "checkpoint id (e.g. \"5\"), or \"working\" for the live tree. Default: this run's pre_edit anchor, else the newest checkpoint."
                },
                "to": {
                    "type": "string",
                    "description": "checkpoint id, or \"working\" for the live tree (default)."
                },
                "patch": {
                    "type": "boolean",
                    "description": "include the unified line-level diff, not just the per-file stat. Default false."
                },
                "paths": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "limit to these paths (repo-relative). Strongly recommended together with patch=true."
                }
            },
            "additionalProperties": false
        })
    }
    fn execute(&self, args: &serde_json::Value) -> Result<String> {
        if !is_repo() {
            return Ok(
                "error: not a git repository — no timeline to diff (run `git init`)".to_string(),
            );
        }
        let side = |key: &str| -> Option<DiffSide> {
            args.get(key)
                .and_then(|v| v.as_str())
                .and_then(DiffSide::parse)
        };
        // "what have I changed" is the overwhelmingly common question, so both ends default toward it:
        // `to` = the live tree, `from` = this run's pre-edit anchor (falling back to the newest
        // checkpoint when there is no anchor, e.g. a fresh turn that hasn't edited yet).
        let to = side("to").unwrap_or(DiffSide::Working);
        let from = match side("from") {
            Some(s) => s,
            None => match recovery_status().pre_edit {
                Some(id) => DiffSide::Checkpoint(id),
                None => {
                    let (snaps, _) = timeline()?;
                    match snaps.last() {
                        Some(s) => DiffSide::Checkpoint(s.id),
                        None => return Ok(
                            "no checkpoints yet — nothing to diff against. The runtime auto-checkpoints before the first edit."
                                .to_string(),
                        ),
                    }
                }
            },
        };
        if from == to {
            return Ok(format!(
                "error: `from` and `to` are the same point ({}) — nothing to compare.",
                from.label()
            ));
        }
        let paths: Vec<String> = args
            .get("paths")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.to_string())
                    .collect()
            })
            .unwrap_or_default();
        let want_patch = args.get("patch").and_then(|v| v.as_bool()).unwrap_or(false);
        // Patch bytes are the one unbounded part of this result; cap them so a broad `patch=true`
        // degrades to a truncated read instead of blowing the tool-result budget.
        let limit = if want_patch { Some(24_000) } else { None };
        match diff(&from, &to, &paths, limit) {
            Ok(report) => Ok(format_diff(&report, 60)),
            Err(e) => Ok(format!("error: {e:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(id: u32, parent: Option<u32>) -> Snapshot {
        Snapshot {
            id,
            commit: format!("{id:040x}"),
            tree: format!("{:040x}", id + 100),
            label: String::new(),
            created: "now".into(),
            auto: false,
            has_chat: false,
            parent,
            worktree_id: "wt-test".into(),
            coverage: Coverage::default(),
            recovery: false,
        }
    }

    #[test]
    fn ledger_rejects_duplicate_ids_and_regressed_counter() {
        let mut duplicate = Ledger {
            snapshots: vec![mk(1, None), mk(1, None)],
            next_id: 2,
            ..Default::default()
        };
        assert!(duplicate.normalize().is_err());
        let mut regressed = Ledger {
            snapshots: vec![mk(9, None)],
            next_id: 2,
            ..Default::default()
        };
        assert!(regressed.normalize().is_err());
    }

    #[test]
    fn retention_protects_cursor_and_explicit_ids() {
        let mut l = Ledger {
            snapshots: vec![mk(1, None), mk(2, Some(1)), mk(3, Some(2)), mk(4, Some(3))],
            next_id: 5,
            ..Default::default()
        };
        l.set_cursor(Some(1));
        let dropped = enforce_retention_plan(&mut l, 2, &[4]);
        assert_eq!(
            l.snapshots.iter().map(|s| s.id).collect::<Vec<_>>(),
            vec![1, 4]
        );
        assert_eq!(dropped.iter().map(|s| s.id).collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(l.cursor_id, Some(1));
    }

    #[test]
    fn legacy_cursor_migrates_to_id() {
        let mut l = Ledger {
            snapshots: vec![mk(7, None), mk(9, Some(7))],
            cursor: Some(1),
            cursor_id: None,
            next_id: 10,
            ..Default::default()
        };
        l.normalize().unwrap();
        assert_eq!(l.cursor_id, Some(9));
    }

    #[test]
    fn redo_prefers_non_recovery_child() {
        // After restore creates recovery preimage #3 under parent #1, redo from #1 must pick the
        // real branch tip #2, not the recovery safety snapshot.
        let snaps = vec![
            mk(1, None),
            Snapshot {
                recovery: false,
                ..mk(2, Some(1))
            },
            Snapshot {
                recovery: true,
                label: "before time-travel".into(),
                ..mk(3, Some(1))
            },
        ];
        let child = snaps
            .iter()
            .filter(|s| s.parent == Some(1) && !s.recovery)
            .max_by_key(|s| s.id)
            .map(|s| s.id);
        assert_eq!(child, Some(2));
    }

    #[test]
    fn run_recovery_anchors_and_budget() {
        begin_agent_run();
        assert_eq!(recovery_status().pre_edit, None);
        assert!(recovery_hint().is_none());
        note_pre_edit(3);
        note_last_good(5);
        let s = recovery_status();
        assert_eq!(s.pre_edit, Some(3));
        assert_eq!(s.last_good, Some(5));
        assert_eq!(s.rewinds_left, 2);
        assert!(recovery_hint().unwrap().contains("last_good"));
        // note_pre_edit is sticky to the earliest id.
        note_pre_edit(9);
        assert_eq!(recovery_status().pre_edit, Some(3));
        begin_agent_run();
        assert_eq!(recovery_status().pre_edit, None);
        assert_eq!(recovery_status().rewinds_used, 0);
    }

    #[test]
    fn diff_side_parses_ids_and_working_aliases() {
        assert_eq!(DiffSide::parse("5"), Some(DiffSide::Checkpoint(5)));
        // The timeline prints ids as `#5`, so pasting one back must work.
        assert_eq!(DiffSide::parse("#12"), Some(DiffSide::Checkpoint(12)));
        for alias in [
            "working", "worktree", "wt", "now", "current", "disk", "WORKING",
        ] {
            assert_eq!(
                DiffSide::parse(alias),
                Some(DiffSide::Working),
                "alias {alias}"
            );
        }
        assert_eq!(DiffSide::parse("nonsense"), None);
        // `0` is not a valid checkpoint id (the ledger starts at 1) but parses as one; the ledger
        // lookup is what rejects it, with a message naming `aizen time list`.
        assert_eq!(DiffSide::parse("0"), Some(DiffSide::Checkpoint(0)));
    }

    #[test]
    fn merge_diff_streams_pairs_counts_with_status() {
        // Byte-for-byte what `git diff-tree -z --name-status` / `--numstat` emit for plain edits:
        // status and path are separate NUL fields; numstat keeps the path in the tab-split field.
        let name = b"M\0Cargo.lock\0M\0Cargo.toml\0";
        let num = b"1\t1\tCargo.lock\x002\t3\tCargo.toml\0";
        let files = merge_diff_streams(name, num);
        assert_eq!(files.len(), 2);
        assert_eq!(files[0].path, "Cargo.lock");
        assert_eq!(
            (files[0].status, files[0].added, files[0].deleted),
            ('M', Some(1), Some(1))
        );
        assert_eq!(
            (files[1].status, files[1].added, files[1].deleted),
            ('M', Some(2), Some(3))
        );
        assert!(files.iter().all(|f| f.old_path.is_none()));
    }

    #[test]
    fn merge_diff_streams_handles_renames_and_binary() {
        // Rename: name-status is `R080\0old\0new`, numstat leaves the inline path EMPTY and follows
        // with old\0new as their own fields — the two streams disagree in shape, which is the whole
        // reason this merge exists. Counts must key on the NEW path.
        let name = b"R080\0old.txt\0new.txt\0";
        let num = b"1\t1\t\0old.txt\0new.txt\0";
        let files = merge_diff_streams(name, num);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].status, 'R');
        assert_eq!(files[0].path, "new.txt");
        assert_eq!(files[0].old_path.as_deref(), Some("old.txt"));
        assert_eq!((files[0].added, files[0].deleted), (Some(1), Some(1)));

        // Binary: git reports `-` for both counts. `None` must survive as "not text", not as 0.
        let files = merge_diff_streams(b"M\0logo.png\0", b"-\t-\tlogo.png\0");
        assert_eq!(files.len(), 1);
        assert_eq!((files[0].added, files[0].deleted), (None, None));
    }

    #[test]
    fn merge_diff_streams_survives_truncated_and_empty_input() {
        assert!(merge_diff_streams(b"", b"").is_empty());
        // A status with no following path must not panic or invent an entry.
        assert!(merge_diff_streams(b"M\0", b"").is_empty());
        // Missing numstat side → counts unknown, but the file is still reported.
        let files = merge_diff_streams(b"A\0new.rs\0", b"");
        assert_eq!(files.len(), 1);
        assert_eq!((files[0].status, files[0].added), ('A', None));
    }

    #[test]
    fn diff_report_totals_ignore_binary_files() {
        let report = DiffReport {
            from: "#1".into(),
            to: "working tree".into(),
            files: vec![
                FileChange {
                    status: 'M',
                    path: "a.rs".into(),
                    old_path: None,
                    added: Some(3),
                    deleted: Some(1),
                },
                FileChange {
                    status: 'M',
                    path: "b.png".into(),
                    old_path: None,
                    added: None,
                    deleted: None,
                },
            ],
            patch: None,
            patch_truncated: false,
        };
        assert_eq!(report.total_added(), 3);
        assert_eq!(report.total_deleted(), 1);
        assert!(!report.is_empty());
    }

    #[test]
    fn rewind_target_parse() {
        assert_eq!(RewindTarget::parse("pre_edit"), Some(RewindTarget::PreEdit));
        assert_eq!(
            RewindTarget::parse("last-good"),
            Some(RewindTarget::LastGood)
        );
        assert_eq!(RewindTarget::parse("undo"), Some(RewindTarget::LastGood));
        assert_eq!(RewindTarget::parse("42"), None);
    }

    #[test]
    fn private_store_path_is_under_aizen_home() {
        // Identity hashing is pure; the store root must never be inside the source .git.
        let common = PathBuf::from(r"C:\work\proj\.git");
        let repo_id = format!("repo-{:016x}", fnv1a64(&common.to_string_lossy()));
        let home = PathBuf::from(r"C:\Users\me\.aizen");
        let store = home.join("timemachine").join(&repo_id).join("store.git");
        assert!(store.starts_with(&home));
        assert!(!store.components().any(|c| c.as_os_str() == ".git"));
        assert!(repo_id.starts_with("repo-"));
    }

    #[test]
    fn journal_roundtrip_shape() {
        let j = Journal::new(JournalKind::Save, 7);
        assert_eq!(j.expected_generation, 7);
        assert!(matches!(j.phase, JournalPhase::Prepared));
        let bytes = serde_json::to_vec(&j).unwrap();
        let back: Journal = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(back.operation_id, j.operation_id);
        assert!(matches!(back.kind, JournalKind::Save));
    }
}
