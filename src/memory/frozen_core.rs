//! The frozen core: the small always-on block injected into the system PREFIX once
//! per session, **immutable mid-session** so the prompt prefix stays byte-stable and
//! the upstream prefix-cache stays warm (the cheap-turns lever).
//!
//! Two-phase deferred rebuild: a session serves `core.active`; any change is computed
//! into `core.next` and only promoted to `core.active` at the NEXT session start. So a
//! memory learned/edited this session never mutates the live prefix — it lands next run.

use crate::config;
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

fn active_path() -> PathBuf {
    config::cli_memory_dir().join("core.active.md")
}
fn next_path() -> PathBuf {
    config::cli_memory_dir().join("core.next.md")
}

/// Core-eligible = the STYLE.md profile (if present) + `type=user` entries. Packing order is
/// **salience-greedy (P8)**: STYLE.md is pinned first, then the rest by salience (reuse-earned
/// rank, [`decay::salience_of`]) so the always-on prefix holds the facts that actually get used,
/// newest-as-tiebreak. Build the rendered block capped at `max_tokens`; overflow spills to the
/// retrieved tier. (Recomputed at session start into `core.next` — the live prefix stays
/// byte-stable mid-session; a re-pack only takes effect next run.)
pub fn build(entries: &[MemoryEntry], style_body: Option<&str>, max_tokens: usize) -> FrozenCore {
    let today = decay::today();
    let half_life = config::MemorySettings::default().recency_half_life_days;

    // The rest (non-style user facts), salience-greedy then newest-first.
    let mut rest: Vec<MemoryEntry> = entries
        .iter()
        .filter(|e| e.mtype == MemoryType::User && e.is_active())
        .cloned()
        .collect();
    rest.sort_by(|a, b| {
        let sa = decay::salience_of(a, &today, half_life);
        let sb = decay::salience_of(b, &today, half_life);
        sb.partial_cmp(&sa)
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

/// Promote a pending rebuild (start-of-session): if `core.next` exists, make it active.
pub fn promote_pending() -> Result<()> {
    let next = next_path();
    if next.exists() {
        let active = active_path();
        fs::create_dir_all(active.parent().unwrap()).ok();
        fs::rename(&next, &active)
            .with_context(|| format!("promoting {} -> {}", next.display(), active.display()))?;
    }
    Ok(())
}

/// Read the currently-active core WITHOUT promoting a pending rebuild.
pub fn read_active() -> String {
    fs::read_to_string(active_path()).unwrap_or_default()
}

/// Session-START refresh: promote any pending, then build fresh from the current store and ADOPT it
/// as the active core. A new session has no warm prefix to protect, so it takes the freshest core —
/// this is what makes a `type=user` fact added between sessions actually reach the prompt. (Deferral
/// via `stage_next` is for MID-session edits only.) Returns the rendered block.
pub fn refresh_active(entries: &[MemoryEntry], style_body: Option<&str>, max_tokens: usize) -> String {
    let _ = promote_pending();
    let fresh = build(entries, style_body, max_tokens);
    if let Some(parent) = active_path().parent() {
        let _ = fs::create_dir_all(parent);
    }
    let _ = fs::write(active_path(), &fresh.rendered); // adopt now (start of session)
    let _ = fs::remove_file(next_path()); // clear any stale pending so it can't double-apply
    fresh.rendered
}

/// Stage a freshly-built core for NEXT session iff it differs from the active one.
/// Returns true if a rebuild was staged.
pub fn stage_next(fresh: &FrozenCore) -> Result<bool> {
    let active = fs::read_to_string(active_path()).unwrap_or_default();
    if active == fresh.rendered {
        // already current — clear any stale pending
        let _ = fs::remove_file(next_path());
        return Ok(false);
    }
    let dir = config::cli_memory_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    if active.is_empty() {
        // first ever build — adopt immediately (no warm prefix to protect yet)
        fs::write(active_path(), &fresh.rendered)?;
        let _ = fs::remove_file(next_path());
        Ok(false)
    } else {
        fs::write(next_path(), &fresh.rendered)?;
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
        let fc = build(&entries, Some("communicates in Vietnamese, terse"), 1500);
        assert!(fc.source_ids.contains(&"style".to_string()));
        assert!(fc.source_ids.contains(&"u1".to_string()));
        assert!(!fc.source_ids.contains(&"r1".to_string()));
        assert!(fc.token_count <= 1500);
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
        let fc = build(&[user_entry("u1", "x", 999)], Some("style line"), 1500);
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
    fn refresh_active_adopts_fresh_immediately() {
        let _g = crate::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-fc-refresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);
        std::fs::create_dir_all(config::cli_memory_dir()).unwrap();

        assert!(read_active().is_empty(), "starts empty");
        let rendered = refresh_active(&[user_entry("u1", "prefers pnpm", 10)], None, 1500);
        assert!(rendered.contains("prefers pnpm"));
        // the key fix: a session-start refresh ADOPTS the fresh core (no manual rebuild needed)
        assert!(read_active().contains("prefers pnpm"), "active adopted immediately");

        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
