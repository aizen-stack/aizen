//! One-time re-slug of memory ids that the old ASCII-only `slugify` shredded.
//!
//! `store::slugify` used to test one codepoint at a time against `is_ascii_alphanumeric`, so every
//! accented letter failed and became a `-` — it cut INSIDE words. On a real 243-entry store that
//! left **185 (76%)** of ids looking like `ng-i-d-ng-giao-ti-p-b-ng-ti-ng-vi-t`: unreadable,
//! unguessable, and therefore unusable as the handle `memory show|edit|forget` requires. `slugify`
//! now folds the accent off the letter before deciding about word boundaries, so the same name files
//! as `nguoi-dung-giao-tiep-bang-tieng-viet` — but that only helps facts written from here on. The
//! ids already on disk stay broken until they are moved.
//!
//! This module does that move, once, at startup. It needs no model and makes no guesses: the display
//! `name` in each file's frontmatter was NEVER mangled (only the filename was), so the correct id is
//! just `slugify(name)` recomputed. Purely mechanical, and reproducible from the same inputs.
//!
//! ## What has to move together
//!
//! An id is referenced from four places, verified against a real store rather than assumed:
//!
//! | Where | References an id? |
//! |---|---|
//! | `entries/*.md`, `review/*.md`, `archive/*.md` | yes — the id IS the filename |
//! | `graph.tsv` | yes — both endpoints of all 189 edges |
//! | `embed-cache/*.json` | no — keyed by content hash, so vectors survive a rename |
//! | frontmatter `superseded_by` / `supersedes` | no rows carry one (measured: 0 of 243) |
//! | `learning-audit.jsonl` | append-only log, never read back to resolve |
//!
//! So renaming files without rewriting `graph.tsv` in the same pass would leave every edge pointing
//! at an id that no longer exists — the association layer would silently go dark. That is the one
//! failure this module is shaped around.
//!
//! ## Ordering, and why
//!
//! The mapping file is written FIRST, before a single rename. The user chose not to review a plan up
//! front, which makes the after-the-fact record the only way to answer "what was this called before"
//! — so it must exist before anything moves, not after everything did. Every subsequent step is a
//! rename or an atomic write, so a mid-way failure leaves that artifact exactly where it was and the
//! mapping on disk still describes the intent.

use std::collections::HashMap;
use std::path::PathBuf;

use crate::core::config;
use crate::memory::{frontmatter, store};

/// Marker that the pass already ran. Presence alone is the check — its contents are informational.
const DONE_FLAG: &str = ".id-slug-v2";

/// Escape hatch + test hook: `AIZEN_NO_ID_MIGRATE=1` skips the pass entirely.
const OPT_OUT_ENV: &str = "AIZEN_NO_ID_MIGRATE";

/// One id that needs to move, and where to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rename {
    pub old: String,
    pub new: String,
}

/// What [`apply`] did. Per-artifact failures land in `warnings` instead of aborting: every operation
/// is independent, and stopping halfway would leave the graph half-rewritten — the one state with no
/// clean recovery.
#[derive(Debug, Default)]
pub struct Report {
    pub renamed: usize,
    pub edges_rewritten: usize,
    pub mapping_path: Option<PathBuf>,
    pub warnings: Vec<String>,
}

impl Report {
    /// One line for the startup banner, or `None` when nothing moved.
    ///
    /// Silence would be wrong here even though the user opted out of a confirmation prompt: opting
    /// out of *approving* a rename of 243 files is not opting out of *knowing* it happened, and the
    /// mapping path is the only way back to the old names.
    pub fn notice(&self) -> Option<String> {
        if self.renamed == 0 {
            return None;
        }
        let mut s = format!(
            "memory: renamed {} id(s) so they read as whole words",
            self.renamed
        );
        if self.edges_rewritten > 0 {
            s.push_str(&format!("; {} graph edge(s) re-pointed", self.edges_rewritten));
        }
        if let Some(p) = &self.mapping_path {
            s.push_str(&format!(" — old→new map: {}", p.display()));
        }
        if !self.warnings.is_empty() {
            s.push_str(&format!(" ({} warning(s))", self.warnings.len()));
        }
        Some(s)
    }
}

/// The three id-keyed stores, in the order [`apply`] walks them.
fn id_dirs() -> [(PathBuf, &'static str); 3] {
    [
        (config::entries_dir(), "entries"),
        (config::review_dir(), "review"),
        (config::archive_dir(), "archive"),
    ]
}

/// Split an archive stem into `(id, suffix)`. `caps` parks evicted rows as `<id>-r1`, `<id>-r2`, so
/// the revision suffix has to survive the rename or two archived copies of one fact would collide.
fn split_archive_suffix(stem: &str) -> (&str, &str) {
    if let Some(pos) = stem.rfind("-r") {
        let tail = &stem[pos + 2..];
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            return (&stem[..pos], &stem[pos..]);
        }
    }
    (stem, "")
}

/// Every id whose recomputed slug differs from its filename, across all three stores.
///
/// Reads only the `name` field, so a file whose body failed to parse still gets planned correctly.
/// Files with no `name` are skipped: without it there is nothing to recompute from, and inventing an
/// id from the body would be exactly the guess this module avoids.
pub fn plan() -> Vec<Rename> {
    let mut out: Vec<Rename> = Vec::new();
    for (dir, _) in id_dirs() {
        // Per-directory, NOT global: the same fact legitimately exists as `entries/x.md` and
        // `archive/x-r1.md`, and a shared collision set would see the second as a clash and park it
        // as `x-2-r1` — quietly divorcing an archived revision from the row it is a revision OF.
        let mut taken: HashMap<String, ()> = HashMap::new();
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut stems: Vec<String> = rd
            .flatten()
            .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("md"))
            .filter_map(|e| {
                e.path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
            })
            .collect();
        // Deterministic order so a collision resolves to the same `-2` on every machine and the
        // mapping file is reproducible.
        stems.sort();
        for stem in stems {
            let path = dir.join(format!("{stem}.md"));
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let fm = frontmatter::parse(&raw);
            let Some(name) = fm.get("name").map(str::trim).filter(|s| !s.is_empty()) else {
                continue;
            };
            let (base, suffix) = split_archive_suffix(&stem);
            let fresh = store::slugify(name);
            if fresh == base {
                // Already correct. Claim it anyway, or a later row that slugs to the same thing would
                // be handed this exact filename and the rename would be refused as "already exists".
                taken.insert(format!("{fresh}{suffix}"), ());
                continue;
            }
            // Two names that differed only in punctuation can now slug identically. `store::unique_id`
            // is not reusable here (it tests the live dir, which still holds the OLD names), so
            // disambiguate against what this directory's plan has already claimed.
            let mut candidate = format!("{fresh}{suffix}");
            if taken.contains_key(&candidate) {
                let mut n = 2;
                while taken.contains_key(&format!("{fresh}-{n}{suffix}")) {
                    n += 1;
                }
                candidate = format!("{fresh}-{n}{suffix}");
            }
            taken.insert(candidate.clone(), ());
            out.push(Rename {
                old: stem.clone(),
                new: candidate,
            });
        }
    }
    out
}

/// Execute a plan. Best-effort per artifact; see [`Report`].
pub fn apply(plan: &[Rename]) -> Report {
    let mut rep = Report::default();
    if plan.is_empty() {
        return rep;
    }
    // FIRST, before anything moves — see the module header.
    match write_mapping(plan) {
        Ok(p) => rep.mapping_path = Some(p),
        Err(e) => {
            // No record means no way back, so this is the one failure that stops the pass.
            rep.warnings
                .push(format!("could not write the old→new map ({e:#}) — nothing was renamed"));
            return rep;
        }
    }
    // Graph endpoints are bare ids, never archive revisions, so the table is keyed by the id part.
    // Feeding it `x-r1 → y-r1` would leave a real `x` endpoint unmapped.
    let mut by_old: HashMap<&str, &str> = HashMap::new();
    for r in plan {
        let (old_base, old_suffix) = split_archive_suffix(&r.old);
        let (new_base, _) = split_archive_suffix(&r.new);
        if old_suffix.is_empty() {
            by_old.insert(old_base, new_base);
        } else {
            // Only add a suffixed row's mapping if the plain id isn't already covered — the live
            // entry (if any) is the authority on what `x` becomes.
            by_old.entry(old_base).or_insert(new_base);
        }
    }
    for (dir, label) in id_dirs() {
        for r in plan {
            let from = dir.join(format!("{}.md", r.old));
            if !from.is_file() {
                continue; // this rename belongs to a different store
            }
            let to = dir.join(format!("{}.md", r.new));
            if to.exists() {
                rep.warnings.push(format!(
                    "{label}: '{}' already exists — left '{}' in place",
                    r.new, r.old
                ));
                continue;
            }
            match std::fs::rename(&from, &to) {
                Ok(()) => rep.renamed += 1,
                Err(e) => rep
                    .warnings
                    .push(format!("{label}: could not rename '{}': {e}", r.old)),
            }
        }
    }
    match rewrite_graph(&by_old) {
        Ok(n) => rep.edges_rewritten = n,
        Err(e) => rep
            .warnings
            .push(format!("graph.tsv: could not re-point edges: {e:#}")),
    }
    rep
}

/// Persist the old→new table as TSV next to the store.
fn write_mapping(plan: &[Rename]) -> anyhow::Result<PathBuf> {
    let dir = config::cli_memory_dir();
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!(".id-migration-{}.tsv", crate::memory::bloat::decay::today()));
    let mut s = String::with_capacity(plan.len() * 64);
    s.push_str("# old_id\tnew_id — written before any rename; `memory show <old>` will not work\n");
    for r in plan {
        s.push_str(&format!("{}\t{}\n", r.old, r.new));
    }
    crate::core::persist::atomic_write(&path, s.as_bytes())?;
    Ok(path)
}

/// Re-point both endpoints of every edge. Endpoints absent from the table are copied verbatim —
/// that is what keeps the cross-kind `skill:` / `persona:` namespaces intact.
fn rewrite_graph(by_old: &HashMap<&str, &str>) -> anyhow::Result<usize> {
    let path = config::graph_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(0); // no graph yet
    };
    let mut out = String::with_capacity(raw.len());
    let mut changed = 0usize;
    for line in raw.lines() {
        let mut cols: Vec<String> = line.split('\t').map(str::to_string).collect();
        if cols.len() < 2 {
            out.push_str(line);
            out.push('\n');
            continue;
        }
        let mut touched = false;
        // `iter_mut().take(2)` rather than an index loop: only the two endpoint columns are
        // rewritten, and weight/date must pass through byte-identical.
        for col in cols.iter_mut().take(2) {
            if let Some(new) = by_old.get(col.trim()) {
                *col = (*new).to_string();
                touched = true;
            }
        }
        if touched {
            changed += 1;
        }
        out.push_str(&cols.join("\t"));
        out.push('\n');
    }
    if changed > 0 {
        crate::core::persist::atomic_write(&path, out.as_bytes())?;
    }
    Ok(changed)
}

/// Whether the pass still needs to run here.
fn pending() -> bool {
    if std::env::var(OPT_OUT_ENV).is_ok_and(|v| !v.trim().is_empty() && v.trim() != "0") {
        return false;
    }
    !config::cli_memory_dir().join(DONE_FLAG).exists()
}

/// Record that the pass ran, so the next launch skips it.
fn mark_done(rep: &Report) {
    let dir = config::cli_memory_dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let body = format!(
        "{}\trenamed={}\tedges={}\n",
        crate::memory::bloat::decay::today(),
        rep.renamed,
        rep.edges_rewritten
    );
    let _ = crate::core::persist::atomic_write(&dir.join(DONE_FLAG), body.as_bytes());
}

/// The startup entry point: plan + apply + flag, once, under the store lock.
///
/// Returns `None` when there was nothing to do (already migrated, opted out, or every id was already
/// correct), so the caller prints nothing on the overwhelmingly common path.
///
/// Takes the same exclusive lock the write path uses. Renaming files under a concurrent session's
/// feet would make its next read fail on a path that vanished mid-turn; not getting the lock simply
/// defers the pass to a later launch, which is why the flag is only set on a run that held it.
pub fn run_once_at_startup() -> Option<Report> {
    if !pending() {
        return None;
    }
    let lock_path = crate::core::workspace_txn::store_lock("memory_id_migration", "global");
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        std::time::Duration::from_secs(2),
    )
    .ok()?;
    let plan = plan();
    let rep = apply(&plan);
    // Flag even when the plan was empty: that store is already correct, and re-scanning every entry
    // on every launch to rediscover that is a startup cost with no payoff.
    if rep.warnings.is_empty() || rep.renamed > 0 {
        mark_done(&rep);
    }
    // Report anything the user would want to know about — a rename, or a failure to do one. Returning
    // `None` only for the silent no-op keeps the caller from printing on the common path.
    if rep.renamed > 0 || !rep.warnings.is_empty() {
        Some(rep)
    } else {
        None
    }
}

/// Path of the marker. Test-only: nothing in the product needs to name it — `pending` checks it and
/// `mark_done` writes it, both from inside this module.
#[cfg(test)]
fn done_flag_path() -> PathBuf {
    config::cli_memory_dir().join(DONE_FLAG)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &std::path::Path, stem: &str, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("{stem}.md")),
            format!("---\nname: {name}\ntype: user\n---\n\nbody of {stem}\n"),
        )
        .unwrap();
    }

    /// Full pass on a store shaped like the real one: a shredded id, an already-correct id, a
    /// suffixed archive row, a graph edge pointing at the shredded id, and a cross-kind `skill:`
    /// edge that must come out untouched.
    #[test]
    fn migrates_ids_and_repoints_graph_but_leaves_other_namespaces_alone() {
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-idmig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);
        std::env::remove_var(OPT_OUT_ENV);

        let shredded = "ng-i-d-ng-giao-ti-p-b-ng-ti-ng-vi-t";
        write(
            &config::entries_dir(),
            shredded,
            "Người dùng giao tiếp bằng tiếng Việt",
        );
        write(&config::entries_dir(), "auth-strategy", "Auth Strategy");
        write(&config::review_dir(), "m-y-windows", "Máy Windows");
        write(&config::archive_dir(), &format!("{shredded}-r1"), "Người dùng giao tiếp bằng tiếng Việt");
        std::fs::write(
            config::graph_path(),
            format!(
                "{shredded}\tauth-strategy\t1.0000\t2026-08-01\n\
                 skill:do-a-thing\t{shredded}\t0.5000\t2026-08-02\n"
            ),
        )
        .unwrap();

        let moves = plan();
        let rep = apply(&moves);
        assert!(rep.warnings.is_empty(), "warnings: {:?}", rep.warnings);

        let want = "nguoi-dung-giao-tiep-bang-tieng-viet";
        assert!(
            config::entries_dir().join(format!("{want}.md")).is_file(),
            "entry not renamed; dir now: {:?}",
            std::fs::read_dir(config::entries_dir())
                .unwrap()
                .flatten()
                .map(|e| e.file_name())
                .collect::<Vec<_>>()
        );
        // An already-correct id must not move — churn on every launch would be its own bug.
        assert!(config::entries_dir().join("auth-strategy.md").is_file());
        assert!(config::review_dir().join("may-windows.md").is_file());
        // The `-r1` revision suffix survives, or two archived copies would collide.
        assert!(config::archive_dir().join(format!("{want}-r1.md")).is_file());

        let graph = std::fs::read_to_string(config::graph_path()).unwrap();
        assert!(graph.contains(want), "edge not re-pointed: {graph}");
        assert!(!graph.contains(shredded), "old id still in graph: {graph}");
        // The whole point of the namespace: an endpoint that is not a memory id passes through.
        assert!(
            graph.contains("skill:do-a-thing"),
            "cross-kind endpoint mangled: {graph}"
        );
        assert_eq!(rep.edges_rewritten, 2);

        // The map exists and describes what moved.
        let map = std::fs::read_to_string(rep.mapping_path.as_ref().unwrap()).unwrap();
        assert!(map.contains(&format!("{shredded}\t{want}")), "map: {map}");
        assert!(!map.contains("auth-strategy\t"), "unchanged id in map: {map}");

        // Second pass is a no-op: nothing left to rename.
        assert!(plan().is_empty(), "plan not empty on a migrated store");

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// Two names that differ only in punctuation now slug identically; the second must be parked
    /// rather than overwriting the first.
    #[test]
    fn collisions_get_a_numeric_suffix_instead_of_clobbering() {
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-idmig-col-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);

        write(&config::entries_dir(), "a-b", "Việt: Nam");
        write(&config::entries_dir(), "c-d", "Việt Nam");
        let moves = plan();
        assert_eq!(moves.len(), 2, "{moves:?}");
        let news: Vec<&str> = moves.iter().map(|r| r.new.as_str()).collect();
        assert!(news.contains(&"viet-nam"), "{news:?}");
        assert!(news.contains(&"viet-nam-2"), "{news:?}");
        let rep = apply(&moves);
        assert_eq!(rep.renamed, 2, "warnings: {:?}", rep.warnings);

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The opt-out has to be honoured before any disk work, and the flag has to stop a second pass.
    #[test]
    fn opt_out_env_and_done_flag_both_skip_the_pass() {
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-idmig-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);

        std::env::set_var(OPT_OUT_ENV, "1");
        assert!(!pending(), "opt-out ignored");
        std::env::remove_var(OPT_OUT_ENV);
        assert!(pending(), "should be pending on a fresh home");

        std::fs::create_dir_all(config::cli_memory_dir()).unwrap();
        std::fs::write(done_flag_path(), "2026-08-05\n").unwrap();
        assert!(!pending(), "done flag ignored");

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn archive_revision_suffix_is_split_off_not_slugged() {
        assert_eq!(split_archive_suffix("some-id-r1"), ("some-id", "-r1"));
        assert_eq!(split_archive_suffix("some-id-r12"), ("some-id", "-r12"));
        // Not a revision marker: a word that merely starts with `r`.
        assert_eq!(split_archive_suffix("some-id-run"), ("some-id-run", ""));
        assert_eq!(split_archive_suffix("plain"), ("plain", ""));
    }
}
