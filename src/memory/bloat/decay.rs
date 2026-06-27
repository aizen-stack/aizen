//! Recency decay for RANKING (never for deletion). Stale auto-learned facts sink below
//! the top-K so they stop crowding retrieval — but they're still there, and eviction is
//! the LRU cap's job (see `caps`), not decay's. Curated facts (manual / user-explicit /
//! imported) never decay: a deliberate fact stays as relevant as the day it was written.

use crate::memory::provenance::ProvenanceKind;
use crate::memory::score::{recency_factor, salience};
use crate::memory::store::MemoryEntry;

/// Whole days between `date` (`YYYY-MM-DD`) and `today` (same format). Negative clamped to 0.
/// Returns `None` if either date can't be parsed.
pub fn age_days(date: &str, today: &str) -> Option<f64> {
    let d = chrono::NaiveDate::parse_from_str(date.trim(), "%Y-%m-%d").ok()?;
    let t = chrono::NaiveDate::parse_from_str(today.trim(), "%Y-%m-%d").ok()?;
    Some((t - d).num_days().max(0) as f64)
}

/// Today as `YYYY-MM-DD` (local).
pub fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// Decay-adjust a base relevance score for one entry. Only INFERRED facts decay; their
/// age is taken from `updated` (last reinforcement) else `created`. Everything else is
/// returned unchanged.
///
/// **Reinforcement-scaled (P8):** the effective half-life is stretched by reuse —
/// `half_life·(1 + ln1p(reinforced))` — so a fact reinforced 10× decays ~2.4× slower than a
/// never-reused one. Useful facts persist; one-off noise still sinks on the default half-life.
pub fn decayed_score(base: f64, e: &MemoryEntry, today: &str, half_life_days: f64) -> f64 {
    if e.source != ProvenanceKind::Inferred {
        return base;
    }
    let date = e.updated.as_deref().or(e.created.as_deref());
    let half_life = half_life_days * (1.0 + (e.reinforced as f64).ln_1p());
    match date.and_then(|d| age_days(d, today)) {
        Some(age) => base * recency_factor(age, half_life),
        None => base, // no usable date → don't penalize
    }
}

/// The salience multiplier for one entry (P8): reuse-earned rank, in [0.5, 1.0]. The recency
/// term decays `last_retrieved` on the same half-life as the rank decay.
pub fn salience_of(e: &MemoryEntry, today: &str, half_life_days: f64) -> f64 {
    let retrieved_recency = e
        .last_retrieved
        .as_deref()
        .and_then(|d| age_days(d, today))
        .map(|age| recency_factor(age, half_life_days))
        .unwrap_or(0.0);
    salience(e.reinforced, retrieved_recency)
}

/// The full evolved relevance: `bm25 · decay · salience` (P8). This is what the live retrieval
/// path ranks by, so facts rise/sink on *reuse and reinforcement*, not age alone.
pub fn evolved_score(base: f64, e: &MemoryEntry, today: &str, half_life_days: f64) -> f64 {
    decayed_score(base, e, today, half_life_days) * salience_of(e, today, half_life_days)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::store::MemoryEntry;

    fn inferred(updated: &str) -> MemoryEntry {
        MemoryEntry {
            source: ProvenanceKind::Inferred,
            updated: Some(updated.into()),
            created: Some(updated.into()),
            ..Default::default()
        }
    }

    #[test]
    fn age_is_whole_days() {
        assert_eq!(age_days("2026-06-01", "2026-06-01"), Some(0.0));
        assert_eq!(age_days("2026-05-02", "2026-06-01"), Some(30.0));
        assert_eq!(age_days("2026-07-01", "2026-06-01"), Some(0.0)); // future clamped
    }

    #[test]
    fn fresh_inferred_barely_decays_old_sinks() {
        let fresh = decayed_score(1.0, &inferred("2026-06-01"), "2026-06-01", 30.0);
        let stale = decayed_score(1.0, &inferred("2026-01-01"), "2026-06-01", 30.0);
        assert!((fresh - 1.0).abs() < 1e-9);
        assert!(stale < 0.02, "old inferred fact should be heavily down-weighted, got {stale}");
    }

    #[test]
    fn curated_facts_never_decay() {
        let mut manual = inferred("2020-01-01");
        manual.source = ProvenanceKind::Manual;
        assert!((decayed_score(1.0, &manual, "2026-06-01", 30.0) - 1.0).abs() < 1e-9);
    }

    // ── P8 evolution ──────────────────────────────────────────────────────────

    #[test]
    fn reinforcement_slows_decay() {
        // Same age, same source — but the reinforced fact retains more score (longer half-life).
        let mut fresh = inferred("2026-05-01"); // 31 days old at 2026-06-01
        fresh.reinforced = 0;
        let mut reused = inferred("2026-05-01");
        reused.reinforced = 10;
        let s_fresh = decayed_score(1.0, &fresh, "2026-06-01", 30.0);
        let s_reused = decayed_score(1.0, &reused, "2026-06-01", 30.0);
        assert!(s_reused > s_fresh, "reinforced={} should decay slower (fresh={s_fresh} reused={s_reused})", reused.reinforced);
    }

    #[test]
    fn salience_of_rewards_recent_reuse() {
        let mut unused = inferred("2026-06-01");
        unused.reinforced = 0;
        unused.last_retrieved = None;
        let mut reused = inferred("2026-06-01");
        reused.reinforced = 5;
        reused.last_retrieved = Some("2026-06-01".into());
        assert!((salience_of(&unused, "2026-06-01", 30.0) - 0.5).abs() < 1e-9);
        assert!(salience_of(&reused, "2026-06-01", 30.0) > 0.7);
    }

    #[test]
    fn evolved_score_lets_reuse_overtake_a_stronger_raw_match() {
        // A weaker raw match (0.8) that's been reused beats a stronger raw match (1.0) that's
        // stale + never reused — exactly the lift the evolution engine exists to produce.
        let today = "2026-06-01";
        let mut strong_stale = inferred("2026-03-01"); // ~92 days old, unused
        strong_stale.reinforced = 0;
        let mut weak_reused = inferred(today);
        weak_reused.reinforced = 8;
        weak_reused.last_retrieved = Some(today.into());
        let s_strong = evolved_score(1.0, &strong_stale, today, 30.0);
        let s_weak = evolved_score(0.8, &weak_reused, today, 30.0);
        assert!(s_weak > s_strong, "reused weak ({s_weak}) should overtake stale strong ({s_strong})");
    }
}
