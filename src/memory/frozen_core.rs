//! The frozen core: the small always-on block injected into the system PREFIX once
//! per session, **immutable mid-session** so the prompt prefix stays byte-stable and
//! the upstream prefix-cache stays warm (the cheap-turns lever).
//!
//! Two-phase deferred rebuild: a session serves `core.active`; any change is computed
//! into `core.next` and only promoted to `core.active` at the NEXT session start. So a
//! memory learned/edited this session never mutates the live prefix — it lands next run.
//!
//! ## Policy (token-lean)
//! Always-on packs **STYLE.md + global user prefs only**. Project-zone facts live in the
//! long-tail and are retrieved via `memory_search` (scoped to current workspace). Core
//! files are **per-repo** (`core/active/<slug>.md`) so repo A's prefix never leaks into B.

use crate::core::config;
use crate::memory::bloat::decay;
use crate::memory::render::{est_tokens, render_block};
use crate::memory::store::{MemoryEntry, MemoryType};
use anyhow::{Context, Result};
use std::cmp::Ordering;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq)]
pub struct FrozenCore {
    pub rendered: String,
    pub token_count: usize,
    pub source_ids: Vec<String>,
    pub spilled_ids: Vec<String>,
}

fn active_path_for(slug: &str) -> PathBuf {
    config::core_active_path(slug)
}
fn next_path_for(slug: &str) -> PathBuf {
    config::core_next_path(slug)
}
fn active_path() -> PathBuf {
    active_path_for(&config::project_slug())
}
fn next_path() -> PathBuf {
    next_path_for(&config::project_slug())
}

/// Core-eligible = the STYLE.md profile (if present) + **global** `type=user` entries that
/// pass: not `core_denied`, `scope.is_none()`, and **core-trusted** (curated provenance, or an
/// inferred fact re-observed across ≥2 sessions). Project-zone facts never spend the always-on
/// prefix budget — they are long-tail only (`memory_search`).
///
/// Packing order: STYLE.md pinned first, then global facts salience-greedy (reuse-earned rank,
/// [`decay::salience_of`]). Overflow spills to the retrieved tier.
pub fn build(entries: &[MemoryEntry], style_body: Option<&str>, max_tokens: usize) -> FrozenCore {
    // Always-on is global-only; slug/subpath are unused for packing but kept on the pure API
    // for tests that assert project exclusion.
    build_scoped(
        entries,
        style_body,
        max_tokens,
        &crate::memory::ScopeSel::Global,
        &config::project_slug(),
        config::current_subpath().as_deref(),
    )
}

/// Curated facts are always core-trusted; an inferred (regex-extracted) fact must recur across
/// sessions before it may occupy the always-on prefix.
fn core_trusted(e: &MemoryEntry) -> bool {
    e.source != crate::memory::provenance::ProvenanceKind::Inferred || e.sessions >= 2
}

/// Does the user's current region (`current`) fall under an entry's `subpath` tag (segment-safe)?
/// Shared with the search-path boost (`search_filtered_scoped`).
pub(crate) fn subpath_matches(entry_subpath: &str, current: &str) -> bool {
    current == entry_subpath
        || current.strip_prefix(entry_subpath).is_some_and(|r| r.starts_with('/'))
}

/// [`build`] with the workspace view made explicit (pure — fully deterministic in tests).
///
/// Always-on policy: only **global** (`scope.is_none()`) core-trusted user facts + STYLE.
/// `sel` / `current_slug` / `current_subpath` remain for API compatibility and search-exclusion
/// callers; project-scoped entries are hard-excluded from the always-on pack regardless of `sel`.
pub fn build_scoped(
    entries: &[MemoryEntry],
    style_body: Option<&str>,
    max_tokens: usize,
    _sel: &crate::memory::ScopeSel,
    _current_slug: &str,
    _current_subpath: Option<&str>,
) -> FrozenCore {
    let today = decay::today();
    let half_life = config::MemorySettings::default().recency_half_life_days;

    let mut rest: Vec<MemoryEntry> = entries
        .iter()
        .filter(|e| {
            e.mtype == MemoryType::User
                && e.is_active()
                && !e.core_denied
                && e.scope.is_none() // always-on = global only
                && core_trusted(e)
        })
        .cloned()
        .collect();
    // Salience-greedy, newest as tiebreak.
    rest.sort_by(|a, b| {
        decay::salience_of(b, &today, half_life)
            .partial_cmp(&decay::salience_of(a, &today, half_life))
            .unwrap_or(Ordering::Equal)
            .then(b.mtime_ms.cmp(&a.mtime_ms))
    });

    let mut eligible: Vec<MemoryEntry> = Vec::with_capacity(rest.len() + 1);
    if let Some(body) = style_body {
        if !body.trim().is_empty() {
            eligible.push(MemoryEntry {
                id: "style".into(),
                path: config::style_path(),
                name: "user-style".into(),
                description: "the user's distilled style/preferences".into(),
                mtype: MemoryType::User,
                created: None,
                body: body.trim().to_string(),
                mtime_ms: u128::MAX, // pin newest so STYLE.md always leads the core
                tokens: vec![],
                ..Default::default()
            });
        }
    }
    eligible.extend(rest);

    let (rendered, source_ids, spilled_ids) = render_block("cli-core", &eligible, max_tokens);
    FrozenCore {
        token_count: est_tokens(&rendered),
        rendered,
        source_ids,
        spilled_ids,
    }
}

/// Promote a pending rebuild (start-of-session) for the **current** project slug.
/// Also consumes a legacy single-file `core.next.md` once (pre per-repo layout).
pub fn promote_pending() -> Result<()> {
    let slug = config::project_slug();
    let next = next_path_for(&slug);
    let active = active_path_for(&slug);
    if next.exists() {
        if let Some(parent) = active.parent() {
            fs::create_dir_all(parent).ok();
        }
        fs::rename(&next, &active)
            .with_context(|| format!("promoting {} -> {}", next.display(), active.display()))?;
        return Ok(());
    }
    // One-shot legacy fallback: old single-file next → current slug active (only if slug active missing).
    let legacy_next = config::legacy_core_next_path();
    if legacy_next.exists() && !active.exists() {
        if let Some(parent) = active.parent() {
            fs::create_dir_all(parent).ok();
        }
        let _ = fs::rename(&legacy_next, &active);
    }
    Ok(())
}

/// Read the currently-active core for this project WITHOUT promoting a pending rebuild.
/// Falls back to legacy `core.active.md` when the per-slug file is absent (one-shot read).
pub fn read_active() -> String {
    let path = active_path();
    if let Ok(s) = fs::read_to_string(&path) {
        return s;
    }
    fs::read_to_string(config::legacy_core_active_path()).unwrap_or_default()
}

/// Session-START refresh: promote any pending, then build fresh from the current store and ADOPT it
/// as the active core for **this** project slug. A new session has no warm prefix to protect, so it
/// takes the freshest core — this is what makes a global `type=user` fact added between sessions
/// actually reach the prompt. (Deferral via `stage_next` is for MID-session edits only.)
pub fn refresh_active(entries: &[MemoryEntry], style_body: Option<&str>, max_tokens: usize) -> String {
    let _ = promote_pending();
    let fresh = build(entries, style_body, max_tokens);
    let active = active_path();
    if let Some(parent) = active.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(&active, &fresh.rendered); // adopt now (start of session)
    let _ = fs::remove_file(next_path()); // clear any stale pending so it can't double-apply
    fresh.rendered
}

/// Stage a freshly-built core for NEXT session (current slug) iff it differs from the active one.
/// Returns true if a rebuild was staged.
pub fn stage_next(fresh: &FrozenCore) -> Result<bool> {
    let active_p = active_path();
    let next_p = next_path();
    let active = fs::read_to_string(&active_p)
        .or_else(|_| fs::read_to_string(config::legacy_core_active_path()))
        .unwrap_or_default();
    if active == fresh.rendered {
        // already current — clear any stale pending
        let _ = fs::remove_file(&next_p);
        return Ok(false);
    }
    if let Some(parent) = next_p.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    if let Some(parent) = active_p.parent() {
        fs::create_dir_all(parent).ok();
    }
    if active.is_empty() {
        // first ever build — adopt immediately (no warm prefix to protect yet)
        fs::write(&active_p, &fresh.rendered)?;
        let _ = fs::remove_file(&next_p);
        Ok(false)
    } else {
        fs::write(&next_p, &fresh.rendered)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn user_entry(id: &str, body: &str, mtime: u128) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            path: PathBuf::from(format!("{id}.md")),
            name: id.into(),
            description: String::new(),
            mtype: MemoryType::User,
            created: None,
            body: body.into(),
            mtime_ms: mtime,
            tokens: vec![],
            ..Default::default()
        }
    }
    fn ref_entry(id: &str) -> MemoryEntry {
        let mut e = user_entry(id, "reference body", 1);
        e.mtype = MemoryType::Reference;
        e
    }

    #[test]
    fn build_includes_only_user_and_style_within_cap() {
        let entries = vec![
            user_entry("u1", "prefers pnpm", 10),
            ref_entry("r1"), // excluded (not user/style)
        ];
        let fc = build(&entries, Some("communicates in Vietnamese, terse"), 800);
        assert!(fc.source_ids.contains(&"style".to_string()));
        assert!(fc.source_ids.contains(&"u1".to_string()));
        assert!(!fc.source_ids.contains(&"r1".to_string()));
        assert!(fc.token_count <= 800);
    }

    #[test]
    fn overflow_spills_not_drops() {
        let big = "word ".repeat(4000);
        let entries = vec![user_entry("u1", &big, 10), user_entry("u2", &big, 5)];
        let fc = build(&entries, None, 200);
        // at most one fits; the other spills (never silently dropped)
        assert_eq!(fc.source_ids.len() + fc.spilled_ids.len(), 2);
        assert!(!fc.spilled_ids.is_empty());
        assert!(fc.token_count <= 200);
    }

    #[test]
    fn style_leads_the_core() {
        let fc = build(&[user_entry("u1", "x", 999)], Some("style line"), 800);
        assert_eq!(fc.source_ids.first().map(String::as_str), Some("style"));
    }

    #[test]
    fn salience_greedy_packs_a_reused_fact_before_a_newer_unused_one() {
        // `recent` is newer by mtime, but `reused` has earned salience through reinforcement +
        // recent retrieval → it must lead the core under salience-greedy packing (P8).
        let today = decay::today();
        let mut reused = user_entry("reused", "the fact the user keeps coming back to", 1);
        reused.reinforced = 8;
        reused.last_retrieved = Some(today);
        let recent = user_entry("recent", "a freshly added but never-reused fact", 999);

        // tight cap: only one body fits, so ordering decides who makes the core.
        let fc = build(&[recent, reused], None, 40);
        assert_eq!(
            fc.source_ids.first().map(String::as_str),
            Some("reused"),
            "salience must beat raw mtime: {:?}",
            fc.source_ids
        );
    }

    #[test]
    fn always_on_is_global_only_project_facts_excluded() {
        use crate::memory::ScopeSel;
        let mut here = user_entry("here", "fact about this project", 10);
        here.scope = Some("proja-00000001".into());
        let mut foreign = user_entry("foreign", "fact about another project", 20);
        foreign.scope = Some("projb-00000002".into());
        let global = user_entry("global", "the user prefers pnpm", 5);
        // Even with ScopeSel::Current, always-on packs ONLY global.
        let fc = build_scoped(
            &[here, foreign, global],
            None,
            800,
            &ScopeSel::Current,
            "proja-00000001",
            None,
        );
        assert!(fc.source_ids.contains(&"global".to_string()), "global packs");
        assert!(
            !fc.source_ids.contains(&"here".to_string()) && !fc.spilled_ids.contains(&"here".to_string()),
            "current-zone project fact must NOT spend always-on budget"
        );
        assert!(
            !fc.source_ids.contains(&"foreign".to_string())
                && !fc.spilled_ids.contains(&"foreign".to_string()),
            "foreign zone never spends this prefix budget"
        );
    }

    #[test]
    fn single_session_inferred_is_barred_from_core_until_it_recurs() {
        use crate::memory::provenance::ProvenanceKind;
        use crate::memory::ScopeSel;
        let mut one_shot = user_entry("one-shot", "a regex guess seen once", 10);
        one_shot.source = ProvenanceKind::Inferred;
        one_shot.sessions = 1;
        let mut recurring = user_entry("recurring", "re-observed across sessions", 5);
        recurring.source = ProvenanceKind::Inferred;
        recurring.sessions = 2;
        let curated = user_entry("curated", "hand-authored", 1); // Manual default
        let fc = build_scoped(&[one_shot, recurring, curated], None, 800, &ScopeSel::Current, "x", None);
        assert!(!fc.source_ids.contains(&"one-shot".to_string()), "single-session inference stays search-only");
        assert!(fc.source_ids.contains(&"recurring".to_string()), "recurrence earns core residency");
        assert!(fc.source_ids.contains(&"curated".to_string()));
    }

    #[test]
    fn subpath_matches_is_segment_safe() {
        assert!(subpath_matches("src/agent", "src/agent"));
        assert!(subpath_matches("src/agent", "src/agent/lsp"));
        assert!(!subpath_matches("src/a", "src/agent"));
    }

    #[test]
    fn refresh_active_adopts_fresh_immediately() {
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-fc-refresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);
        std::fs::create_dir_all(config::cli_memory_dir()).unwrap();

        assert!(read_active().is_empty(), "starts empty");
        let rendered = refresh_active(&[user_entry("u1", "prefers pnpm", 10)], None, 800);
        assert!(rendered.contains("prefers pnpm"));
        // the key fix: a session-start refresh ADOPTS the fresh core (no manual rebuild needed)
        assert!(read_active().contains("prefers pnpm"), "active adopted immediately");
        // written under per-slug path
        assert!(active_path().exists(), "per-slug active file created");

        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_slug_paths_are_isolated() {
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-fc-iso-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);
        std::fs::create_dir_all(config::cli_memory_dir()).unwrap();

        let a = active_path_for("repo-aaa");
        let b = active_path_for("repo-bbb");
        assert_ne!(a, b);
        if let Some(p) = a.parent() {
            fs::create_dir_all(p).unwrap();
        }
        if let Some(p) = b.parent() {
            fs::create_dir_all(p).unwrap();
        }
        fs::write(&a, "core-A").unwrap();
        fs::write(&b, "core-B").unwrap();
        assert_eq!(fs::read_to_string(&a).unwrap(), "core-A");
        assert_eq!(fs::read_to_string(&b).unwrap(), "core-B");

        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
