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
    let cap = config::MemorySettings::default().learn_inferred_cap;
    let archived = caps::enforce_caps(cap, (cap / 2).max(1))?;
    let edges_pruned = prune_graph_best_effort();
    Ok(CompactReport { archived, edges_pruned })
}

/// Prune dangling graph edges against the union of the live store + the archive (a superseded fact
/// is still a legitimate association endpoint). Best-effort: any error → 0 pruned, never propagated.
fn prune_graph_best_effort() -> usize {
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
