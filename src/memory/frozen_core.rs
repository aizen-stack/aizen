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
use crate::memory::path_scope::Tier;
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

fn active_path_for(key: &str) -> PathBuf {
    config::core_active_path(key)
}
fn next_path_for(key: &str) -> PathBuf {
    config::core_next_path(key)
}

/// The core file key: the **device**, not the project.
///
/// Was `project_slug()`. Since the always-on block now holds only `user`- and `device`-tier facts,
/// its content is identical in every directory on one machine — so a per-project file gave every
/// `cd` a different prefix and threw away the upstream prefix cache for no gain. One file per
/// machine also means a `cd` mid-session cannot change the served core at all.
fn core_key() -> &'static str {
    crate::core::device::id()
}
fn active_path() -> PathBuf {
    active_path_for(core_key())
}
fn next_path() -> PathBuf {
    next_path_for(core_key())
}

/// Core-eligible = the STYLE.md profile (if present) + entries that pass: `tier` is `User` (or
/// `Device` matching THIS machine), active, not `core_denied`, and **core-trusted** (curated
/// provenance, or an inferred fact re-observed across ≥2 sessions). `Place` facts never spend the
/// always-on prefix budget — they are long-tail only (`memory_search`).
///
/// Packing order: STYLE.md pinned first, then the rest salience-greedy (reuse-earned rank,
/// [`decay::salience_of`]). Overflow spills to the retrieved tier.
pub fn build(entries: &[MemoryEntry], style_body: Option<&str>, max_tokens: usize) -> FrozenCore {
    // The half-life comes from the LOADED settings, not `MemorySettings::default()`: the core's
    // packing order is salience-ranked, so reading the default here silently ignored a user who
    // tuned `recency_half_life_days` and ranked their core by a decay curve they never chose.
    let s = crate::memory::settings();
    build_scoped(
        entries,
        style_body,
        max_tokens,
        crate::core::device::id(),
        s.recency_half_life_days,
    )
}

/// Curated facts are always core-trusted; an inferred (regex-extracted) fact must recur across
/// sessions before it may occupy the always-on prefix.
fn core_trusted(e: &MemoryEntry) -> bool {
    e.source != crate::memory::provenance::ProvenanceKind::Inferred || e.sessions >= 2
}

/// May this entry occupy the always-on prefix on `this_device`?
///
/// The tier axis replaces the old `mtype == User && scope.is_none()` pair. That test keyed
/// eligibility off the fact's TYPE tag, which is why a `#remember` (written as `Feedback`) and a
/// `memory_save` (written as `Project`) could never reach the core no matter how global they were —
/// one of the four independent reasons the store stayed empty. Now placement decides placement.
fn core_eligible(e: &MemoryEntry, this_device: &str) -> bool {
    if !e.is_active() || e.core_denied || !core_trusted(e) {
        return false;
    }
    match e.tier {
        // About the person → true wherever they are, on whatever machine.
        Tier::User => true,
        // About the machine → always-on HERE, invisible on every other machine. `also_read` covers
        // an id that rotated (container rebuild, Windows reset) so those facts don't evaporate.
        Tier::Device => match e.device.as_deref() {
            Some(d) => d == this_device || crate::core::device::also_read().iter().any(|o| o == d),
            None => false, // device-tier with no id is unattributable — fail closed
        },
        // About a place → long tail only, retrieved by anchor match, never resident.
        Tier::Place => false,
    }
}

/// Does the user's current region (`current`) fall under an entry's `subpath` tag (segment-safe)?
/// Shared with the search-path boost (`search_filtered_scoped`).
///
/// Both callers now go through `path_scope`'s own matcher; kept here as the segment-safe reference
/// (a plain `starts_with` would match `src/foo` against `src/foobar`).
#[allow(dead_code)]
pub(crate) fn subpath_matches(entry_subpath: &str, current: &str) -> bool {
    current == entry_subpath
        || current
            .strip_prefix(entry_subpath)
            .is_some_and(|r| r.starts_with('/'))
}

/// [`build`] with the machine identity and decay curve injected (pure — fully deterministic in
/// tests, no cwd/env/registry reads).
///
/// Always-on policy: `tier == User`, plus `tier == Device` whose `device` tag is `this_device`.
/// A `Place` fact is hard-excluded no matter how salient — the whole point of the anchor axis is
/// that place-specific truth does not follow the user into unrelated directories.
///
/// The old `sel`/`current_slug`/`current_subpath` parameters are gone: all three were `_`-ignored,
/// so they advertised a workspace view this function never had.
pub fn build_scoped(
    entries: &[MemoryEntry],
    style_body: Option<&str>,
    max_tokens: usize,
    this_device: &str,
    half_life: f64,
) -> FrozenCore {
    let today = decay::today();

    let mut rest: Vec<MemoryEntry> = entries
        .iter()
        .filter(|e| core_eligible(e, this_device))
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
                // STYLE.md is the distilled user profile, so it is a `user`-tier row by definition.
                // Stated rather than defaulted: `MemoryEntry::default()` is a fail-closed orphan
                // Place, and this synthetic row is pushed past `core_eligible`, so a default here
                // would leave a Place-tagged entry sitting in the always-on block.
                tier: Tier::User,
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

/// Promote a pending rebuild (start-of-session) for **this device**.
/// Also consumes a legacy single-file `core.next.md` once (pre per-device layout).
pub fn promote_pending() -> Result<()> {
    let key = core_key();
    let next = next_path_for(key);
    let active = active_path_for(key);
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

/// Read the currently-active core for this DEVICE WITHOUT promoting a pending rebuild.
/// Falls back to legacy `core.active.md` when the per-device file is absent (one-shot read).
pub fn read_active() -> String {
    let path = active_path();
    if let Ok(s) = fs::read_to_string(&path) {
        return s;
    }
    fs::read_to_string(config::legacy_core_active_path()).unwrap_or_default()
}

/// Session-START refresh: promote any pending, then build fresh from the current store and ADOPT it
/// as the active core for **this device**. A new session has no warm prefix to protect, so it takes
/// the freshest core — this is what makes a `user`-tier fact added between sessions actually reach
/// the prompt. (Deferral via `stage_next` is for MID-session edits only.)
pub fn refresh_active(
    entries: &[MemoryEntry],
    style_body: Option<&str>,
    max_tokens: usize,
) -> String {
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

/// Stage a freshly-built core for NEXT session (this device) iff it differs from the active one.
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

    /// A core-eligible row: `tier: User` is stated, not defaulted. `MemoryEntry::default()` is a
    /// fail-closed orphan `Place`, so leaving it out would make every core test assert against an
    /// empty block and pass for the wrong reason.
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
            tier: Tier::User,
            ..Default::default()
        }
    }
    /// A row that must NOT reach the core. Under the old `scope` axis that was expressed as
    /// "any `mtype` other than `user`"; residency is now decided by TIER, so the non-core case is a
    /// `place` fact. (That widening is the point: `#remember` writes `Feedback` and `memory_save`
    /// writes `Project`, so a type-keyed gate meant neither could ever reach the always-on lane.)
    fn ref_entry(id: &str) -> MemoryEntry {
        let mut e = user_entry(id, "reference body", 1);
        e.mtype = MemoryType::Reference;
        e.tier = Tier::Place;
        e.anchor = Some("c:/work/somewhere".into());
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

    /// The device id every `build_scoped` test packs for.
    const THIS_DEV: &str = "dev-11111111";
    /// Default decay curve, so ordering assertions don't depend on user config.
    const HL: f64 = 30.0;

    fn place_entry(id: &str, anchor: &str, mtime: u128) -> MemoryEntry {
        let mut e = user_entry(id, "fact about a place", mtime);
        e.tier = Tier::Place;
        e.anchor = Some(anchor.into());
        e
    }

    fn device_entry(id: &str, device: &str, mtime: u128) -> MemoryEntry {
        let mut e = user_entry(id, "fact about a machine", mtime);
        e.tier = Tier::Device;
        e.device = Some(device.into());
        e
    }

    #[test]
    fn always_on_packs_user_and_this_device_but_never_a_place() {
        // Successor to `always_on_is_global_only_project_facts_excluded`. Same guarantee — a fact
        // tied to one directory must not spend the always-on prefix budget — now keyed on the tier
        // axis. Also pins the NEW rule: a `device` fact IS resident, but only on its own machine.
        let here = place_entry("here", "c:/work/proja", 10);
        let elsewhere = place_entry("elsewhere", "c:/work/projb", 20);
        let mine = device_entry("mine", THIS_DEV, 15);
        let theirs = device_entry("theirs", "dev-99999999", 25);
        let user = user_entry("user", "the user prefers pnpm", 5);

        let fc = build_scoped(
            &[here, elsewhere, mine, theirs, user],
            None,
            800,
            THIS_DEV,
            HL,
        );

        let packed = |id: &str| fc.source_ids.contains(&id.to_string());
        // "not packed AND not spilled" = never eligible at all, as opposed to eligible-but-overflowed.
        let absent = |id: &str| {
            !fc.source_ids.contains(&id.to_string()) && !fc.spilled_ids.contains(&id.to_string())
        };

        assert!(
            packed("user"),
            "a user-tier fact is always-on: {:?}",
            fc.source_ids
        );
        assert!(
            packed("mine"),
            "a device fact for THIS machine is always-on: {:?}",
            fc.source_ids
        );
        assert!(
            absent("theirs"),
            "another machine's device fact must never be resident"
        );
        assert!(
            absent("here"),
            "a place fact must not spend always-on budget, even here"
        );
        assert!(absent("elsewhere"), "…nor one anchored somewhere else");
    }

    #[test]
    fn device_tier_without_an_id_is_never_resident() {
        // Fail-closed: an unattributable device fact could otherwise leak onto every machine.
        let mut orphan = user_entry("orphan", "some machine detail", 10);
        orphan.tier = Tier::Device;
        orphan.device = None;
        let fc = build_scoped(&[orphan], None, 800, THIS_DEV, HL);
        assert!(
            fc.source_ids.is_empty() && fc.spilled_ids.is_empty(),
            "{fc:?}"
        );
    }

    #[test]
    fn single_session_inferred_is_barred_from_core_until_it_recurs() {
        use crate::memory::provenance::ProvenanceKind;
        let mut one_shot = user_entry("one-shot", "a regex guess seen once", 10);
        one_shot.source = ProvenanceKind::Inferred;
        one_shot.sessions = 1;
        let mut recurring = user_entry("recurring", "re-observed across sessions", 5);
        recurring.source = ProvenanceKind::Inferred;
        recurring.sessions = 2;
        let curated = user_entry("curated", "hand-authored", 1); // Manual default
        let fc = build_scoped(&[one_shot, recurring, curated], None, 800, THIS_DEV, HL);
        assert!(
            !fc.source_ids.contains(&"one-shot".to_string()),
            "single-session inference stays search-only"
        );
        assert!(
            fc.source_ids.contains(&"recurring".to_string()),
            "recurrence earns core residency"
        );
        assert!(fc.source_ids.contains(&"curated".to_string()));
    }

    #[test]
    fn subpath_matches_is_segment_safe() {
        assert!(subpath_matches("src/agent", "src/agent"));
        assert!(subpath_matches("src/agent", "src/agent/lsp"));
        assert!(!subpath_matches("src/a", "src/agent"));
    }

    #[test]
    fn same_session_read_never_promotes_pending_but_next_session_refresh_does() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-fc-lifecycle-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("AIZEN_HOME", &dir);
        std::fs::create_dir_all(config::cli_memory_dir()).unwrap();

        let first = refresh_active(&[user_entry("old", "old preference", 1)], None, 800);
        assert!(first.contains("old preference"));
        let pending = build(&[user_entry("new", "new preference", 2)], None, 800);
        assert!(
            stage_next(&pending).unwrap(),
            "a changed core must be staged for the next conversation"
        );
        let active_before = std::fs::read(active_path()).unwrap();
        let pending_before = std::fs::read(next_path()).unwrap();

        assert_eq!(
            read_active(),
            first,
            "ordinary refreshes keep serving the adopted core"
        );
        assert_eq!(
            std::fs::read(active_path()).unwrap(),
            active_before,
            "read_active must not rewrite active"
        );
        assert_eq!(
            std::fs::read(next_path()).unwrap(),
            pending_before,
            "read_active must not consume pending"
        );

        let adopted = refresh_active(&[user_entry("new", "new preference", 2)], None, 800);
        assert!(adopted.contains("new preference"));
        assert!(!adopted.contains("old preference"));
        assert!(
            !next_path().exists(),
            "session-boundary refresh consumes/clears pending"
        );

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn refresh_active_adopts_fresh_immediately() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-fc-refresh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("AIZEN_HOME", &dir);
        std::fs::create_dir_all(config::cli_memory_dir()).unwrap();

        assert!(read_active().is_empty(), "starts empty");
        let rendered = refresh_active(&[user_entry("u1", "prefers pnpm", 10)], None, 800);
        assert!(rendered.contains("prefers pnpm"));
        // the key fix: a session-start refresh ADOPTS the fresh core (no manual rebuild needed)
        assert!(
            read_active().contains("prefers pnpm"),
            "active adopted immediately"
        );
        // written under per-slug path
        assert!(active_path().exists(), "per-slug active file created");

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn per_slug_paths_are_isolated() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-fc-iso-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("AIZEN_HOME", &dir);
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

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
