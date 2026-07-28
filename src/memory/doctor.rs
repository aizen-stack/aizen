//! `aizen memory doctor` — read-only health report for the two-axis store.
//!
//! Everything the tier/anchor redesign can get wrong is invisible in normal use: a `place` fact with
//! no anchor never surfaces anywhere, an anchor pointing at a deleted directory never matches, a
//! `supersededBy` naming a purged id leaves a fact buried with no visible reason. None of these
//! throw an error — they just quietly subtract from what the store can recall. So the diagnosis is
//! its own command, and the whole of it is a PURE function over a loaded snapshot ([`diagnose`]):
//! the printing layer adds no logic, and every finding is testable from hand-built entries.

use crate::memory::bloat;
use crate::memory::path_scope::{self, Tier};
use crate::memory::store::MemoryEntry;

/// One thing wrong with a specific fact.
#[derive(Debug, Clone, PartialEq)]
pub enum Finding {
    /// A `place` fact with no anchor. No lineage can ever admit it — it is stored, searchable by
    /// explicit id, and dead to every automatic recall.
    OrphanPlace { id: String },
    /// `supersededBy` names an id that is not in the store. The fact is retired and the thing that
    /// replaced it is gone, so nothing explains why it is hidden.
    DanglingSupersededBy { id: String, missing: String },
    /// A live fact claims `supersedes: <id>` for an id that does not exist. Harmless to the live
    /// view, but the claim is a lie the history view will read.
    DanglingSupersedes { id: String, missing: String },
    /// A `place` anchor whose directory no longer exists on this machine. The fact is not wrong, it
    /// is unreachable: no cwd will ever sit under it again.
    AnchorMissing { id: String, anchor: String },
    /// Two LIVE facts that a local similarity check already considers near-duplicates. These are
    /// exactly what the batch pass exists to resolve, so seeing them here means either it has not
    /// run or it declined to act.
    LiveNearDuplicate {
        a: String,
        b: String,
        similarity: f64,
    },
}

impl Finding {
    /// The fact this finding is about (for grouping/sorting).
    pub fn subject(&self) -> &str {
        match self {
            Finding::OrphanPlace { id }
            | Finding::DanglingSupersededBy { id, .. }
            | Finding::DanglingSupersedes { id, .. }
            | Finding::AnchorMissing { id, .. } => id,
            Finding::LiveNearDuplicate { a, .. } => a,
        }
    }

    /// One line, in the imperative-free "what is wrong" voice the CLI prints.
    pub fn describe(&self) -> String {
        match self {
            Finding::OrphanPlace { id } => format!(
                "{id}: place fact with no anchor — no directory will ever match it (fix: `aizen memory edit {id}` or re-learn it here)"
            ),
            Finding::DanglingSupersededBy { id, missing } => format!(
                "{id}: retired by '{missing}', which is not in the store — nothing explains why it is hidden (fix: `aizen memory revive {id}`)"
            ),
            Finding::DanglingSupersedes { id, missing } => format!(
                "{id}: claims to supersede '{missing}', which does not exist — the claim affects the as-of history only"
            ),
            Finding::AnchorMissing { id, anchor } => format!(
                "{id}: anchored at {anchor}, which no longer exists on this machine — unreachable until that path returns"
            ),
            Finding::LiveNearDuplicate { a, b, similarity } => format!(
                "{a} ~ {b}: two live facts at similarity {similarity:.2} — `aizen memory reconcile` judges pairs like this"
            ),
        }
    }
}

/// Counts of the store's populations. Separate from [`Finding`] because these are not problems —
/// they are the numbers §8's first metric ("do live facts flatten while use grows?") is read from.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Counts {
    pub live: usize,
    pub superseded: usize,
    pub archived: usize,
    pub review: usize,
    pub user_tier: usize,
    pub device_tier: usize,
    pub place_tier: usize,
    /// Live place facts anchored at a path that is not an ancestor of the current cwd. Not a
    /// problem — most facts belong to other projects — but it explains a small recall block.
    pub inapplicable_here: usize,
}

/// The full report.
#[derive(Debug, Clone, Default)]
pub struct Report {
    pub counts: Counts,
    pub findings: Vec<Finding>,
    /// Device identity, so a user whose `device` facts vanished can see whether the id changed.
    pub device_id: String,
    pub device_source: String,
    pub device_also_read: Vec<String>,
    /// Date of the last batch reconciliation, if it has ever run.
    pub last_reconcile: Option<String>,
    pub pending_pairs: usize,
}

/// Diagnose a loaded snapshot. **Pure** apart from `dir_exists`, which is injected so the
/// anchor-liveness check is testable without touching the filesystem.
///
/// `all` is the whole entries dir (live + retired), `archived` the archive, `review` the queue.
pub fn diagnose(
    all: &[MemoryEntry],
    archived: &[MemoryEntry],
    review: &[MemoryEntry],
    lineage: &path_scope::Lineage,
    dir_exists: &dyn Fn(&str) -> bool,
) -> Report {
    let live = bloat::supersede::active(all);
    let all_ids: std::collections::HashSet<&str> = all.iter().map(|e| e.id.as_str()).collect();
    // A `supersededBy`/`supersedes` target may legitimately have been archived rather than deleted,
    // so "exists" means the entries dir OR the archive — otherwise every LRU eviction would report
    // as a dangling pointer.
    let known: std::collections::HashSet<&str> = all_ids
        .union(&archived.iter().map(|e| e.id.as_str()).collect())
        .copied()
        .collect();

    let mut findings = Vec::new();
    let mut counts = Counts {
        live: live.len(),
        superseded: all.len() - live.len(),
        archived: archived.len(),
        review: review.len(),
        ..Default::default()
    };

    for e in all {
        match e.tier {
            Tier::User => counts.user_tier += 1,
            Tier::Device => counts.device_tier += 1,
            Tier::Place => counts.place_tier += 1,
        }
        if let Some(by) = e
            .superseded_by
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !known.contains(by) {
                findings.push(Finding::DanglingSupersededBy {
                    id: e.id.clone(),
                    missing: by.to_string(),
                });
            }
        }
        if let Some(old) = e
            .supersedes
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            if !known.contains(old) {
                findings.push(Finding::DanglingSupersedes {
                    id: e.id.clone(),
                    missing: old.to_string(),
                });
            }
        }
    }

    // Placement problems are only worth reporting for LIVE facts: a retired place fact with no
    // anchor is history, and telling the user to fix it would be telling them to edit the past.
    for e in &live {
        if e.tier != Tier::Place {
            continue;
        }
        match e.anchor.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            None => findings.push(Finding::OrphanPlace { id: e.id.clone() }),
            Some(a) => {
                if !dir_exists(a) {
                    findings.push(Finding::AnchorMissing {
                        id: e.id.clone(),
                        anchor: a.to_string(),
                    });
                } else if !path_scope::is_ancestor(a, &lineage.cwd) {
                    counts.inapplicable_here += 1;
                }
            }
        }
    }

    // Live near-duplicates: the pairs the batch pass would judge. Same collector the pass uses, so
    // the number here IS the number `reconcile` would work on — a doctor that counted differently
    // would send the user to a command that then reports nothing to do.
    let pairs = crate::memory::learning::reconcile::collect_pairs(&live, &live);
    for p in &pairs {
        findings.push(Finding::LiveNearDuplicate {
            a: p.candidate_id.clone(),
            b: p.target_id.clone(),
            similarity: p.similarity,
        });
    }

    let dev = crate::core::device::current();
    Report {
        counts,
        findings,
        device_id: dev.id.clone(),
        device_source: dev.source.to_string(),
        device_also_read: dev.also_read.clone(),
        last_reconcile: crate::memory::learning::reconcile::last_run(),
        pending_pairs: pairs.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::store::MemoryType;
    use crate::memory::tokenize::tokenize;

    fn place(id: &str, anchor: Option<&str>, body: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            name: id.into(),
            mtype: MemoryType::Project,
            body: body.into(),
            tokens: tokenize(body),
            tier: Tier::Place,
            anchor: anchor.map(str::to_string),
            created: Some("2026-01-01".into()),
            ..Default::default()
        }
    }

    fn lineage_at(cwd: &str) -> path_scope::Lineage {
        path_scope::Lineage {
            cwd: cwd.into(),
            places: vec![cwd.into()],
            device: "dev-test".into(),
            home: None,
        }
    }

    #[test]
    fn reports_orphan_place_and_dangling_supersededby() {
        let mut retired = place("gone", Some("c:/work/proj"), "the api key lives in vault");
        retired.valid_to = Some("2026-02-01".into());
        retired.superseded_by = Some("never-existed".into());

        let all = vec![
            place("orphan", None, "this project builds with cmake"),
            place("fine", Some("c:/work/proj"), "the deploy target is fly"),
            retired,
        ];
        let r = diagnose(&all, &[], &[], &lineage_at("c:/work/proj/src"), &|_| true);

        assert!(
            r.findings.contains(&Finding::OrphanPlace {
                id: "orphan".into()
            }),
            "a place fact with no anchor is unreachable and must be reported: {:?}",
            r.findings
        );
        assert!(
            r.findings.contains(&Finding::DanglingSupersededBy {
                id: "gone".into(),
                missing: "never-existed".into()
            }),
            "a retired fact whose replacement is absent must be reported: {:?}",
            r.findings
        );
        // The healthy fact produces nothing.
        assert!(
            !r.findings.iter().any(|f| f.subject() == "fine"),
            "a well-formed anchored fact is not a finding: {:?}",
            r.findings
        );
        assert_eq!(r.counts.live, 2, "the retired fact is not live");
        assert_eq!(r.counts.superseded, 1);
    }

    #[test]
    fn a_missing_anchor_directory_is_a_finding_not_a_silent_loss() {
        // The fact is well-formed; the machine changed under it. Distinguishing this from an orphan
        // matters because the fix is different (restore/rename the directory, not edit the fact).
        let all = vec![place(
            "stale",
            Some("d:/old/checkout"),
            "tests run with cargo nextest",
        )];
        let r = diagnose(&all, &[], &[], &lineage_at("c:/work/proj"), &|p| {
            p != "d:/old/checkout"
        });
        assert_eq!(
            r.findings,
            vec![Finding::AnchorMissing {
                id: "stale".into(),
                anchor: "d:/old/checkout".into()
            }]
        );
    }

    #[test]
    fn a_fact_anchored_elsewhere_is_counted_not_flagged() {
        // Most of a real store belongs to other projects. Reporting each one as a problem would bury
        // the actual findings, so this is a COUNT — it explains a short recall block without
        // implying anything is broken.
        let all = vec![place(
            "other",
            Some("c:/work/elsewhere"),
            "that repo pins node 18",
        )];
        let r = diagnose(&all, &[], &[], &lineage_at("c:/work/proj"), &|_| true);
        assert!(r.findings.is_empty(), "not a problem: {:?}", r.findings);
        assert_eq!(r.counts.inapplicable_here, 1);
    }

    #[test]
    fn an_archived_replacement_is_not_a_dangling_pointer() {
        // LRU eviction moves a fact to the archive without touching the pointers aimed at it. If
        // "exists" meant only the live dir, every eviction would surface here as corruption.
        let mut retired = place("old", Some("c:/w"), "staging runs in frankfurt");
        retired.valid_to = Some("2026-02-01".into());
        retired.superseded_by = Some("evicted".into());
        let archived = vec![place("evicted", Some("c:/w"), "staging runs in dublin")];
        let r = diagnose(&[retired], &archived, &[], &lineage_at("c:/w"), &|_| true);
        assert!(
            !r.findings
                .iter()
                .any(|f| matches!(f, Finding::DanglingSupersededBy { .. })),
            "an archived target still explains the retirement: {:?}",
            r.findings
        );
    }
}
