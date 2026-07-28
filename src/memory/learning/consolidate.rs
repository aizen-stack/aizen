//! Mem0-style consolidation: before persisting a new fact, check it against the
//! existing store. A near-duplicate UPDATEs (reinforces) the existing fact instead of
//! inserting a twin — this is the primary anti-bloat lever on the write path (the
//! semantic-dup variant via cosine lands with the dense tier in P5).

use crate::memory::score::lexical_score_tokens;
use crate::memory::store::MemoryEntry;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemOp {
    /// No similar fact exists — insert a new one.
    Add,
    /// A near-duplicate exists — reinforce it (bump recency/frequency) instead.
    Reinforce { id: String },
}

/// Lowest lexical score for a same-slot supersede on a **Correction** turn (below
/// [`learn_dedup_threshold`] so we do not reinforce the stale fact).
pub const SUPERSEDE_SLOT_MIN: f64 = 0.52;

/// Best lexical match in `existing` for `candidate_tokens`.
pub fn best_match(candidate_tokens: &[String], existing: &[MemoryEntry]) -> Option<(String, f64)> {
    let mut best: Option<(String, f64)> = None;
    for e in existing {
        let s = lexical_score_tokens(candidate_tokens, &e.tokens);
        if best.as_ref().map(|(_, bs)| s > *bs).unwrap_or(true) {
            best = Some((e.id.clone(), s));
        }
    }
    best
}

/// Decide ADD vs REINFORCE for `candidate_tokens` against the current store.
/// Picks the single best lexical match; reinforces if it clears `dedup_threshold`.
pub fn decide(
    candidate_tokens: &[String],
    existing: &[MemoryEntry],
    dedup_threshold: f64,
) -> MemOp {
    let mut best: Option<(&str, f64)> = None;
    for e in existing {
        let s = lexical_score_tokens(candidate_tokens, &e.tokens);
        if best.map(|(_, bs)| s > bs).unwrap_or(true) {
            best = Some((e.id.as_str(), s));
        }
    }
    match best {
        Some((id, s)) if s >= dedup_threshold => MemOp::Reinforce { id: id.to_string() },
        _ => MemOp::Add,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::store::MemoryType;
    use crate::memory::tokenize::tokenize;

    fn entry(id: &str, body: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            name: id.into(),
            mtype: MemoryType::User,
            body: body.into(),
            tokens: tokenize(body),
            ..Default::default()
        }
    }

    #[test]
    fn near_duplicate_reinforces() {
        let existing = vec![entry("prefers-pnpm", "prefers pnpm over npm")];
        let op = decide(&tokenize("prefers pnpm over npm"), &existing, 0.82);
        assert_eq!(
            op,
            MemOp::Reinforce {
                id: "prefers-pnpm".into()
            }
        );
    }

    #[test]
    fn novel_fact_adds() {
        let existing = vec![entry("prefers-pnpm", "prefers pnpm over npm")];
        let op = decide(&tokenize("deploys on fridays only"), &existing, 0.78);
        assert_eq!(op, MemOp::Add);
    }

    #[test]
    fn reworded_restatement_reinforces() {
        // the default 0.78 threshold absorbs a reworded restatement of the same fact…
        let existing = vec![entry(
            "prefers-pnpm",
            "prefers pnpm over npm for everything",
        )];
        let op = decide(&tokenize("prefers pnpm over npm"), &existing, 0.78);
        assert_eq!(
            op,
            MemOp::Reinforce {
                id: "prefers-pnpm".into()
            }
        );
    }

    #[test]
    fn different_topic_does_not_false_merge() {
        // …but distinct facts sharing a token or two stay separate.
        let existing = vec![entry("dark-theme", "prefers the dark theme")];
        let op = decide(&tokenize("prefers the light theme"), &existing, 0.78);
        assert_eq!(op, MemOp::Add);
    }

    #[test]
    fn empty_store_adds() {
        assert_eq!(decide(&tokenize("anything"), &[], 0.82), MemOp::Add);
    }
}
