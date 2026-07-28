//! Project-zone inventory + migration (`aizen zone migrate`, the startup legacy-zone hint).
//!
//! The slug keying changed 2026-07: the old key preferred `remote.origin.url` and fell back to a
//! raw `canonicalize` string, so *whether git spawned* (PATH luck) silently picked the key — one
//! checkout accumulated TWIN zones (memory scope tags, skills dir, codebase index, frozen core)
//! under different slugs. This module finds every artifact keyed by a legacy slug of the CURRENT
//! project and merges it into the current slug. Dry-run by default, `--apply` to execute, every
//! action reported. Nothing is destroyed: name clashes are moved aside with a `premigrated`
//! marker, never overwritten — the one exception is the codebase-index cache (derivable via
//! `/init`), where the older of two copies is dropped and the drop is reported.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::core::config;
use crate::memory::store;

/// Everything found on disk under one legacy slug.
pub struct LegacyZone {
    pub slug: String,
    pub skills_dir: Option<PathBuf>,
    pub skills_files: usize,
    pub codebase_index: Option<PathBuf>,
    pub core_active: Option<PathBuf>,
    pub core_next: Option<PathBuf>,
    pub entries: usize,
    pub review: usize,
    pub archive: usize,
    /// Saved sessions whose in-file provenance still names this legacy slug. Sessions are a FLAT
    /// pool (one dir for every project) keyed by metadata inside each file, so they are invisible to
    /// the per-slug directory sweep every other artifact here uses.
    pub sessions: usize,
}

impl LegacyZone {
    fn is_empty(&self) -> bool {
        self.skills_dir.is_none()
            && self.codebase_index.is_none()
            && self.core_active.is_none()
            && self.core_next.is_none()
            && self.entries == 0
            && self.review == 0
            && self.archive == 0
            && self.sessions == 0
    }

    /// One human line for the plan listing: which artifact kinds exist under this slug.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.skills_dir.is_some() {
            parts.push(format!("skills: {} file(s)", self.skills_files));
        }
        if self.codebase_index.is_some() {
            parts.push("codebase index".to_string());
        }
        if self.core_active.is_some() {
            parts.push("frozen core (active)".to_string());
        }
        if self.core_next.is_some() {
            parts.push("frozen core (staged)".to_string());
        }
        if self.sessions > 0 {
            parts.push(format!(
                "{} saved conversation{}",
                self.sessions,
                if self.sessions == 1 { "" } else { "s" }
            ));
        }
        if self.entries + self.review + self.archive > 0 {
            parts.push(format!(
                "memory: {} entr{}, {} review, {} archived",
                self.entries,
                if self.entries == 1 { "y" } else { "ies" },
                self.review,
                self.archive
            ));
        }
        format!("{}  —  {}", self.slug, parts.join(" · "))
    }
}

pub struct ZonePlan {
    pub current_slug: String,
    pub legacy: Vec<LegacyZone>,
}

/// What `apply` did (or a warning per artifact it could not move — it never stops halfway, and
/// every operation is a rename/move, so a failure leaves that artifact exactly where it was).
pub struct MigrateReport {
    pub actions: Vec<String>,
    pub warnings: Vec<String>,
}

/// `~/.aizen/skills/p/<slug>/` for an EXPLICIT slug (the current-project variant lives in
/// `skills::project_zone_dir`).
fn skills_zone_dir(slug: &str) -> PathBuf {
    crate::skills::skills_dir().join("p").join(slug)
}

/// Full inventory: for each legacy slug the old keying could have produced here, what exists.
/// Slugs with zero artifacts are omitted.
pub fn plan() -> Result<ZonePlan> {
    plan_for(config::project_slug(), config::legacy_slug_candidates())
}

/// The injectable core of [`plan`]: candidates are a parameter so tests can exercise the merge
/// machinery on any host (on Unix, canonicalize has no verbatim spelling, so a sandbox often has
/// ZERO real legacy candidates — the machinery still has to be provable there).
fn plan_for(current_slug: String, candidates: Vec<String>) -> Result<ZonePlan> {
    let stores = [
        store::load_from(&config::entries_dir())?,
        store::load_from(&config::review_dir())?,
        store::load_from(&config::archive_dir())?,
    ];
    let mut legacy = Vec::new();
    for slug in candidates {
        let skills = skills_zone_dir(&slug);
        let counts: Vec<usize> = stores
            .iter()
            .map(|s| {
                s.iter()
                    .filter(|e| e.scope.as_deref() == Some(slug.as_str()))
                    .count()
            })
            .collect();
        let z = LegacyZone {
            skills_files: if skills.is_dir() {
                dir_file_count(&skills)
            } else {
                0
            },
            skills_dir: Some(skills).filter(|p| p.is_dir()),
            codebase_index: Some(config::codebase_index_path(&slug)).filter(|p| p.is_file()),
            core_active: Some(config::core_active_path(&slug)).filter(|p| p.is_file()),
            core_next: Some(config::core_next_path(&slug)).filter(|p| p.is_file()),
            entries: counts[0],
            review: counts[1],
            archive: counts[2],
            sessions: crate::count_sessions_of_slug(&slug),
            slug,
        };
        if !z.is_empty() {
            legacy.push(z);
        }
    }
    Ok(ZonePlan {
        current_slug,
        legacy,
    })
}

/// Existence-only probe for the startup hint — file/dir checks, no store scan (a zone whose only
/// artifacts are entry tags still shows up in `aizen zone migrate`'s full plan). Returns the
/// first legacy slug with anything on disk.
pub fn quick_legacy_probe() -> Option<String> {
    config::legacy_slug_candidates().into_iter().find(|slug| {
        skills_zone_dir(slug).is_dir()
            || config::codebase_index_path(slug).is_file()
            || config::core_active_path(slug).is_file()
            || config::core_next_path(slug).is_file()
    })
}

/// Execute the merge. Per-artifact failures become warnings (the report says exactly what moved
/// and what didn't) rather than aborting a half-done migration.
pub fn apply(plan: &ZonePlan) -> MigrateReport {
    let mut rep = MigrateReport {
        actions: Vec::new(),
        warnings: Vec::new(),
    };
    for z in &plan.legacy {
        if let Some(src) = &z.skills_dir {
            let dst = skills_zone_dir(&plan.current_slug);
            merge_skills(src, &dst, &z.slug, &mut rep);
        }
        if let Some(src) = &z.codebase_index {
            let dst = config::codebase_index_path(&plan.current_slug);
            merge_index_cache(src, &dst, &mut rep);
        }
        for (src, dst, label) in [
            (
                &z.core_active,
                config::core_active_path(&plan.current_slug),
                "frozen core (active)",
            ),
            (
                &z.core_next,
                config::core_next_path(&plan.current_slug),
                "frozen core (staged)",
            ),
        ] {
            if let Some(src) = src {
                merge_keep_current(src, &dst, label, &mut rep);
            }
        }
        retag_store_scope(&z.slug, &plan.current_slug, &mut rep);
        // Sessions are the one artifact keyed INSIDE the file rather than by directory. Without this
        // leg a renamed/moved checkout kept every pre-move conversation labeled "from <old dir>" and
        // made `/resume` warn that the user's own transcripts belonged to another project — with no
        // way to heal it, because migrate only ever touched slug-keyed paths.
        let mut session_errs: Vec<String> = Vec::new();
        let moved = crate::retag_sessions_of_slug(&z.slug, &mut |e| session_errs.push(e));
        if moved > 0 {
            rep.actions.push(format!(
                "sessions: re-homed {moved} saved conversation(s) {} → {}",
                z.slug, plan.current_slug
            ));
        }
        for e in session_errs {
            rep.warnings
                .push(format!("sessions: could not re-home {e}"));
        }
    }
    rep
}

/// Move every skill file from the legacy zone dir into the current one. Whole-dir rename when the
/// target doesn't exist yet; else per-file, a name clash landing as `<stem>.from-<legacy>.md` so
/// both versions survive for the user to reconcile.
fn merge_skills(src: &Path, dst: &Path, legacy_slug: &str, rep: &mut MigrateReport) {
    if !dst.exists() {
        let moved = dst
            .parent()
            .map(|p| fs::create_dir_all(p).context("creating skills zone parent"))
            .unwrap_or(Ok(()))
            .and_then(|_| fs::rename(src, dst).context("renaming skills zone"));
        match moved {
            Ok(_) => rep
                .actions
                .push(format!("skills: {} → {}", src.display(), dst.display())),
            Err(e) => rep
                .warnings
                .push(format!("skills: could not move {}: {e:#}", src.display())),
        }
        return;
    }
    let rd = match fs::read_dir(src) {
        Ok(rd) => rd,
        Err(e) => {
            rep.warnings
                .push(format!("skills: could not read {}: {e:#}", src.display()));
            return;
        }
    };
    for ent in rd.flatten() {
        let from = ent.path();
        let name = ent.file_name().to_string_lossy().into_owned();
        let mut to = dst.join(&name);
        if to.exists() {
            let stem = Path::new(&name)
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| name.clone());
            match unique_path(&dst.join(format!("{stem}.from-{legacy_slug}.md"))) {
                Some(p) => to = p,
                None => {
                    rep.warnings.push(format!(
                        "skill: no free clash name for {} — left in place at {}",
                        name,
                        from.display()
                    ));
                    continue;
                }
            }
        }
        match fs::rename(&from, &to) {
            Ok(_) => rep
                .actions
                .push(format!("skill: {name} → {}", to.display())),
            Err(e) => rep
                .warnings
                .push(format!("skill: could not move {}: {e:#}", from.display())),
        }
    }
    match fs::remove_dir(src) {
        Ok(_) => rep
            .actions
            .push(format!("skills: removed emptied {}", src.display())),
        Err(_) => rep
            .actions
            .push(format!("skills: left non-empty {}", src.display())),
    }
}

/// The codebase index is a derivable cache (`/init` rebuilds it), so a clash keeps whichever copy
/// is newer and DROPS the other — the only deletion in the whole migration, and it says so.
fn merge_index_cache(src: &Path, dst: &Path, rep: &mut MigrateReport) {
    if !dst.exists() {
        match fs::rename(src, dst) {
            Ok(_) => rep.actions.push(format!(
                "codebase index: {} → {}",
                src.display(),
                dst.display()
            )),
            Err(e) => rep.warnings.push(format!(
                "codebase index: could not move {}: {e:#}",
                src.display()
            )),
        }
        return;
    }
    if mtime(src) > mtime(dst) {
        // Two fallible steps — report exactly how far it got, because "merge failed" alone would
        // hide that the current copy may already be gone (it's a cache either way: /init rebuilds).
        if let Err(e) = fs::remove_file(dst) {
            rep.warnings.push(format!(
                "codebase index: could not remove the older current copy {}: {e:#} — both copies left in place",
                dst.display()
            ));
            return;
        }
        match fs::rename(src, dst) {
            Ok(_) => rep.actions.push(
                "codebase index: legacy copy is newer — replaced the current one (cache — `/init` rebuilds it)".to_string(),
            ),
            Err(e) => rep.warnings.push(format!(
                "codebase index: current copy was removed but the legacy move failed: {e:#} — legacy remains at {}; run `/init` to rebuild",
                src.display()
            )),
        }
    } else {
        match fs::remove_file(src) {
            Ok(_) => rep.actions.push(
                "codebase index: dropped the older legacy copy (cache — `/init` rebuilds it)"
                    .to_string(),
            ),
            Err(e) => rep.warnings.push(format!(
                "codebase index: could not drop the older legacy copy {}: {e:#}",
                src.display()
            )),
        }
    }
}

/// Frozen-core files are distilled state, so a clash keeps the CURRENT zone's file and moves the
/// legacy one aside with a `premigrated` marker for the user to review — never silently merged.
fn merge_keep_current(src: &Path, dst: &Path, label: &str, rep: &mut MigrateReport) {
    if !dst.exists() {
        let moved = dst
            .parent()
            .map(|p| fs::create_dir_all(p).context("creating core dir"))
            .unwrap_or(Ok(()))
            .and_then(|_| fs::rename(src, dst).context("renaming core file"));
        match moved {
            Ok(_) => rep
                .actions
                .push(format!("{label}: {} → {}", src.display(), dst.display())),
            Err(e) => rep
                .warnings
                .push(format!("{label}: could not move {}: {e:#}", src.display())),
        }
        return;
    }
    let aside = match unique_path(&PathBuf::from(format!("{}.premigrated", src.display()))) {
        Some(p) => p,
        None => {
            rep.warnings.push(format!(
                "{label}: no free aside name — legacy left in place at {}",
                src.display()
            ));
            return;
        }
    };
    match fs::rename(src, &aside) {
        Ok(_) => rep.actions.push(format!(
            "{label}: kept the current zone's file; legacy moved aside → {} (review, then delete by hand)",
            aside.display()
        )),
        Err(e) => rep.warnings.push(format!("{label}: could not move {} aside: {e:#}", src.display())),
    }
}

/// Rewrite `scope: <legacy>` → `scope: <current>` across entries/review/archive via the
/// field-map-preserving `store::update`, so unknown frontmatter keys survive verbatim.
fn retag_store_scope(legacy_slug: &str, current_slug: &str, rep: &mut MigrateReport) {
    for (dir, label) in [
        (config::entries_dir(), "entries"),
        (config::review_dir(), "review"),
        (config::archive_dir(), "archive"),
    ] {
        let loaded = match store::load_from(&dir) {
            Ok(l) => l,
            Err(e) => {
                rep.warnings.push(format!(
                    "memory {label}: could not read {}: {e:#}",
                    dir.display()
                ));
                continue;
            }
        };
        let mut n = 0usize;
        for e in loaded
            .iter()
            .filter(|e| e.scope.as_deref() == Some(legacy_slug))
        {
            let patch = store::EntryPatch {
                scope: Some(Some(current_slug.to_string())),
                // Bookkeeping-only: the fact didn't change, so its aging clock must not either.
                preserve_updated: true,
                ..Default::default()
            };
            match store::update(e, &patch) {
                Ok(_) => n += 1,
                Err(err) => rep
                    .warnings
                    .push(format!("memory {label}: {} not retagged: {err:#}", e.id)),
            }
        }
        if n > 0 {
            rep.actions.push(format!(
                "memory {label}: retagged {n} fact(s) {legacy_slug} → {current_slug}"
            ));
        }
    }
}

fn dir_file_count(dir: &Path) -> usize {
    fs::read_dir(dir)
        .map(|rd| rd.flatten().count())
        .unwrap_or(0)
}

fn mtime(p: &Path) -> std::time::SystemTime {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(std::time::UNIX_EPOCH)
}

/// First non-existing variant of `p` (`p`, `p.2`, `p.3`, …). `None` when every variant is taken —
/// callers must then SKIP the move and warn: falling back to an existing path would rename onto
/// it and break the never-overwrite guarantee this module promises.
fn unique_path(p: &Path) -> Option<PathBuf> {
    if !p.exists() {
        return Some(p.to_path_buf());
    }
    (2..1000)
        .map(|i| PathBuf::from(format!("{}.{i}", p.display())))
        .find(|cand| !cand.exists())
}

#[cfg(test)]
mod tests {
    use super::*;

    // The env-sandbox helpers mirror memory::store tests: pin AIZEN_HOME + NG_PROJECT_ROOT under
    // a temp dir while holding the process-wide TEST_HOME_LOCK.
    fn with_sandbox<T>(f: impl FnOnce(&Path) -> T) -> T {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let sandbox =
            std::env::temp_dir().join(format!("ng-zones-{}-{:x}", std::process::id(), rand_tag()));
        let home = sandbox.join("home");
        let root = sandbox.join("proj");
        std::fs::create_dir_all(&home).unwrap();
        // canonicalize needs the dir to exist BEFORE the first project_slug call (the
        // project-slug-canonicalize flake class).
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("AIZEN_HOME", &home);
        std::env::set_var("NG_PROJECT_ROOT", &root);
        let out = f(&root);
        std::env::remove_var("AIZEN_HOME");
        std::env::remove_var("NG_PROJECT_ROOT");
        let _ = std::fs::remove_dir_all(&sandbox);
        out
    }

    fn rand_tag() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .subsec_nanos() as u64
    }

    #[test]
    fn migrate_moves_skills_index_core_and_retags_entries() {
        with_sandbox(|_root| {
            let current = config::project_slug();
            // A fabricated legacy twin, injected via `plan_for` — host-independent (on Unix the
            // real candidate list is often empty because canonicalize has no verbatim spelling).
            let legacy = format!(
                "{}-deadbeef",
                current.rsplit_once('-').map(|(n, _)| n).unwrap_or("proj")
            );
            assert_ne!(current, legacy);

            // Legacy artifacts: one skill, one codebase index, one active core, one tagged entry.
            let lskills = skills_zone_dir(&legacy);
            std::fs::create_dir_all(&lskills).unwrap();
            std::fs::write(lskills.join("deploy.md"), "# skill\n").unwrap();
            let lindex = config::codebase_index_path(&legacy);
            std::fs::create_dir_all(lindex.parent().unwrap()).unwrap();
            std::fs::write(&lindex, "{}").unwrap();
            let lcore = config::core_active_path(&legacy);
            std::fs::create_dir_all(lcore.parent().unwrap()).unwrap();
            std::fs::write(&lcore, "core\n").unwrap();
            store::add_scoped(
                "fact one",
                "",
                store::MemoryType::Project,
                "the body",
                Some(&legacy),
            )
            .expect("seed scoped entry");

            let p = plan_for(current.clone(), vec![legacy.clone()]).expect("plan");
            assert_eq!(p.current_slug, current);
            assert_eq!(p.legacy.len(), 1, "exactly the fabricated legacy zone");
            let z = &p.legacy[0];
            assert_eq!(z.slug, legacy);
            assert!(
                z.skills_dir.is_some() && z.codebase_index.is_some() && z.core_active.is_some()
            );
            assert_eq!(z.entries, 1);

            let rep = apply(&p);
            assert!(rep.warnings.is_empty(), "warnings: {:?}", rep.warnings);
            assert!(skills_zone_dir(&current).join("deploy.md").is_file());
            assert!(
                !lskills.exists(),
                "legacy skills dir removed after whole-dir rename"
            );
            assert!(config::codebase_index_path(&current).is_file());
            assert!(!lindex.exists());
            assert!(config::core_active_path(&current).is_file());
            assert!(!lcore.exists());
            let entries = store::load_from(&config::entries_dir()).unwrap();
            assert_eq!(entries.len(), 1);
            assert_eq!(entries[0].scope.as_deref(), Some(current.as_str()));

            // Second plan is clean: migration converges to nothing-to-do.
            let p2 = plan_for(current.clone(), vec![legacy.clone()]).expect("second plan");
            assert!(p2.legacy.is_empty(), "no legacy artifacts after apply");
        });
    }

    /// Windows-only: the verbatim-`\\?\` spelling is what canonicalize returns there, which is
    /// exactly the legacy key class this machine population accumulated. On Unix canonicalize
    /// has no divergent spelling, so a sandbox legitimately has zero real candidates.
    #[cfg(windows)]
    #[test]
    fn quick_probe_spots_a_verbatim_keyed_legacy_zone() {
        with_sandbox(|root| {
            assert!(
                quick_legacy_probe().is_none(),
                "clean sandbox has no legacy artifacts"
            );
            let verbatim = std::fs::canonicalize(root).unwrap().display().to_string();
            let name = root.file_name().unwrap().to_str().unwrap().to_string();
            let legacy = crate::core::config::slug_for_key(&name, &verbatim);
            assert_ne!(
                legacy,
                config::project_slug(),
                "verbatim spelling must be a LEGACY key"
            );
            std::fs::create_dir_all(skills_zone_dir(&legacy)).unwrap();
            assert_eq!(
                quick_legacy_probe().as_deref(),
                Some(legacy.as_str()),
                "the probe must find the real on-disk artifact through the real candidate list"
            );
        });
    }

    #[test]
    fn clash_keeps_both_versions_never_overwrites() {
        with_sandbox(|_root| {
            let current = config::project_slug();
            let legacy = format!(
                "{}-0badcafe",
                current.rsplit_once('-').map(|(n, _)| n).unwrap_or("proj")
            );

            // Same skill name in BOTH zones with different bodies.
            for (slug, body) in [(&current, "current body"), (&legacy, "legacy body")] {
                let d = skills_zone_dir(slug);
                std::fs::create_dir_all(&d).unwrap();
                std::fs::write(d.join("deploy.md"), body).unwrap();
            }
            // Frozen core in both zones.
            for (slug, body) in [(&current, "core cur"), (&legacy, "core old")] {
                let p = config::core_active_path(slug);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, body).unwrap();
            }

            let rep = apply(&plan_for(current.clone(), vec![legacy.clone()]).unwrap());
            assert!(rep.warnings.is_empty(), "warnings: {:?}", rep.warnings);
            let dst = skills_zone_dir(&current);
            assert_eq!(
                std::fs::read_to_string(dst.join("deploy.md")).unwrap(),
                "current body"
            );
            let renamed = dst.join(format!("deploy.from-{legacy}.md"));
            assert_eq!(std::fs::read_to_string(&renamed).unwrap(), "legacy body");
            // Core: current kept, legacy moved aside with the premigrated marker.
            assert_eq!(
                std::fs::read_to_string(config::core_active_path(&current)).unwrap(),
                "core cur"
            );
            let aside = PathBuf::from(format!(
                "{}.premigrated",
                config::core_active_path(&legacy).display()
            ));
            assert_eq!(std::fs::read_to_string(&aside).unwrap(), "core old");
        });
    }
}
