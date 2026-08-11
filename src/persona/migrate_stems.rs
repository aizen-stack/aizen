//! One-time re-slug of persona self-memory filenames that the old ASCII-only stem shredded.
//!
//! `self_mem::unique_path` used to test one codepoint at a time against `is_ascii_alphanumeric`, so
//! every accented letter failed and became a `-` — the name was cut apart from the inside. Measured
//! on a real store: **45 of 89 files (51%)** looked like `in-t-i-n-n-l-9` or `ep-work-handled-b-y-gi-3`.
//! Worse in proportion than the memory store was, and less visible, because `/persona self` renders
//! bodies rather than filenames — so nothing surfaced how bad it had got.
//!
//! Two things this pass has to get right, and they are not the same thing:
//!
//! 1. **Re-slug.** The new stem is recomputed from the file's own body, which was never mangled (only
//!    the filename was). No model, no guessing — the same mechanical recompute the memory migration
//!    does from frontmatter `name`.
//! 2. **De-collide.** Twelve files shared the stem `ep-correction-user-redirected-me-todo`, separated
//!    only by a `-2`…`-12` counter, because every episode body opens with the same type label and the
//!    same scaffolding. The new stem skips the boilerplate and carries a content hash, so re-slugging
//!    fixes the collision as a side effect.
//!
//! ## What does NOT have to move
//!
//! Unlike memory ids, a self-memory id is referenced from nowhere else. Verified rather than assumed:
//!
//! | Where | References a self-mem id? |
//! |---|---|
//! | `<slug>.self/*.md` filenames | yes — the id IS the filename |
//! | `<slug>.self/.archive/*.md` | yes — same shape, evicted rows |
//! | `graph.tsv` `persona:<slug>/<id>` | capability exists; **0 such edges** on the measured store |
//! | anything persisted elsewhere | no — ids are returned to the caller and dropped |
//!
//! The `persona:` namespace is still re-pointed when edges exist, because "zero today" is a fact about
//! one machine and `note_insight_cofire` can write them at any time.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::core::config;
use crate::persona::self_mem;

/// Marker that the pass already ran for a persona. Written inside that persona's `.self` dir, so a
/// character created later still gets migrated on its own first launch.
const DONE_FLAG: &str = ".stem-slug-v2";

/// Escape hatch + test hook, shared with the memory-id pass: one switch turns off all id migration.
const OPT_OUT_ENV: &str = "AIZEN_NO_ID_MIGRATE";

/// One file that needs to move, and where to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rename {
    /// Which directory this move belongs to — the live `.self` dir or its `.archive`.
    pub dir: PathBuf,
    pub old: String,
    pub new: String,
}

#[derive(Debug, Default)]
pub struct Report {
    pub renamed: usize,
    pub edges_rewritten: usize,
    pub mapping_path: Option<PathBuf>,
    pub warnings: Vec<String>,
}

impl Report {
    /// One line for the startup banner, or `None` when nothing moved.
    pub fn notice(&self) -> Option<String> {
        if self.renamed == 0 {
            return None;
        }
        let mut s = format!(
            "persona: renamed {} self-memory file(s) so they read as whole words",
            self.renamed
        );
        if self.edges_rewritten > 0 {
            s.push_str(&format!(
                "; {} graph edge(s) re-pointed",
                self.edges_rewritten
            ));
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

/// Every `<slug>.self` directory under `personas/`, paired with its persona slug.
fn self_dirs() -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(config::aizen_home().join("personas")) else {
        return Vec::new();
    };
    rd.flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_str()?.to_string();
            let slug = name.strip_suffix(".self")?.to_string();
            Some((slug, e.path()))
        })
        .collect()
}

/// Plan the moves for one persona: its live dir and its `.archive`, each de-colliding independently.
///
/// A file already carrying the right stem is left alone but still claims its name, so a later file
/// that recomputes to the same stem is parked rather than handed a path that exists.
pub fn plan_for(self_dir: &Path) -> Vec<Rename> {
    let mut out = Vec::new();
    for dir in [self_dir.to_path_buf(), self_dir.join(".archive")] {
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
        // Deterministic, so a collision resolves the same way on every machine and the mapping file
        // is reproducible.
        stems.sort();
        let mut taken: HashMap<String, ()> = HashMap::new();
        for stem in stems {
            let path = dir.join(format!("{stem}.md"));
            let Ok(raw) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Some(mem) = self_mem::parse_for_migration(&raw) else {
                continue; // unparseable / no body — nothing to recompute from
            };
            let prefix = if mem.is_insight { "in" } else { "ep" };
            let fresh = self_mem::stem_for(prefix, &mem.body);
            if fresh == stem {
                taken.insert(fresh, ());
                continue;
            }
            let mut candidate = fresh.clone();
            if taken.contains_key(&candidate) {
                let mut n = 2;
                while taken.contains_key(&format!("{fresh}-{n}")) {
                    n += 1;
                }
                candidate = format!("{fresh}-{n}");
            }
            taken.insert(candidate.clone(), ());
            out.push(Rename {
                dir: dir.clone(),
                old: stem,
                new: candidate,
            });
        }
    }
    out
}

/// Execute a plan for one persona. Best-effort per file: a failure on one leaves it where it is.
pub fn apply_for(persona_slug: &str, plan: &[Rename]) -> Report {
    let mut rep = Report::default();
    if plan.is_empty() {
        return rep;
    }
    // The record goes down before anything moves — it is the only way back to the old names.
    match write_mapping(persona_slug, plan) {
        Ok(p) => rep.mapping_path = Some(p),
        Err(e) => {
            rep.warnings.push(format!(
                "could not write the old→new map ({e:#}) — nothing was renamed"
            ));
            return rep;
        }
    }
    for r in plan {
        let from = r.dir.join(format!("{}.md", r.old));
        let to = r.dir.join(format!("{}.md", r.new));
        if !from.is_file() {
            continue;
        }
        if to.exists() {
            rep.warnings.push(format!(
                "'{}' already exists — left '{}' alone",
                r.new, r.old
            ));
            continue;
        }
        match std::fs::rename(&from, &to) {
            Ok(()) => rep.renamed += 1,
            Err(e) => rep
                .warnings
                .push(format!("could not rename '{}': {e}", r.old)),
        }
    }
    // `persona:<slug>/<id>` endpoints. Zero exist on the measured store, but `note_insight_cofire`
    // can write them at any time, so the pass re-points rather than assuming.
    let by_old: HashMap<String, String> = plan
        .iter()
        .map(|r| {
            (
                format!("persona:{persona_slug}/{}", r.old),
                format!("persona:{persona_slug}/{}", r.new),
            )
        })
        .collect();
    match rewrite_graph(&by_old) {
        Ok(n) => rep.edges_rewritten = n,
        Err(e) => rep
            .warnings
            .push(format!("graph.tsv: could not re-point edges: {e:#}")),
    }
    rep
}

fn write_mapping(persona_slug: &str, plan: &[Rename]) -> anyhow::Result<PathBuf> {
    let dir = config::aizen_home().join("personas");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!(
        ".stem-migration-{persona_slug}-{}.tsv",
        crate::memory::bloat::decay::today()
    ));
    let mut s = String::with_capacity(plan.len() * 64);
    s.push_str("# old_stem\tnew_stem\tdir — written before any rename\n");
    for r in plan {
        let where_ = if r.dir.file_name().and_then(|s| s.to_str()) == Some(".archive") {
            "archive"
        } else {
            "live"
        };
        s.push_str(&format!("{}\t{}\t{}\n", r.old, r.new, where_));
    }
    crate::core::persist::atomic_write(&path, s.as_bytes())?;
    Ok(path)
}

/// Re-point both endpoints of every edge whose id appears in the table. Endpoints absent from it are
/// copied byte-for-byte, which is what keeps memory ids and `skill:` endpoints intact.
fn rewrite_graph(by_old: &HashMap<String, String>) -> anyhow::Result<usize> {
    if by_old.is_empty() {
        return Ok(0);
    }
    let path = config::graph_path();
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(0);
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
        for col in cols.iter_mut().take(2) {
            if let Some(new) = by_old.get(col.trim()) {
                *col = new.clone();
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

fn opted_out() -> bool {
    std::env::var(OPT_OUT_ENV).is_ok_and(|v| !v.trim().is_empty() && v.trim() != "0")
}

/// The startup entry point: migrate every persona that has not been done yet, under that persona's
/// own store lock (the same lock `self_mem::write` takes, so a concurrent session never reads a path
/// that vanished mid-turn).
///
/// Returns `None` when nothing moved anywhere.
pub fn run_once_at_startup() -> Option<Report> {
    if opted_out() {
        return None;
    }
    let mut total = Report::default();
    for (slug, dir) in self_dirs() {
        if dir.join(DONE_FLAG).exists() {
            continue;
        }
        let lock_path = crate::core::workspace_txn::store_lock("persona_self", &slug);
        let Ok(_lock) = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
            &lock_path,
            std::time::Duration::from_secs(2),
        ) else {
            continue; // another session holds it — try again next launch
        };
        let plan = plan_for(&dir);
        let rep = apply_for(&slug, &plan);
        if rep.warnings.is_empty() || rep.renamed > 0 {
            let body = format!(
                "{}\trenamed={}\n",
                crate::memory::bloat::decay::today(),
                rep.renamed
            );
            let _ = crate::core::persist::atomic_write(&dir.join(DONE_FLAG), body.as_bytes());
        }
        total.renamed += rep.renamed;
        total.edges_rewritten += rep.edges_rewritten;
        total.warnings.extend(rep.warnings);
        if total.mapping_path.is_none() {
            total.mapping_path = rep.mapping_path;
        }
    }
    if total.renamed > 0 || !total.warnings.is_empty() {
        Some(total)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_mem(dir: &Path, stem: &str, kind: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join(format!("{stem}.md")),
            format!("---\nkind: {kind}\nimportance: 7\ncreated: 2026-08-01\nupdated: 2026-08-01\n---\n{body}\n"),
        )
        .unwrap();
    }

    /// The shredded-name case, on the exact shape found on disk, plus an archive row and a
    /// `persona:` graph edge that has to follow the rename.
    #[test]
    fn migrates_shredded_stems_and_repoints_persona_edges() {
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-selfmig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);
        std::env::remove_var(OPT_OUT_ENV);

        let dir = home.join("personas").join("kira.self");
        let shredded = "in-ng-i-d-ng-giao-2";
        write_mem(
            &dir,
            shredded,
            "insight",
            "Người dùng giao tiếp bằng tiếng Việt",
        );
        write_mem(
            &dir.join(".archive"),
            "ep-work-handled-b-y-gi",
            "episode",
            "work: handled \"bây giờ chạy build\" via 3 tool steps",
        );
        // `graph_path()` lives under `cli-memory/`, which a persona-only temp home has not created.
        std::fs::create_dir_all(config::graph_path().parent().unwrap()).unwrap();
        std::fs::write(
            config::graph_path(),
            format!(
                "persona:kira/{shredded}\tsome-memory-id\t1.0000\t2026-08-01\n\
                 skill:do-a-thing\tsome-memory-id\t0.5000\t2026-08-02\n"
            ),
        )
        .unwrap();

        let plan = plan_for(&dir);
        assert_eq!(plan.len(), 2, "{plan:?}");
        let rep = apply_for("kira", &plan);
        assert!(rep.warnings.is_empty(), "warnings: {:?}", rep.warnings);
        assert_eq!(rep.renamed, 2);

        let live: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter_map(|e| e.file_name().to_str().map(str::to_string))
            .filter(|n| n.ends_with(".md"))
            .collect();
        assert_eq!(live.len(), 1, "{live:?}");
        let got = &live[0];
        assert!(got.is_ascii(), "diacritics in the new name: {got}");
        assert!(
            got.starts_with("in-nguoi-dung-giao-tiep-bang-tieng"),
            "not folded into whole words: {got}"
        );
        assert_eq!(
            got.trim_end_matches(".md")
                .split('-')
                .filter(|w| w.chars().count() == 1)
                .count(),
            0,
            "still shredded: {got}"
        );

        let graph = std::fs::read_to_string(config::graph_path()).unwrap();
        assert!(
            !graph.contains(shredded),
            "old persona endpoint survives: {graph}"
        );
        assert!(
            graph.contains("persona:kira/in-nguoi-dung-giao"),
            "edge not re-pointed: {graph}"
        );
        assert!(
            graph.contains("skill:do-a-thing"),
            "unrelated namespace mangled: {graph}"
        );
        assert_eq!(rep.edges_rewritten, 1);

        let map = std::fs::read_to_string(rep.mapping_path.as_ref().unwrap()).unwrap();
        assert!(map.contains(shredded), "map missing the old stem: {map}");

        // Second pass is a no-op.
        assert!(plan_for(&dir).is_empty(), "plan not empty after migrating");

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The collision case: 12 real files shared one stem because the body's lead-in is boilerplate.
    /// After the pass each has its own name, and none of them falls back to the `-N` counter.
    #[test]
    fn twelve_way_collision_resolves_to_distinct_stems() {
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-selfmig-col-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);
        std::env::remove_var(OPT_OUT_ENV);

        let dir = home.join("personas").join("kira.self");
        let tasks = [
            "Hoàn thiện landing page",
            "Build and verify tsc",
            "Điều tra import graph",
            "Port backend Rust",
            "Tạo Rust workspace",
        ];
        for (i, t) in tasks.iter().enumerate() {
            let stem = if i == 0 {
                "ep-correction-user-redirected-me-todo".to_string()
            } else {
                format!("ep-correction-user-redirected-me-todo-{}", i + 1)
            };
            write_mem(
                &dir,
                &stem,
                "episode",
                &format!(
                    "correction: user redirected me — \"[todo-poke] Session todos are still incomplete — you may not finish yet. Incomplete: [>] {t}\""
                ),
            );
        }

        let plan = plan_for(&dir);
        assert_eq!(plan.len(), tasks.len(), "{plan:?}");
        let rep = apply_for("kira", &plan);
        assert!(rep.warnings.is_empty(), "warnings: {:?}", rep.warnings);

        let stems: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter_map(|e| e.path().file_stem()?.to_str().map(str::to_string))
            .filter(|s| s.starts_with("ep-"))
            .collect();
        assert_eq!(stems.len(), tasks.len(), "{stems:?}");
        let unique: std::collections::HashSet<&String> = stems.iter().collect();
        assert_eq!(unique.len(), tasks.len(), "stems still collide: {stems:?}");
        for s in &stems {
            assert!(
                !s.contains("correction-user-redirected"),
                "boilerplate still dominates: {s}"
            );
        }
        // Each file now names its own task.
        assert!(stems.iter().any(|s| s.contains("hoan-thien")), "{stems:?}");
        assert!(stems.iter().any(|s| s.contains("dieu-tra")), "{stems:?}");

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// The opt-out is shared with the memory pass, and the per-persona flag stops a second run.
    #[test]
    fn opt_out_and_done_flag_skip_the_pass() {
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("aizen-selfmig-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::env::set_var("AIZEN_HOME", &home);

        let dir = home.join("personas").join("kira.self");
        write_mem(&dir, "in-t-i-n-n-l-9", "insight", "Tôi nên nói lời cảm ơn");

        std::env::set_var(OPT_OUT_ENV, "1");
        assert!(run_once_at_startup().is_none(), "opt-out ignored");
        std::env::remove_var(OPT_OUT_ENV);

        let rep = run_once_at_startup().expect("a shredded stem should migrate");
        assert_eq!(rep.renamed, 1, "warnings: {:?}", rep.warnings);
        assert!(dir.join(DONE_FLAG).exists(), "flag not written");
        // Flagged → skipped, even though a new shredded file appears afterwards.
        write_mem(&dir, "in-t-i-c-n-ph", "insight", "Tôi cần phải kiểm tra");
        assert!(
            run_once_at_startup().is_none(),
            "done flag ignored on second run"
        );

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&home);
    }
}
