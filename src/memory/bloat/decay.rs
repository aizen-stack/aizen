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

/// Half-life LADDER (M1), as multipliers of the configured `recency_half_life_days`.
///
/// Indexed by `min(confirmations, 3)`, so at the default 30-day base a fact's half-life is
/// 30 / 90 / 270 / 720 days as it earns its first three confirmations. Multipliers rather than
/// literal day counts so `recency_half_life_days` stays a live knob instead of quietly dying.
///
/// Replaces `half_life·(1 + ln1p(reinforced))`. Two reasons the old curve was wrong: `reinforced`
/// counted RETRIEVALS (a fact merely shown got credit — see `record_retrieval`), and `ln1p` grows
/// without bound, so a fact retrieved often enough became effectively permanent. The ladder
/// saturates at three, and its input is `confirmations`, which only the "this actually helped"
/// report increments.
const HALF_LIFE_LADDER: [f64; 4] = [1.0, 3.0, 9.0, 24.0];

/// The effective half-life for one entry: its rung on [`HALF_LIFE_LADDER`].
fn half_life_for(e: &MemoryEntry, base_half_life: f64) -> f64 {
    base_half_life * HALF_LIFE_LADDER[(e.confirmations.min(3)) as usize]
}

/// Days since this fact was last USEFUL: `lastUsed`, else `updated`, else `created`.
///
/// `lastUsed` leads because it is the only one of the three that means "a human or the agent got
/// value from this". `updated` moves on bookkeeping writes, so a fact could look fresh purely
/// because something rewrote its frontmatter.
fn idle_days(e: &MemoryEntry, today: &str) -> Option<f64> {
    e.last_used
        .as_deref()
        .or(e.updated.as_deref())
        .or(e.created.as_deref())
        .and_then(|d| age_days(d, today))
}

/// Decay-adjust a base relevance score for one entry. Only INFERRED facts decay in time; a
/// curated fact (manual / user-explicit / imported) is returned unchanged — a deliberate statement
/// does not become less true by sitting still.
pub fn decayed_score(base: f64, e: &MemoryEntry, today: &str, half_life_days: f64) -> f64 {
    if e.source != ProvenanceKind::Inferred {
        return base;
    }
    match idle_days(e, today) {
        Some(age) => base * recency_factor(age, half_life_for(e, half_life_days)),
        None => base, // no usable date → don't penalize
    }
}

/// The salience multiplier for one entry: confirmation-earned rank, in [`S_min`, 1.0].
///
/// The recency term decays on the entry's OWN ladder half-life, so a well-confirmed fact keeps its
/// salience for as long as it keeps its rank — the two used to disagree, with salience decaying on
/// the base half-life while the score decayed on a stretched one.
///
/// Curated facts get a floor of 0.65 rather than 0.5. Without it, a fact the user typed by hand and
/// never searched for sat permanently at the neutral 0.5 — multiplied into the score, that is a
/// standing ×0.5 penalty for a statement the user made deliberately, which is the opposite of what
/// `decayed_score`'s curated exemption is trying to say.
pub fn salience_of(e: &MemoryEntry, today: &str, half_life_days: f64) -> f64 {
    let curated = e.source != ProvenanceKind::Inferred;
    let half_life = half_life_for(e, half_life_days);
    let used_recency = e
        .last_used
        .as_deref()
        .or(e.last_retrieved.as_deref())
        .and_then(|d| age_days(d, today))
        .map(|age| recency_factor(age, half_life))
        .unwrap_or(0.0);
    let s = salience(e.confirmations, used_recency);
    if curated {
        s.max(CURATED_SALIENCE_FLOOR)
    } else {
        s
    }
}

/// Salience floor for a curated fact — see [`salience_of`].
const CURATED_SALIENCE_FLOOR: f64 = 0.65;

/// How much this fact still counts: `decay · salience`, in (0, 1].
///
/// The single number the archive sweep and the ranking path share, so "faded enough to set aside"
/// and "ranked low" can never drift apart into two different notions of stale.
pub fn strength(e: &MemoryEntry, today: &str, half_life_days: f64) -> f64 {
    decayed_score(1.0, e, today, half_life_days) * salience_of(e, today, half_life_days)
}

/// The full evolved relevance: `bm25 · strength`. This is what the live retrieval path ranks by,
/// so facts rise and sink on confirmed usefulness, not age alone.
pub fn evolved_score(base: f64, e: &MemoryEntry, today: &str, half_life_days: f64) -> f64 {
    base * strength(e, today, half_life_days)
}

/// Minimum strength a fact must keep to stay in the live store.
pub const ARCHIVE_STRENGTH_FLOOR: f64 = 0.05;
/// A fact younger than this is never swept, however weak — it has not had a chance to be used yet.
pub const ARCHIVE_MIN_AGE_DAYS: f64 = 14.0;

/// Should this fact be moved aside (to the recoverable archive, never deleted)?
///
/// `strength < 0.05 ∧ age >= 14d ∧ is_active()`, and nothing else. In particular NOT
/// `confirmations == 0`: as an AND term, a single confirmation would make a fact permanently
/// unsweepable, so "total facts stops growing" would be structurally impossible no matter how the
/// rest of the system behaved. Curated facts stay safe through the FORMULA instead (`D = 1` and the
/// 0.65 salience floor keep them well above the floor), not through a special case.
pub fn should_archive(e: &MemoryEntry, today: &str, half_life_days: f64) -> bool {
    if !e.is_active() {
        return false; // already superseded — history, not clutter
    }
    match idle_days(e, today) {
        Some(age) if age >= ARCHIVE_MIN_AGE_DAYS => {
            strength(e, today, half_life_days) < ARCHIVE_STRENGTH_FLOOR
        }
        // Undated (hand-authored file with no frontmatter dates) → never swept. We cannot show it
        // has gone stale, and guessing costs the user a fact.
        _ => false,
    }
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
        assert!(
            stale < 0.02,
            "old inferred fact should be heavily down-weighted, got {stale}"
        );
    }

    #[test]
    fn curated_facts_never_decay() {
        let mut manual = inferred("2020-01-01");
        manual.source = ProvenanceKind::Manual;
        assert!((decayed_score(1.0, &manual, "2026-06-01", 30.0) - 1.0).abs() < 1e-9);
    }

    // ── P8 evolution ──────────────────────────────────────────────────────────

    #[test]
    fn confirmation_slows_decay_and_the_ladder_saturates() {
        // Same age, same source — the CONFIRMED fact retains more score (a longer ladder rung).
        // The input is `confirmations`, not `reinforced`: the old curve counted retrievals, so a
        // fact merely shown to the model earned permanence it never demonstrated.
        let today = "2026-06-01";
        let at = |c: u32| {
            let mut e = inferred("2026-05-01"); // 31 days old
            e.confirmations = c;
            decayed_score(1.0, &e, today, 30.0)
        };
        assert!(
            at(1) > at(0),
            "one confirmation must slow decay: {} vs {}",
            at(1),
            at(0)
        );
        assert!(at(3) > at(1));
        // Saturates at 3 — unbounded growth is what made facts immortal before.
        assert!(
            (at(9) - at(3)).abs() < 1e-12,
            "the ladder must cap at rung 3"
        );
    }

    #[test]
    fn salience_rewards_recent_use_and_floors_curated_facts() {
        let today = "2026-06-01";
        let mut unused = inferred(today);
        unused.confirmations = 0;
        unused.last_used = None;
        unused.last_retrieved = None;
        assert!(
            (salience_of(&unused, today, 30.0) - 0.5).abs() < 1e-9,
            "neutral at zero"
        );

        let mut used = inferred(today);
        used.confirmations = 5;
        used.last_used = Some(today.into());
        assert!(salience_of(&used, today, 30.0) > 0.7);

        // A hand-authored fact the user never searched for used to sit at the neutral 0.5 forever —
        // a standing halving of a statement they made deliberately. The floor removes that.
        let mut curated = inferred("2020-01-01");
        curated.source = ProvenanceKind::Manual;
        curated.confirmations = 0;
        curated.last_used = None;
        curated.last_retrieved = None;
        assert!(
            salience_of(&curated, today, 30.0) >= CURATED_SALIENCE_FLOOR,
            "curated facts sit on a floor, not at neutral"
        );
    }

    #[test]
    fn archive_sweep_spares_the_young_the_curated_and_the_already_superseded() {
        let today = "2026-06-01";

        // Faded and old → swept.
        let mut faded = inferred("2020-01-01");
        faded.confirmations = 0;
        assert!(
            should_archive(&faded, today, 30.0),
            "strength {}",
            strength(&faded, today, 30.0)
        );

        // Same fact, but younger than the grace window → spared, however weak.
        let mut young = inferred("2026-05-25"); // 7 days
        young.confirmations = 0;
        assert!(
            !should_archive(&young, today, 30.0),
            "a fact needs time to be used first"
        );

        // Curated → spared by the FORMULA (D=1, salience floored), not by a special case. A single
        // confirmation must NOT be what saves a fact: as an AND term that made facts unsweepable.
        let mut curated = inferred("2019-01-01");
        curated.source = ProvenanceKind::UserExplicit;
        assert!(!should_archive(&curated, today, 30.0));
        let mut confirmed_once = inferred("2019-01-01");
        confirmed_once.confirmations = 1;
        assert!(
            should_archive(&confirmed_once, today, 30.0),
            "one confirmation must not grant immortality (strength {})",
            strength(&confirmed_once, today, 30.0)
        );

        // Already superseded → history, not clutter.
        let mut retired = inferred("2020-01-01");
        retired.superseded_by = Some("newer".into());
        assert!(!should_archive(&retired, today, 30.0));

        // Undated → we cannot show it went stale, so we never guess.
        let mut undated = inferred("2020-01-01");
        undated.last_used = None;
        undated.updated = None;
        undated.created = None;
        assert!(!should_archive(&undated, today, 30.0));
    }

    #[test]
    fn last_used_leads_the_idle_clock() {
        // `updated` moves on bookkeeping rewrites, so a fact could look fresh purely because
        // something rewrote its frontmatter. `lastUsed` is the only one that means "this helped".
        let today = "2026-06-01";
        let mut e = inferred("2026-05-31"); // updated/created yesterday
        e.last_used = Some("2020-01-01".into()); // but genuinely unused for years
        assert!(
            strength(&e, today, 30.0) < 0.1,
            "a freshly-rewritten but long-unused fact must still read as faded, got {}",
            strength(&e, today, 30.0)
        );
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
        assert!(
            s_weak > s_strong,
            "reused weak ({s_weak}) should overtake stale strong ({s_strong})"
        );
    }
}
