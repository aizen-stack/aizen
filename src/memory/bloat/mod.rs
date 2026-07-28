//! Anti-bloat (P4). The plan's premise: disk/token bloat is a non-problem (thousands of
//! entries are still µs to scan); the REAL vectors are (1) frozen-core overflow — handled
//! in P2 by the hard cap + spill + deferred rebuild — and (2) near-duplicate precision-rot
//! in the long tail. This module addresses (2) and keeps the long tail bounded for years:
//!
//! - `dedup`     — MinHash near-dup detection on the write path (char-level guard).
//! - `decay`     — recency down-weighting in the RANK (inferred facts only; never deletes).
//! - `caps`      — per-tier LRU caps → recoverable archive (curated facts exempt).
//! - `supersede` — bi-temporal validity (`active` / `as_of`); facts are superseded, not deleted.

pub mod caps;
pub mod decay;
pub mod dedup;
pub mod supersede;

use crate::core::config;
use anyhow::Result;

/// Outcome of a `compact` pass.
#[derive(Debug, Default)]
pub struct CompactReport {
    pub archived: Vec<String>,
    /// Co-retrieval edges (P5) pruned because an endpoint fact no longer exists.
    pub edges_pruned: usize,
    /// Faded facts the strength sweep WOULD have archived, on a store where it has not been armed
    /// yet (the first pass is always a dry run — see [`sweep_faded`]).
    pub sweep_preview: Vec<String>,
}

/// Run a maintenance pass: enforce the inferred-fact LRU caps (archiving victims). Per-zone:
/// the global pool gets the full cap, each project zone half of it — bounded growth per project
/// without one project starving the others. Idempotent and safe to run often (the learning path
/// calls it best-effort after writes).
///
/// Also prunes the Hebbian co-retrieval graph (P5) of any edge whose endpoint is no longer a live
/// fact — best-effort, so a graph write failure never fails the compaction. Archived (superseded /
/// evicted) facts stay valid graph endpoints: only ids that have vanished from the store entirely
/// are pruned, so an association to a superseded fact survives for `as_of` history.
pub fn compact() -> Result<CompactReport> {
    let s = crate::memory::settings();
    let cap = s.learn_inferred_cap;
    let mut archived = caps::enforce_caps(cap, (cap / 2).max(1))?;
    let (swept, sweep_preview) = sweep_faded(s.recency_half_life_days)?;
    archived.extend(swept);
    let edges_pruned = prune_graph_best_effort();
    Ok(CompactReport {
        archived,
        edges_pruned,
        sweep_preview,
    })
}

/// Marker file that arms the strength sweep for this store.
fn sweep_armed_path() -> std::path::PathBuf {
    config::cli_memory_dir().join(".sweep-armed")
}

/// Most facts one sweep may set aside: whichever is larger of 10 or 5% of the live facts.
///
/// A sweep is driven by a formula over dates, so a wrong clock, a bad migration, or a mistuned
/// half-life could otherwise clear a store in one pass. The ceiling turns that from "everything is
/// gone" into "a few went early and the next pass takes a few more".
fn sweep_ceiling(live: usize) -> usize {
    (live / 20).max(10)
}

/// Set aside facts that have faded below the strength floor (`decay::should_archive`).
///
/// Returns `(archived, preview)`. The **first** sweep on any store only previews: it reports what it
/// would do, writes the arming marker, and moves nothing. An upgrade that starts quietly relocating
/// a user's facts on the strength of a brand-new formula is not something they can consent to after
/// the fact, and `lastUsed`/`confirmations` are seeded from legacy fields, so the first pass is
/// exactly the one whose inputs are least trustworthy.
///
/// Destination is `caps::archive_entry` — a rename into the recoverable archive, never a delete.
pub fn sweep_faded(half_life_days: f64) -> Result<(Vec<String>, Vec<String>)> {
    let today = decay::today();
    let all = crate::memory::store::load_all()?;
    let live = all.len();
    let mut faded: Vec<_> = all
        .into_iter()
        .filter(|e| decay::should_archive(e, &today, half_life_days))
        .collect();
    if faded.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    // Weakest first, so a capped sweep takes the most-faded rather than an arbitrary slice.
    faded.sort_by(|a, b| {
        decay::strength(a, &today, half_life_days)
            .partial_cmp(&decay::strength(b, &today, half_life_days))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    faded.truncate(sweep_ceiling(live));

    let armed = sweep_armed_path();
    if !armed.exists() {
        let _ = std::fs::create_dir_all(config::cli_memory_dir());
        let _ = std::fs::write(&armed, today.as_bytes());
        return Ok((Vec::new(), faded.into_iter().map(|e| e.id).collect()));
    }
    let mut archived = Vec::new();
    for e in &faded {
        // Best-effort per fact: one unwritable file must not abort the pass and leave the rest
        // un-swept every run.
        if let Ok(id) = caps::archive_entry(e) {
            archived.push(id);
        }
    }
    Ok((archived, Vec::new()))
}

/// Prune dangling graph edges against the union of the live store + the archive (a superseded fact
/// is still a legitimate association endpoint). Best-effort: any error → 0 pruned, never propagated.
pub fn prune_graph_best_effort() -> usize {
    use std::collections::HashSet;
    let mut live: HashSet<String> = HashSet::new();
    if let Ok(entries) = crate::memory::store::load_all() {
        live.extend(entries.into_iter().map(|e| e.id));
    }
    // The archive holds superseded/evicted rows; their ids remain valid edge endpoints.
    if let Ok(archived) = crate::memory::store::load_from(&config::archive_dir()) {
        live.extend(archived.into_iter().map(|e| e.id));
    }
    if live.is_empty() {
        return 0; // nothing loaded (or truly empty store) → don't prune the whole graph away
    }
    crate::memory::graph::prune(&live, &decay::today()).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::store::{self, LearnedWrite, MemoryType};

    fn with_temp_home<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-sweep-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("NEXTGEN_HOME", &dir);
        let out = f();
        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    /// Write an inferred fact and back-date its dates so it reads as long-idle.
    fn add_faded(name: &str, date: &str) -> String {
        let id = store::add_learned(&LearnedWrite {
            name,
            description: "",
            mtype: MemoryType::User,
            body: name,
            source: crate::memory::provenance::ProvenanceKind::Inferred,
            confidence: 0.8,
            session_id: "s",
            no_core: false,
            scope: None,
            subpath: None,
            tier: crate::memory::path_scope::Tier::User,
            anchor: None,
            device: None,
            supersedes: None,
        })
        .unwrap();
        let path = config::entries_dir().join(format!("{id}.md"));
        let content = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| {
                if l.starts_with("created:") {
                    format!("created: {date}")
                } else if l.starts_with("updated:") {
                    format!("updated: {date}")
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&path, content).unwrap();
        id
    }

    #[test]
    fn the_first_sweep_on_a_store_only_previews() {
        with_temp_home("dryrun", || {
            let id = add_faded("a fact nobody has needed in years", "2020-01-01");

            // Pass 1: reports what it WOULD do and moves nothing. An upgrade that quietly starts
            // relocating facts on a brand-new formula is not something the user can consent to
            // afterwards — and this is the pass whose inputs (seeded from legacy fields) are least
            // trustworthy.
            let (archived, preview) = sweep_faded(30.0).unwrap();
            assert!(archived.is_empty(), "nothing moves on the first pass");
            assert_eq!(preview, vec![id.clone()], "…but it says exactly what would");
            assert_eq!(
                store::load_all().unwrap().len(),
                1,
                "the fact is still live"
            );

            // Pass 2: now armed, so it actually sets the fact aside — into the archive, recoverably.
            let (archived, preview) = sweep_faded(30.0).unwrap();
            assert_eq!(archived, vec![id.clone()]);
            assert!(preview.is_empty());
            assert!(
                store::load_all().unwrap().is_empty(),
                "gone from the live store"
            );
            let arch = caps::list_archive().unwrap();
            assert_eq!(arch.len(), 1, "moved aside, never deleted");
            assert_eq!(arch[0].id, id);
        });
    }

    #[test]
    fn a_fresh_store_sweeps_nothing_and_stays_unarmed() {
        with_temp_home("fresh", || {
            add_faded("something learned today", &decay::today());
            let (archived, preview) = sweep_faded(30.0).unwrap();
            assert!(archived.is_empty() && preview.is_empty());
            // Nothing faded, so the marker must NOT be written — otherwise the FIRST real sweep,
            // whenever it comes, would skip its dry run.
            assert!(
                !sweep_armed_path().exists(),
                "an empty sweep must not arm the store"
            );
        });
    }

    #[test]
    fn one_sweep_can_never_clear_a_whole_store() {
        with_temp_home("ceiling", || {
            // 100 faded facts. A formula-driven sweep is one bad clock or mistuned half-life away
            // from taking everything, so the ceiling turns "your memory is gone" into "a few went
            // early and the next pass takes a few more".
            for i in 0..100 {
                add_faded(
                    &format!("stale fact number {i} about an old project"),
                    "2019-01-01",
                );
            }
            let _ = sweep_faded(30.0).unwrap(); // arm (dry run)
            let (archived, _) = sweep_faded(30.0).unwrap();
            assert_eq!(
                archived.len(),
                sweep_ceiling(100),
                "capped at 5% (min 10), got {}",
                archived.len()
            );
            assert!(
                archived.len() < 100,
                "a single pass must never take the lot"
            );
            assert!(
                !store::load_all().unwrap().is_empty(),
                "the store survives one bad sweep"
            );
        });
    }

    #[test]
    fn sweep_ceiling_is_five_percent_with_a_floor_of_ten() {
        assert_eq!(
            sweep_ceiling(0),
            10,
            "a tiny store still gets a usable floor"
        );
        assert_eq!(sweep_ceiling(100), 10, "5% of 100 == the floor");
        assert_eq!(sweep_ceiling(1000), 50);
    }
}
