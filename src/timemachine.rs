//! Time machine — git-backed code snapshots you can rewind to and re-apply ("quay về / trở lại").
//!
//! A **checkpoint** captures the WHOLE working tree (tracked + untracked, honoring `.gitignore`) as a
//! git commit object on a private ref (`refs/ng/tm/<id>`), recorded in a per-repo ledger. It NEVER
//! touches your real index, HEAD, branches, or stash — it's a parallel timeline. **Restore** rewinds
//! the working tree to any checkpoint exactly (files added since are removed, deleted ones come back),
//! and because every restore first auto-snapshots the current state, you can always go forward again.
//!
//! Mechanism (all via the `git` CLI — no new dep): snapshot = `add -A` into a TEMP index
//! (`GIT_INDEX_FILE`, so the real index is untouched) → `write-tree` → `commit-tree` → `update-ref`.
//! Restore = stage current state into a temp index → `read-tree --reset -u <tree>` (updates the
//! working tree, removing files not in the snapshot). `.gitignore`d paths (node_modules/target) are
//! never staged, so they're never touched.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Default cap on retained checkpoints (config `timemachine_keep`; `Some(0)` = unlimited). Beyond
/// this, the OLDEST checkpoints are auto-pruned on each save so the timeline can't grow without
/// bound. Pruning deletes each snapshot's `refs/ng/tm/<id>` ref → its git objects become unreachable
/// and are reclaimed by git's normal maintenance (`git gc`).
const DEFAULT_KEEP: usize = 50;

/// One saved point on the timeline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub id: u32,
    /// The snapshot commit sha (on `refs/ng/tm/<id>`).
    pub commit: String,
    /// The snapshot tree sha (used to detect "is the working tree already at this state?").
    pub tree: String,
    pub label: String,
    /// Local timestamp string (display only).
    pub created: String,
    /// `true` if auto-created (e.g. just before a restore) vs an explicit user/agent checkpoint.
    pub auto: bool,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Ledger {
    pub snapshots: Vec<Snapshot>,
    /// Index into `snapshots` of the currently-active point (for `undo`/`redo`). `None` until the
    /// first restore.
    pub cursor: Option<usize>,
    pub next_id: u32,
}

// ── git helpers (each takes an explicit repo root → testable against a temp repo) ──

/// Run `git <args>` in `root`. `index` sets `GIT_INDEX_FILE` (a temp index) when `Some`.
fn git_at(root: &Path, index: Option<&Path>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(root).args(args);
    if let Some(idx) = index {
        cmd.env("GIT_INDEX_FILE", idx);
    }
    let out = cmd.output().with_context(|| format!("running `git {}` (is git installed?)", args.join(" ")))?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The repo root for the current directory, or an error if we're not in a git repo.
pub fn repo_root() -> Result<PathBuf> {
    let root = git_at(&std::env::current_dir()?, None, &["rev-parse", "--show-toplevel"])
        .context("not a git repository (run `git init` first to use the time machine)")?;
    Ok(PathBuf::from(root))
}

/// Whether the current directory is inside a git repo (cheap probe).
pub fn is_repo() -> bool {
    repo_root().is_ok()
}

fn ledger_path(root: &Path) -> Result<PathBuf> {
    let git_dir = git_at(root, None, &["rev-parse", "--git-dir"])?;
    let gd = root.join(git_dir);
    Ok(gd.join("ng_timemachine.json"))
}

fn load_ledger(root: &Path) -> Ledger {
    match ledger_path(root).ok().and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(s) => serde_json::from_str(&s).unwrap_or_default(),
        None => Ledger::default(),
    }
}

fn save_ledger(root: &Path, l: &Ledger) -> Result<()> {
    let p = ledger_path(root)?;
    std::fs::write(&p, serde_json::to_string_pretty(l)? + "\n").with_context(|| format!("writing {}", p.display()))
}

/// A unique temp index path inside the git dir (cleaned up after use).
fn temp_index(root: &Path) -> Result<PathBuf> {
    let git_dir = git_at(root, None, &["rev-parse", "--git-dir"])?;
    Ok(root.join(git_dir).join(format!("ng_tm_index_{}", std::process::id())))
}

/// The tree sha for the CURRENT working tree (everything `add -A` would stage). Uses a temp index so
/// the real index is untouched.
fn current_tree(root: &Path) -> Result<String> {
    let idx = temp_index(root)?;
    let _ = std::fs::remove_file(&idx);
    git_at(root, Some(&idx), &["add", "-A"])?;
    let tree = git_at(root, Some(&idx), &["write-tree"]);
    let _ = std::fs::remove_file(&idx);
    tree
}

// ── public API ──────────────────────────────────────────────────────────────────

/// Capture the current working tree as a checkpoint. `auto` marks system-created points.
pub fn save(label: &str, auto: bool) -> Result<Snapshot> {
    let root = repo_root()?;
    save_in(&root, label, auto)
}

fn save_in(root: &Path, label: &str, auto: bool) -> Result<Snapshot> {
    let tree = current_tree(root)?;
    let mut ledger = load_ledger(root);

    // Dedup: if the newest checkpoint already captures this exact tree, reuse it instead of piling on
    // a zero-diff snapshot (the main source of checkpoint spam).
    if let Some(last) = ledger.snapshots.last() {
        if last.tree == tree {
            let last = last.clone();
            ledger.cursor = Some(ledger.snapshots.len() - 1);
            save_ledger(root, &ledger)?;
            return Ok(last);
        }
    }

    // Chain onto HEAD when there is one (nicer `git show`), else a parentless commit.
    let head = git_at(root, None, &["rev-parse", "--verify", "-q", "HEAD"]).ok();
    let msg = format!("ng checkpoint: {}", if label.is_empty() { "(no label)" } else { label });
    let mut args = vec!["commit-tree", &tree, "-m", &msg];
    if let Some(h) = head.as_deref() {
        args.push("-p");
        args.push(h);
    }
    let commit = git_at(root, None, &args)?;

    let id = ledger.next_id.max(1);
    ledger.next_id = id + 1;
    git_at(root, None, &["update-ref", &format!("refs/ng/tm/{id}"), &commit])?;
    let snap = Snapshot {
        id,
        commit,
        tree,
        label: label.to_string(),
        created: now_string(),
        auto,
    };
    ledger.snapshots.push(snap.clone());
    ledger.cursor = Some(ledger.snapshots.len() - 1);
    // Cap the timeline so heavy checkpointing can't fill the repo.
    let keep = crate::cli_config::load().timemachine_keep.unwrap_or(DEFAULT_KEEP);
    enforce_retention(root, &mut ledger, keep);
    save_ledger(root, &ledger)?;
    Ok(snap)
}

/// Delete a checkpoint's ref so its git objects become unreachable (reclaimed by `git gc`).
fn delete_ref(root: &Path, id: u32) {
    let _ = git_at(root, None, &["update-ref", "-d", &format!("refs/ng/tm/{id}")]);
}

/// Drop oldest checkpoints (deleting their refs) until at most `keep` remain — but NEVER drop the
/// currently-active one (`cursor`). `keep == 0` means unlimited. Fixes up `cursor` by id afterward.
fn enforce_retention(root: &Path, ledger: &mut Ledger, keep: usize) {
    if keep == 0 || ledger.snapshots.len() <= keep {
        return;
    }
    let active_id = ledger.cursor.and_then(|c| ledger.snapshots.get(c)).map(|s| s.id);
    let mut i = 0;
    while ledger.snapshots.len() > keep && i < ledger.snapshots.len() {
        if Some(ledger.snapshots[i].id) == active_id {
            i += 1; // protect the active checkpoint; try the next-oldest
            continue;
        }
        let dropped = ledger.snapshots.remove(i); // oldest non-active → gone (don't advance i)
        delete_ref(root, dropped.id);
    }
    ledger.cursor = active_id.and_then(|id| ledger.snapshots.iter().position(|s| s.id == id));
}

/// Manually prune to at most `keep` checkpoints. Returns the number dropped.
pub fn prune(keep: usize) -> Result<usize> {
    let root = repo_root()?;
    let mut ledger = load_ledger(&root);
    let before = ledger.snapshots.len();
    enforce_retention(&root, &mut ledger, keep);
    save_ledger(&root, &ledger)?;
    Ok(before - ledger.snapshots.len())
}

/// Delete ALL checkpoints (and their refs). Returns the number removed.
pub fn clear() -> Result<usize> {
    let root = repo_root()?;
    let mut ledger = load_ledger(&root);
    let n = ledger.snapshots.len();
    for s in &ledger.snapshots {
        delete_ref(&root, s.id);
    }
    let next_id = ledger.next_id; // keep the id counter monotonic so old refs never alias
    ledger = Ledger { next_id, ..Default::default() };
    save_ledger(&root, &ledger)?;
    Ok(n)
}

/// Local timestamp string for a snapshot label.
fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Restore the working tree to checkpoint `id`. Auto-snapshots first if the tree has drifted, so the
/// move is reversible (you can restore forward again). Returns the restored snapshot.
pub fn restore(id: u32) -> Result<Snapshot> {
    let root = repo_root()?;
    restore_in(&root, id)
}

fn restore_in(root: &Path, id: u32) -> Result<Snapshot> {
    let ledger = load_ledger(root);
    let target = ledger
        .snapshots
        .iter()
        .find(|s| s.id == id)
        .cloned()
        .with_context(|| format!("no checkpoint #{id} (see `ng time list`)"))?;

    // Safety: if the working tree differs from every saved snapshot, capture it first so nothing is
    // lost — then you can come back to "now" via the timeline.
    let cur = current_tree(root)?;
    if !ledger.snapshots.iter().any(|s| s.tree == cur) {
        save_in(root, "before time-travel", true)?;
    }

    // Exact rewind: stage the current state into a temp index, then reset index+worktree to the
    // snapshot tree (removing files not in it). Real index/HEAD/branches are untouched.
    let idx = temp_index(root)?;
    let _ = std::fs::remove_file(&idx);
    git_at(root, Some(&idx), &["add", "-A"])?;
    let res = git_at(root, Some(&idx), &["read-tree", "--reset", "-u", &target.commit]);
    let _ = std::fs::remove_file(&idx);
    res.context("restoring the working tree to the snapshot")?;

    // Move the cursor to the restored point.
    let mut ledger = load_ledger(root);
    ledger.cursor = ledger.snapshots.iter().position(|s| s.id == id);
    save_ledger(root, &ledger)?;
    Ok(target)
}

/// Step one checkpoint back along the timeline (`undo`).
pub fn undo() -> Result<Snapshot> {
    let root = repo_root()?;
    let ledger = load_ledger(root.as_path());
    if ledger.snapshots.is_empty() {
        bail!("no checkpoints yet — save one with `ng time save`");
    }
    let cur = ledger.cursor.unwrap_or(ledger.snapshots.len() - 1);
    if cur == 0 {
        bail!("already at the oldest checkpoint");
    }
    let id = ledger.snapshots[cur - 1].id;
    restore_in(&root, id)
}

/// Step one checkpoint forward along the timeline (`redo`).
pub fn redo() -> Result<Snapshot> {
    let root = repo_root()?;
    let ledger = load_ledger(root.as_path());
    if ledger.snapshots.is_empty() {
        bail!("no checkpoints yet");
    }
    let cur = ledger.cursor.unwrap_or(ledger.snapshots.len() - 1);
    if cur + 1 >= ledger.snapshots.len() {
        bail!("already at the newest checkpoint");
    }
    let id = ledger.snapshots[cur + 1].id;
    restore_in(&root, id)
}

/// The timeline (snapshots + the active cursor index) for display.
pub fn timeline() -> Result<(Vec<Snapshot>, Option<usize>)> {
    let root = repo_root()?;
    let l = load_ledger(&root);
    Ok((l.snapshots, l.cursor))
}

// ── agent tool ─────────────────────────────────────────────────────────────────

/// `checkpoint` — let the agent stamp a restore point before a risky change. Non-destructive (it
/// only *adds* a recovery point; it never modifies files), so it's not approval-gated. Restoring is
/// deliberately NOT an agent tool — rewinding the user's working tree stays human-driven.
pub struct Checkpoint;
impl crate::agent::tools::Tool for Checkpoint {
    fn name(&self) -> &str {
        "checkpoint"
    }
    fn description(&self) -> &str {
        "Save a time-machine checkpoint of the whole working tree (a restore point). Call this BEFORE \
         a large or risky multi-file change so the user can rewind with `ng time restore`. Only works \
         inside a git repo. Safe / non-destructive."
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {"label": {"type": "string", "description": "short note, e.g. 'before refactor auth'"}},
            "additionalProperties": false
        })
    }
    fn execute(&self, args: &serde_json::Value) -> Result<String> {
        if !is_repo() {
            return Ok("error: not a git repository — checkpoints need git (run `git init`)".to_string());
        }
        let label = args.get("label").and_then(|v| v.as_str()).unwrap_or("").trim();
        let snap = save(label, false)?;
        Ok(format!("checkpoint #{} saved ({}). Restore later with `ng time restore {}`.", snap.id, if snap.label.is_empty() { "no label" } else { &snap.label }, snap.id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spin up a throwaway git repo, exercise save → modify → restore → redo end-to-end.
    fn git_ok(root: &Path, args: &[&str]) {
        git_at(root, None, args).unwrap();
    }

    #[test]
    fn save_restore_redo_round_trip() {
        // Skip silently if git isn't on PATH (keeps the suite green in odd CI images).
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ng-tm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git_ok(&dir, &["init", "-q"]);
        git_ok(&dir, &["config", "user.email", "t@t"]);
        git_ok(&dir, &["config", "user.name", "t"]);
        let file = dir.join("a.txt");

        std::fs::write(&file, "v1").unwrap();
        let s1 = save_in(&dir, "v1", false).unwrap();
        assert_eq!(s1.id, 1);

        std::fs::write(&file, "v2").unwrap();
        let s2 = save_in(&dir, "v2", false).unwrap();
        assert_eq!(s2.id, 2);

        // a brand-new untracked file exists at v2…
        std::fs::write(dir.join("new.txt"), "added").unwrap();

        // restore to v1 → file reverts AND the file added after v1 is removed (exact rewind)…
        restore_in(&dir, 1).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v1");
        assert!(!dir.join("new.txt").exists(), "files added after the snapshot are removed on restore");

        // …and because the pre-restore state was auto-captured, we can go forward to v2.
        restore_in(&dir, 2).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2");

        let _ = std::fs::remove_dir_all(&dir);
    }

    fn mk(id: u32) -> Snapshot {
        Snapshot {
            id,
            commit: format!("c{id}"),
            tree: format!("t{id}"),
            label: String::new(),
            created: "now".into(),
            auto: false,
        }
    }

    #[test]
    fn retention_drops_oldest_but_protects_active() {
        let mut l = Ledger::default();
        for id in 1..=5 {
            l.snapshots.push(mk(id));
        }
        l.cursor = Some(0); // active = the OLDEST (#1)
        // delete_ref no-ops here (temp_dir isn't a repo); we assert the list-trim + cursor fixup.
        enforce_retention(&std::env::temp_dir(), &mut l, 3);
        let ids: Vec<u32> = l.snapshots.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![1, 4, 5], "kept ≤3, dropped oldest non-active (#2,#3), protected active #1");
        assert_eq!(l.cursor, Some(0), "cursor still points at #1");
    }

    #[test]
    fn dedup_reuses_snapshot_when_tree_unchanged() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = std::env::temp_dir().join(format!("ng-tm-dedup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        git_at(&dir, None, &["init", "-q"]).unwrap();
        git_at(&dir, None, &["config", "user.email", "t@t"]).unwrap();
        git_at(&dir, None, &["config", "user.name", "t"]).unwrap();
        std::fs::write(dir.join("a.txt"), "same").unwrap();
        let a = save_in(&dir, "first", false).unwrap();
        let b = save_in(&dir, "again", false).unwrap(); // no change → must reuse, not append
        assert_eq!(a.id, b.id, "an unchanged working tree reuses the last checkpoint");
        assert_eq!(timeline_len(&dir), 1, "no duplicate snapshot created");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn timeline_len(root: &Path) -> usize {
        load_ledger(root).snapshots.len()
    }

    #[test]
    fn ledger_defaults_and_serde_round_trip() {
        let mut l = Ledger::default();
        assert!(l.snapshots.is_empty() && l.cursor.is_none() && l.next_id == 0);
        l.snapshots.push(Snapshot {
            id: 1,
            commit: "abc".into(),
            tree: "def".into(),
            label: "x".into(),
            created: "now".into(),
            auto: false,
        });
        l.cursor = Some(0);
        let s = serde_json::to_string(&l).unwrap();
        let back: Ledger = serde_json::from_str(&s).unwrap();
        assert_eq!(back.snapshots.len(), 1);
        assert_eq!(back.cursor, Some(0));
    }
}
