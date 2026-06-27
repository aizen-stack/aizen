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

use crate::config;
use anyhow::Result;

/// Outcome of a `compact` pass.
#[derive(Debug, Default)]
pub struct CompactReport {
    pub archived: Vec<String>,
}

/// Run a maintenance pass: enforce the inferred-fact LRU cap (archiving victims).
/// Idempotent and safe to run often (the learning path calls it best-effort after writes).
pub fn compact() -> Result<CompactReport> {
    let cap = config::MemorySettings::default().learn_inferred_cap;
    Ok(CompactReport { archived: caps::enforce_caps(cap)? })
}
