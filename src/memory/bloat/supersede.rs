//! Bi-temporal validity: facts are never deleted, only superseded. A superseded fact
//! keeps `created` (valid_from) and gains `validTo` + `supersededBy`, so the history is
//! queryable: `active()` is the live view, `as_of(date)` reconstructs what was true then.
//!
//! `YYYY-MM-DD` strings compare lexicographically in chronological order, so date math
//! here is plain string comparison — no parsing needed.

use crate::memory::store::MemoryEntry;

/// The currently-valid view. A fact is hidden when EITHER side of the supersession says so:
///
/// - **backward** — it carries `validTo`/`supersededBy` (the retired row was stamped), or
/// - **forward** — some other LIVE fact carries `supersedes: <its id>`.
///
/// The forward direction is what lets a replacement be written in ONE atomic file write: the new
/// fact declares what it replaces, and the old row needs no edit at all. Without it, retiring a
/// fact meant two writes (add the new one, then stamp the old one) with a crash window in between
/// that left BOTH facts live — two rival answers to the same question, and nothing marking which
/// one won. Recording the claim on the survivor makes the operation single-write and therefore
/// crash-safe: either the new fact exists (and the old is hidden) or it doesn't (and nothing
/// changed). See `store::unsupersede` + `clear_supersedes_claims` for the inverse.
pub fn active(entries: &[MemoryEntry]) -> Vec<MemoryEntry> {
    // Only a LIVE claimant can hide its predecessor — otherwise reviving a fact whose replacement
    // was itself retired would be impossible (the dead claimant would keep it buried forever).
    let claimed: std::collections::HashSet<&str> = entries
        .iter()
        .filter(|e| e.is_active())
        .filter_map(|e| e.supersedes.as_deref())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    entries
        .iter()
        .filter(|e| e.is_active() && !claimed.contains(e.id.as_str()))
        .cloned()
        .collect()
}

/// What was valid on `date` (`YYYY-MM-DD`): created on/before `date`, and either still
/// valid or superseded strictly after `date`.
///
/// A single-write replacement carries no `validTo` on the retired row, so its end date has to be
/// read off the OTHER side: the claimant's `created` is the day the old fact stopped being true.
/// Without that, a fact retired the single-write way would still show up in every future `as-of`
/// view — the history would claim two contradictory facts were simultaneously true.
pub fn as_of(entries: &[MemoryEntry], date: &str) -> Vec<MemoryEntry> {
    // id → earliest date some live claimant declared it superseded (that claimant's birthday).
    let mut replaced_on: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for e in entries.iter().filter(|e| e.is_active()) {
        let Some(old) = e
            .supersedes
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let born = e.created.as_deref().unwrap_or("9999-99-99");
        replaced_on
            .entry(old)
            .and_modify(|d| *d = (*d).min(born))
            .or_insert(born);
    }
    entries
        .iter()
        .filter(|e| {
            let born = e.created.as_deref().unwrap_or("0000-00-00");
            if born > date {
                return false;
            }
            // Whichever end date exists, the fact is gone once we're at or past it.
            let end = e
                .valid_to
                .as_deref()
                .into_iter()
                .chain(replaced_on.get(e.id.as_str()).copied())
                .min();
            match end {
                Some(vt) => vt > date, // stopped being true strictly after the as-of date
                None => true,
            }
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::store::MemoryEntry;

    fn e(id: &str, created: &str, valid_to: Option<&str>) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            name: id.into(),
            created: Some(created.into()),
            valid_to: valid_to.map(str::to_string),
            superseded_by: valid_to.map(|_| "newer".to_string()),
            ..Default::default()
        }
    }

    /// A live fact that CLAIMS to replace `victim` — the one-write supersession shape.
    fn claimer(id: &str, created: &str, victim: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            name: id.into(),
            created: Some(created.into()),
            supersedes: Some(victim.into()),
            ..Default::default()
        }
    }

    #[test]
    fn active_excludes_superseded() {
        let all = vec![
            e("old", "2026-01-01", Some("2026-03-01")),
            e("cur", "2026-03-01", None),
        ];
        let a = active(&all);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].id, "cur");
    }

    #[test]
    fn a_forward_claim_retires_the_old_fact_without_touching_its_file() {
        // The one-write shape: the NEW fact carries `supersedes: old`, and `old`'s own frontmatter
        // is untouched (no validTo, no supersededBy). This is what removes the crash window —
        // there is no second write that can fail to land.
        let old = e("old", "2026-01-01", None);
        assert!(old.is_active(), "the old fact's own file still looks live");
        let all = vec![old, claimer("new", "2026-03-01", "old")];
        let live = active(&all);
        let ids: Vec<&str> = live.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["new"], "the claim alone is enough to retire it");
    }

    #[test]
    fn a_dead_claimer_cannot_retire_anything() {
        // If the replacement was itself superseded later, its claim dies with it — otherwise a
        // chain of corrections would leave the ORIGINAL fact permanently hidden by a fact that is
        // no longer true either.
        let all = vec![
            e("old", "2026-01-01", None),
            MemoryEntry {
                valid_to: Some("2026-04-01".into()),
                superseded_by: Some("newest".into()),
                ..claimer("mid", "2026-03-01", "old")
            },
        ];
        let live = active(&all);
        let ids: Vec<&str> = live.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(ids, vec!["old"], "a retired claimer releases its victim");
    }

    #[test]
    fn a_claim_on_a_missing_id_is_harmless() {
        // `supersedes` pointing at a purged/never-existing id must not hide anything (and must not
        // hide the claimer itself).
        let all = vec![claimer("new", "2026-03-01", "ghost")];
        assert_eq!(active(&all).len(), 1);
    }

    #[test]
    fn a_forward_claim_ends_the_old_fact_in_the_history_view_too() {
        // Same rule in `as_of`: before the replacement was written the old fact was true; after,
        // it is not. Without this the history view would show BOTH as valid today.
        let all = vec![
            e("old", "2026-01-01", None),
            claimer("new", "2026-03-01", "old"),
        ];
        let then = as_of(&all, "2026-02-01");
        let feb: Vec<&str> = then.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(feb, vec!["old"], "in February only the old fact existed");
        let now = as_of(&all, "2026-06-01");
        let jun: Vec<&str> = now.iter().map(|e| e.id.as_str()).collect();
        assert_eq!(jun, vec!["new"], "by June the claim has taken effect");
    }

    #[test]
    fn as_of_reconstructs_history() {
        let all = vec![
            e("old", "2026-01-01", Some("2026-03-01")),
            e("cur", "2026-03-01", None),
        ];
        // in February the old fact was still valid; the new one didn't exist yet
        let feb = as_of(&all, "2026-02-01");
        assert_eq!(feb.len(), 1);
        assert_eq!(feb[0].id, "old");
        // today both? no — old superseded; only cur
        let now = as_of(&all, "2026-06-01");
        assert_eq!(now.len(), 1);
        assert_eq!(now[0].id, "cur");
    }
}
