//! Per-tier LRU caps with a recoverable archive. Bloat is bounded by ARCHIVING the
//! least-recently-reinforced auto-learned facts once they exceed the cap — they're moved
//! to `~/.nextgen/cli-memory/archive/`, never hard-deleted (`ng memory restore <id>`
//! brings one back). Curated facts (manual / user-explicit / imported) are EXEMPT: a
//! deliberately-authored fact is never auto-evicted.

use crate::core::config;
use crate::memory::path_scope::Tier;
use crate::memory::provenance::ProvenanceKind;
use crate::memory::store::{self, MemoryEntry};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

fn stem(p: &Path) -> String {
    p.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string()
}

/// A free `{id}.md` path under `dir`, appending `-2`, `-3`, … on collision.
///
/// Shared by every archive path in the tree (memory facts, persona self-memory, the review
/// queue) so that "move aside instead of delete" can never silently overwrite an earlier
/// archive file that happens to have the same stem.
pub(crate) fn unique_in(dir: &Path, id: &str) -> PathBuf {
    let p = dir.join(format!("{id}.md"));
    if !p.exists() {
        return p;
    }
    for n in 2..10_000 {
        let c = dir.join(format!("{id}-{n}.md"));
        if !c.exists() {
            return c;
        }
    }
    p
}

/// Move an entry's file into the recoverable archive. Returns the archived id.
pub fn archive_entry(e: &MemoryEntry) -> Result<String> {
    let adir = config::archive_dir();
    fs::create_dir_all(&adir).with_context(|| format!("creating {}", adir.display()))?;
    let dest = unique_in(&adir, &e.id);
    fs::rename(&e.path, &dest).with_context(|| format!("archiving {}", e.path.display()))?;
    Ok(stem(&dest))
}

/// Restore an archived entry back into the live store, **keeping its id**. Returns the restored id.
///
/// The id is not cosmetic: it is the key `supersededBy`, `supersedes`, and every co-retrieval graph
/// edge point at. An earlier version routed this through [`unique_in`], so restoring onto an
/// occupied name silently produced `<id>-2` — a fact that looked restored, answered searches, and
/// yet was invisible to every pointer aimed at it. So a collision is an ERROR the caller has to
/// resolve out loud with `--as <new-id>`; passing `as_id` accepts the rename and its consequences.
pub fn restore(id: &str, as_id: Option<&str>) -> Result<String> {
    let adir = config::archive_dir();
    let src = adir.join(format!("{}.md", id.to_lowercase()));
    if !src.exists() {
        anyhow::bail!("no archived memory '{id}' ({})", src.display());
    }
    let edir = config::entries_dir();
    fs::create_dir_all(&edir).with_context(|| format!("creating {}", edir.display()))?;
    let target = as_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(id)
        .to_lowercase();
    let dest = edir.join(format!("{target}.md"));
    if dest.exists() {
        anyhow::bail!(
            "'{target}' already exists in the live store — restoring would need a different id.\n\
             Re-run with `--as <new-id>`, but note that any `supersededBy`/`supersedes` pointer or \
             graph edge naming '{id}' will NOT follow the rename."
        );
    }
    fs::rename(&src, &dest).with_context(|| format!("restoring {}", src.display()))?;
    Ok(stem(&dest))
}

/// All archived entries.
pub fn list_archive() -> Result<Vec<MemoryEntry>> {
    store::load_from(&config::archive_dir())
}

/// Recency key: oldest-first by (`updated` else `created`), tie-broken by id for determinism.
fn recency_key(e: &MemoryEntry) -> (String, String) {
    let d = e
        .updated
        .clone()
        .or_else(|| e.created.clone())
        .unwrap_or_default();
    (d, e.id.clone())
}

/// The bucket a fact competes for cap space in.
///
/// Keyed on the TIER axis, not on the legacy `scope` slug. It has to be: every fact written since
/// phase 1 carries `scope: None`, so a scope-keyed bucket put the entire store into the single
/// "global" pool — one chatty project would then evict facts belonging to every other, which is the
/// precise failure per-zone caps were introduced to prevent.
fn cap_bucket(e: &MemoryEntry) -> (Tier, String) {
    let key = match e.tier {
        Tier::User => String::new(),
        Tier::Device => e.device.clone().unwrap_or_default(),
        // Place facts compete within their own subtree; an orphan (no anchor) shares the "" bucket,
        // which is fine — no lineage admits it, so it is only ever cap pressure on other orphans.
        Tier::Place => e.anchor.clone().unwrap_or_default(),
    };
    (e.tier, key)
}

/// Archive the oldest INFERRED active facts beyond the caps, PER PARTITION: the `user` tier gets
/// `user_cap`, each device and each place-anchor subtree gets `partition_cap` — one chatty project
/// can no longer evict another's (or the user's) facts. Curated facts are exempt and not counted.
/// Returns the ids archived (LRU victims).
pub fn enforce_caps(user_cap: usize, partition_cap: usize) -> Result<Vec<String>> {
    let all = store::load_all()?;
    let mut by_bucket: std::collections::HashMap<(Tier, String), Vec<MemoryEntry>> =
        std::collections::HashMap::new();
    for e in all
        .into_iter()
        .filter(|e| e.source == ProvenanceKind::Inferred && e.is_active())
    {
        by_bucket.entry(cap_bucket(&e)).or_default().push(e);
    }
    let mut archived = Vec::new();
    // Deterministic bucket order so test output is stable.
    let mut buckets: Vec<(Tier, String)> = by_bucket.keys().cloned().collect();
    buckets.sort_by(|a, b| (a.0.as_str(), &a.1).cmp(&(b.0.as_str(), &b.1)));
    for bucket in buckets {
        let mut inferred = by_bucket.remove(&bucket).unwrap_or_default();
        let cap = if bucket.0 == Tier::User {
            user_cap
        } else {
            partition_cap
        };
        if inferred.len() <= cap {
            continue;
        }
        inferred.sort_by_key(recency_key); // oldest first
        let victims = inferred.len() - cap;
        for e in inferred.into_iter().take(victims) {
            archived.push(archive_entry(&e)?);
        }
    }
    Ok(archived)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::store::{LearnedWrite, MemoryType};

    fn with_temp_home<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-caps-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        std::env::set_var("NEXTGEN_HOME", &dir);
        let out = f();
        std::env::remove_var("NEXTGEN_HOME");
        let _ = fs::remove_dir_all(&dir);
        out
    }

    fn add_inferred(name: &str, created: &str) {
        add_inferred_scoped(name, created, None)
    }

    fn add_inferred_scoped(name: &str, created: &str, scope: Option<&str>) {
        // The LRU caps are per-partition, so the tier axis is what these fixtures actually vary:
        // `scope: None` → a global/user fact, `Some(slug)` → a place fact in that zone.
        let (tier, anchor) = match scope {
            None => (crate::memory::path_scope::Tier::User, None),
            Some(s) => (crate::memory::path_scope::Tier::Place, Some(s.to_string())),
        };
        let w = LearnedWrite {
            name,
            mtype: MemoryType::User,
            body: name,
            scope: scope.map(str::to_string),
            tier,
            anchor,
            ..Default::default()
        };
        let id = store::add_learned(&w).unwrap();
        // back-date created/updated so the LRU order is deterministic in the test
        let path = config::entries_dir().join(format!("{id}.md"));
        let content = fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| {
                if l.starts_with("created:") {
                    format!("created: {created}")
                } else if l.starts_with("updated:") {
                    format!("updated: {created}")
                } else {
                    l.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(&path, content).unwrap();
    }

    #[test]
    fn cap_archives_oldest_keeps_newest() {
        with_temp_home("cap", || {
            add_inferred("fact-old", "2026-01-01");
            add_inferred("fact-mid", "2026-03-01");
            add_inferred("fact-new", "2026-06-01");
            let archived = enforce_caps(2, 1).unwrap();
            assert_eq!(archived.len(), 1, "exactly one over-cap victim");
            assert_eq!(archived[0], "fact-old", "LRU victim is the oldest");
            // archive round-trips
            assert_eq!(list_archive().unwrap().len(), 1);
            let restored = restore("fact-old", None).unwrap();
            assert_eq!(restored, "fact-old");
            assert!(list_archive().unwrap().is_empty());
            assert_eq!(store::load_all().unwrap().len(), 3);
        });
    }

    #[test]
    fn restore_keeps_id_or_errors() {
        with_temp_home("restore-id", || {
            add_inferred("fact-a", "2026-01-01");
            let live = store::load_all()
                .unwrap()
                .into_iter()
                .find(|e| e.id == "fact-a")
                .unwrap();
            archive_entry(&live).unwrap();

            // A free name restores under the ORIGINAL id — every `supersededBy` pointer and graph
            // edge aimed at `fact-a` keeps resolving.
            assert_eq!(restore("fact-a", None).unwrap(), "fact-a");

            // Now make the name occupied and archive a SECOND copy of it, so restoring has to
            // collide.
            let live = store::load_all()
                .unwrap()
                .into_iter()
                .find(|e| e.id == "fact-a")
                .unwrap();
            archive_entry(&live).unwrap();
            add_inferred("fact-a", "2026-02-01"); // squats the live name
            let err = restore("fact-a", None).expect_err("a colliding restore must not proceed");
            let msg = err.to_string();
            assert!(
                msg.contains("--as"),
                "the error must say how to resolve it: {msg}"
            );
            assert!(
                list_archive().unwrap().iter().any(|e| e.id == "fact-a"),
                "the archived copy stays put when the restore is refused"
            );

            // …and `--as` is the explicit opt-in to the rename.
            assert_eq!(
                restore("fact-a", Some("fact-a-revived")).unwrap(),
                "fact-a-revived"
            );
            assert!(store::load_all()
                .unwrap()
                .iter()
                .any(|e| e.id == "fact-a-revived"));
        });
    }

    #[test]
    fn curated_facts_exempt_from_cap() {
        with_temp_home("exempt", || {
            // a manual fact + two inferred, cap=1 → only inferred count, archive 1 inferred
            store::add("keepme", "", MemoryType::User, "a deliberate manual fact").unwrap();
            add_inferred("inf-old", "2026-01-01");
            add_inferred("inf-new", "2026-06-01");
            let archived = enforce_caps(1, 1).unwrap();
            assert_eq!(archived, vec!["inf-old".to_string()]);
            // manual fact untouched
            assert!(store::load_all().unwrap().iter().any(|e| e.id == "keepme"));
        });
    }

    #[test]
    fn caps_are_enforced_per_zone() {
        with_temp_home("zonecap", || {
            // global pool: 2 facts under a cap of 2 → untouched
            add_inferred("glob-old", "2026-01-01");
            add_inferred("glob-new", "2026-06-01");
            // one chatty project zone: 3 facts under a per-zone cap of 1 → its own 2 oldest archived
            add_inferred_scoped("za-old", "2026-01-01", Some("chatty-00000001"));
            add_inferred_scoped("za-mid", "2026-03-01", Some("chatty-00000001"));
            add_inferred_scoped("za-new", "2026-06-01", Some("chatty-00000001"));
            // another zone stays within its cap
            add_inferred_scoped("zb-only", "2026-01-01", Some("quiet-00000002"));

            let archived = enforce_caps(2, 1).unwrap();
            assert_eq!(
                archived,
                vec!["za-old".to_string(), "za-mid".to_string()],
                "only the over-cap zone loses its own oldest facts"
            );
            let live: Vec<String> = store::load_all()
                .unwrap()
                .into_iter()
                .map(|e| e.id)
                .collect();
            assert!(
                live.contains(&"glob-old".to_string()) && live.contains(&"glob-new".to_string()),
                "a chatty project cannot evict the global pool"
            );
            assert!(
                live.contains(&"zb-only".to_string()),
                "…nor another project's zone"
            );
        });
    }
}
