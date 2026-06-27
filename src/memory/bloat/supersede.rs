//! Bi-temporal validity: facts are never deleted, only superseded. A superseded fact
//! keeps `created` (valid_from) and gains `validTo` + `supersededBy`, so the history is
//! queryable: `active()` is the live view, `as_of(date)` reconstructs what was true then.
//!
//! `YYYY-MM-DD` strings compare lexicographically in chronological order, so date math
//! here is plain string comparison — no parsing needed.

use crate::memory::store::MemoryEntry;

/// The currently-valid view: facts with no `validTo`/`supersededBy`.
pub fn active(entries: &[MemoryEntry]) -> Vec<MemoryEntry> {
    entries.iter().filter(|e| e.is_active()).cloned().collect()
}

/// What was valid on `date` (`YYYY-MM-DD`): created on/before `date`, and either still
/// valid or superseded strictly after `date`.
pub fn as_of(entries: &[MemoryEntry], date: &str) -> Vec<MemoryEntry> {
    entries
        .iter()
        .filter(|e| {
            let born = e.created.as_deref().unwrap_or("0000-00-00");
            if born > date {
                return false;
            }
            match e.valid_to.as_deref() {
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

    #[test]
    fn active_excludes_superseded() {
        let all = vec![e("old", "2026-01-01", Some("2026-03-01")), e("cur", "2026-03-01", None)];
        let a = active(&all);
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].id, "cur");
    }

    #[test]
    fn as_of_reconstructs_history() {
        let all = vec![e("old", "2026-01-01", Some("2026-03-01")), e("cur", "2026-03-01", None)];
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
