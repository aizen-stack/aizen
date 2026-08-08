//! Two-axis memory identity for the tier/anchor design.
//!
//! The old `scope` (slug-hash of project root) is replaced by a two-axis model:
//!
//! - **tier**: *what* the fact is about — `User` (follows the person everywhere),
//!   `Device` (true only on this machine), `Place` (applies at a directory tree).
//! - **anchor**: *where* the fact applies — a normalized absolute path (Windows-safe).
//!   Matching is by prefix (segment-safe nearest-ancestor).
//!
//! ## Tier assignment rules (``decide`)
//!
//! See [`tiering::decide`] — this module provides the data types only.

use crate::core::config;
use crate::memory::store::MemoryEntry;
use once_cell::sync::Lazy;
use std::path::Path;
use std::sync::Mutex;

// ── Tier ─────────────────────────────────────────────────────────────────

/// What axis a memory fact lives on.
///
/// `Hash` so it can key a partition map — the LRU caps bucket by `(tier, anchor|device)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tier {
    /// About the user — applies everywhere (preferences, habits, identity).
    /// Maps to the old "global" scope. Eligible for the frozen core.
    User,
    /// About the machine — applies only on this device.
    /// Keyed by device id (from `crate::core::device::id()`).
    Device,
    /// About a place (directory tree) — applies under a specific anchor path.
    /// The anchor is a normalized absolute path with prefix matching.
    Place,
}

impl Tier {
    /// Machine-readable serialized form (lowercase).
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::User => "user",
            Tier::Device => "device",
            Tier::Place => "place",
        }
    }

    /// Lenient parse: unknown → `Place` (the default for legacy files).
    ///
    /// Entries reach memory through serde, which has its own tier deserializer; this is the
    /// hand-parse path for a tier read from anywhere else.
    #[allow(dead_code)]
    pub fn parse(s: &str) -> Tier {
        match s.trim().to_lowercase().as_str() {
            "user" => Tier::User,
            "device" => Tier::Device,
            _ => Tier::Place,
        }
    }

    /// Strict parse: returns `None` on unknown values.
    pub fn parse_strict(s: &str) -> Option<Tier> {
        match s.trim().to_lowercase().as_str() {
            "user" => Some(Tier::User),
            "device" => Some(Tier::Device),
            "place" => Some(Tier::Place),
            _ => None,
        }
    }
}

// ── Lineage ──────────────────────────────────────────────────────────────

/// The current device's place lineage: where we are, where we've been, where home is.
///
/// This is the context needed to decide where a new fact should anchor and whether
/// an existing fact's anchor applies to the current working directory.
#[derive(Debug, Clone)]
pub struct Lineage {
    /// The current working directory, normalized.
    pub cwd: String,
    /// Ancestor places that may be relevant (project root, git roots, home).
    /// Ordered from narrowest (closest to cwd) to broadest.
    ///
    /// Scope decisions read `cwd` and compare against each entry's own anchor, so the precomputed
    /// ancestor chain is never consulted. Tests assert on it — it is the visible record of what the
    /// lineage walk found.
    #[allow(dead_code)]
    pub places: Vec<String>,
    /// The stable device id (from `crate::core::device::id()`).
    pub device: String,
    /// The user's home directory, if resolvable.
    pub home: Option<String>,
}

impl Lineage {
    /// The lineage for the current process state, computed once per (cwd, `AIZEN_PROJECT_ROOT`).
    ///
    /// Keyed like [`crate::core::config::project_slug`]'s cache, and for the same reason: an
    /// unkeyed one-entry cache would hand a test (or a `cd`) the lineage of whatever ran first,
    /// which is exactly the stale-slug flake class this codebase has already been bitten by.
    pub fn current() -> Self {
        static CACHE: Lazy<Mutex<Option<(String, Lineage)>>> = Lazy::new(|| Mutex::new(None));
        let cache_key = format!(
            "{}|{}",
            std::env::var("AIZEN_PROJECT_ROOT").unwrap_or_default(),
            std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default()
        );

        if let Ok(guard) = CACHE.lock() {
            if let Some((ref k, ref cached)) = *guard {
                if *k == cache_key {
                    return cached.clone();
                }
            }
        }

        let cwd = config::current_anchor();
        let home = std::env::var("USERPROFILE")
            .ok()
            .or_else(|| std::env::var("HOME").ok())
            .map(|s| {
                let p = Path::new(&s);
                config::anchor_of(p)
            })
            .filter(|s| !s.is_empty());

        // Build place list: project root (nearest), then cwd parents up to home
        let project_root = config::project_root();
        let project_norm = config::anchor_of(&project_root);

        let mut places = Vec::new();
        // Narrowest first: cwd itself
        places.push(cwd.clone());
        // Project root (if different from cwd and an ancestor)
        if project_norm != cwd && is_descendant(&cwd, &project_norm) {
            places.push(project_norm);
        }
        // Parents of cwd up to (but not including) home
        if let Some(ref home_str) = home {
            let mut cur = Path::new(&cwd)
                .parent()
                .and_then(|p| p.to_str().map(String::from));
            while let Some(ref dir) = cur {
                if dir == home_str || home_str.starts_with(dir.as_str()) {
                    break;
                }
                places.push(dir.clone());
                cur = Path::new(dir)
                    .parent()
                    .and_then(|p| p.to_str().map(String::from));
            }
        }

        let lineage = Lineage {
            cwd,
            places,
            device: crate::core::device::id().to_string(),
            home,
        };

        if let Ok(mut guard) = CACHE.lock() {
            *guard = Some((cache_key, lineage.clone()));
        }

        lineage
    }

    /// How specific an entry is for the current lineage.
    /// Returns `Some(u32)` where higher = more specific (narrower anchor), or `None`
    /// if the entry does not apply at all in the current context.
    pub fn specificity(&self, entry: &MemoryEntry) -> Option<u32> {
        let tier = entry.tier;

        match tier {
            Tier::User => {
                // User facts apply everywhere — least specific
                Some(0)
            }
            Tier::Device => {
                // Device facts apply if the device id matches
                let entry_dev = entry.device.as_deref().unwrap_or("");
                let also_read = crate::core::device::also_read();
                if entry_dev == self.device
                    || also_read.contains(&entry.device.as_deref().unwrap_or("").to_string())
                {
                    Some(10)
                } else {
                    None // not this device
                }
            }
            Tier::Place => {
                // Place facts apply if the anchor is an ancestor of (or equal to) cwd
                match entry.anchor.as_deref() {
                    // Segment-safe: a raw `starts_with` would let anchor `c:/a/b` claim
                    // `c:/a/bc`, silently applying one project's facts inside its sibling.
                    Some(anchor) if is_ancestor(anchor, &self.cwd) => {
                        // Specificity = anchor depth, so the NEAREST ancestor wins a tie.
                        Some(depth(anchor) as u32)
                    }
                    Some(_) => None, // anchored somewhere else entirely
                    None => {
                        // Place entry with no anchor = orphan
                        None
                    }
                }
            }
        }
    }

    /// The narrowest anchor that is either the project root or the cwd itself
    /// (whichever is narrower / closer to the actual working files).
    pub fn narrowest_project_or_cwd(&self) -> String {
        // The project root is always a plausible anchor; cwd is narrower.
        // Prefer the project root if it's an ancestor of cwd (it's more stable).
        let project_norm = config::anchor_of(&config::project_root());
        if project_norm.len() < self.cwd.len() && self.cwd.starts_with(&project_norm) {
            project_norm
        } else {
            self.cwd.clone()
        }
    }
}

// ── Helper ───────────────────────────────────────────────────────────────

/// Check if `child` is a descendant of (or equal to) `parent`, comparing
/// normalized path segments on Windows safely (case-insensitive ASCII).
pub fn is_descendant(child: &str, parent: &str) -> bool {
    if child == parent {
        return true;
    }
    // Both must use forward slashes and be lowercased for comparison
    let child = child.trim_end_matches('/');
    let parent = parent.trim_end_matches('/');
    if parent.is_empty() {
        return true; // root is ancestor of everything
    }
    child.starts_with(parent) && {
        let remainder = &child[parent.len()..];
        remainder.starts_with('/') || remainder.is_empty()
    }
}

/// Check if `anchor` is an ancestor of (or equal to) `cwd`, segment-safe.
/// Both strings must be in normalized form (forward slashes, no trailing slash).
pub fn is_ancestor(anchor: &str, cwd: &str) -> bool {
    is_descendant(cwd, anchor)
}

/// The depth of a normalized path (number of non-empty segments).
pub fn depth(path: &str) -> usize {
    path.split('/').filter(|s| !s.is_empty()).count()
}

/// The deepest folder that contains BOTH `a` and `b`, or `None` when they share nothing (different
/// drives, or one is empty).
///
/// Used when reconciliation picks a winner between two place facts anchored at different depths: the
/// survivor must be re-anchored here, not left at either original. Keeping the winner's own anchor
/// would silently change the fact's SCOPE as a side effect of resolving its CONTENT — a fact that
/// was true for the whole repo would come back true only under `src/agent`, and nothing in the
/// output would say so.
///
/// Only [`super::learning::reconcile::resolve_chain`] needs this, and that rule is not wired into the
/// reconcile pass yet — so this goes dead with it, and comes back with it.
#[allow(dead_code)]
pub fn common_ancestor(a: &str, b: &str) -> Option<String> {
    let a = a.trim_end_matches('/');
    let b = b.trim_end_matches('/');
    if a.is_empty() || b.is_empty() {
        return None;
    }
    let mut shared: Vec<&str> = Vec::new();
    for (sa, sb) in a.split('/').zip(b.split('/')) {
        // Windows paths reach here already lowercased by `normalize_path_key`, but a hand-authored
        // anchor may not be — compare case-insensitively so `C:/Work` and `c:/work/x` still match.
        if sa.eq_ignore_ascii_case(sb) {
            shared.push(sa);
        } else {
            break;
        }
    }
    // Refuse to widen all the way to a filesystem root. `c:` (or `/`) technically IS a common
    // ancestor of two unrelated trees, but re-anchoring a project fact there makes it fire in every
    // directory on the disk — louder and more wrong than simply failing to reconcile the pair.
    let named: Vec<&&str> = shared.iter().filter(|s| !s.is_empty()).collect();
    match named.as_slice() {
        [] => return None,                            // nothing shared, or only `/`
        [only] if only.ends_with(':') => return None, // only the drive letter
        _ => {}
    }
    Some(shared.join("/"))
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_parse_roundtrip() {
        for t in &[Tier::User, Tier::Device, Tier::Place] {
            assert_eq!(Tier::parse(t.as_str()), *t);
            assert_eq!(Tier::parse_strict(t.as_str()), Some(*t));
        }
    }

    #[test]
    fn tier_parse_lenient_defaults_to_place() {
        assert_eq!(Tier::parse("global"), Tier::Place);
        assert_eq!(Tier::parse("project"), Tier::Place);
        assert_eq!(Tier::parse("nonsense"), Tier::Place);
        assert_eq!(Tier::parse(""), Tier::Place);
    }

    #[test]
    fn tier_parse_strict_rejects_unknown() {
        assert_eq!(Tier::parse_strict("global"), None);
        assert_eq!(Tier::parse_strict(""), None);
    }

    #[test]
    fn is_ancestor_windows() {
        // Simulate normalized Windows paths
        assert!(is_ancestor("c:/users/admin", "c:/users/admin/desktop"));
        assert!(is_ancestor("c:/users/admin", "c:/users/admin"));
        assert!(!is_ancestor("c:/users/admin", "c:/users/other"));
        // Different roots always false
        assert!(!is_ancestor("d:/stuff", "c:/stuff"));
        // Partial prefix match must not hit
        assert!(!is_ancestor("c:/users/admini", "c:/users/admin/desktop"));
    }

    #[test]
    fn depth_counts_segments() {
        assert_eq!(depth("c:/"), 1);
        assert_eq!(depth("c:/users/admin"), 3);
        assert_eq!(depth("c:/users/admin/desktop"), 4);
    }

    #[test]
    fn common_ancestor_widens_to_the_shared_prefix() {
        // The reconciliation rule: when two facts on the same chain disagree, the survivor is
        // re-anchored to where BOTH claims were true, never to the narrower of the two.
        assert_eq!(
            common_ancestor("c:/work/proj/src", "c:/work/proj/tests").as_deref(),
            Some("c:/work/proj")
        );
        // One contains the other → the container wins (widening, not narrowing).
        assert_eq!(
            common_ancestor("c:/work/proj", "c:/work/proj/src/agent").as_deref(),
            Some("c:/work/proj")
        );
        // Identical anchors are their own ancestor.
        assert_eq!(
            common_ancestor("c:/work/proj", "c:/work/proj").as_deref(),
            Some("c:/work/proj")
        );
        // Segment comparison, not string prefix: `proj` must not be treated as matching
        // `projector`, so the shared part stops at `work` — which IS a real shared parent.
        assert_eq!(
            common_ancestor("c:/work/proj", "c:/work/projector").as_deref(),
            Some("c:/work")
        );
    }

    #[test]
    fn common_ancestor_refuses_to_widen_past_usefulness() {
        // Two unrelated trees share only the drive. Anchoring there would make a project-specific
        // fact fire in every directory on the disk — worse than losing the anchor, so: None.
        assert_eq!(common_ancestor("c:/work/a", "c:/other/b"), None);
        // Different drives share nothing at all.
        assert_eq!(common_ancestor("c:/work/a", "d:/work/a"), None);
        // An empty anchor is not a wildcard.
        assert_eq!(common_ancestor("", "c:/work/a"), None);
    }

    #[test]
    fn is_descendant_edge_cases() {
        assert!(is_descendant("c:/a/b", "c:/a"));
        assert!(is_descendant("c:/a", "c:/a"));
        assert!(!is_descendant("c:/a", "c:/a/b")); // parent not descendant of child
        assert!(!is_descendant("c:/ab", "c:/a")); // "ab" should not match "a" prefix
    }

    #[test]
    fn narrowest_project_or_cwd_is_reasonable() {
        let lineage = Lineage::current();
        let anchor = lineage.narrowest_project_or_cwd();
        assert!(!anchor.is_empty(), "anchor should not be empty");
        // Must be a normalized path (forward slashes)
        assert!(
            !anchor.contains('\\'),
            "anchor should use forward slashes: {anchor}"
        );
    }
}
