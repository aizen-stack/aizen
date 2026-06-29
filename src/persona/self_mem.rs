//! Persona-scoped self-memory — the `<self>` layer (P1) + the substrate the reflection pass (P2)
//! reads and writes.
//!
//! Generative-Agents shape: a **memory stream** of `episode`s (cheap, free, transient records of
//! what the character lived through) periodically distilled into `insight`s (durable, higher-level
//! reflections). Episodes are pruned under an LRU cap — raw experience is ephemeral; the reflected
//! insight is what persists and shapes the character.
//!
//! Storage: `~/.nextgen/personas/<slug>.self/*.md`, one memory per file:
//! ```text
//! ---
//! kind: insight        # episode | insight
//! importance: 7        # 0..=10
//! created: 2026-06-21
//! updated: 2026-06-21
//! ---
//! body…
//! ```
//! Injection (`self_block`) ranks by `importance × recency`, insights weighted up (they are the
//! distilled wisdom), capped to a token budget — so the always-on block stays small and current.

use crate::memory::frontmatter;
use crate::memory::render::{est_tokens, sanitize_body};
use crate::memory::score::recency_factor;
use crate::persona::personas_dir;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Keep at most this many episodes per character (LRU by importance then age). Raw experience is
/// transient — the reflection pass lifts the load-bearing parts into durable insights.
pub const EPISODE_CAP: usize = 60;
/// Keep at most this many reflected insights (the durable character layer).
pub const INSIGHT_CAP: usize = 40;
/// Reflect once accumulated episode importance since the last insight crosses this.
pub const REFLECT_IMPORTANCE_THRESHOLD: u32 = 15;
/// …and only if at least this many fresh episodes have piled up (avoid reflecting on one big turn).
pub const REFLECT_MIN_EPISODES: usize = 3;
/// Recency half-life (days) for the self-memory rank. Longer than the user-fact half-life — a
/// character's formative experiences should fade slowly.
const SELF_HALF_LIFE_DAYS: f64 = 45.0;
/// Insights rank above episodes of equal importance/age (distilled > raw).
const INSIGHT_RANK_BONUS: f64 = 1.4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Episode,
    Insight,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Episode => "episode",
            Kind::Insight => "insight",
        }
    }
    pub fn parse(s: &str) -> Kind {
        if s.trim().eq_ignore_ascii_case("insight") {
            Kind::Insight
        } else {
            Kind::Episode
        }
    }
}

#[derive(Debug, Clone)]
pub struct SelfMemory {
    // `id`/`updated` are parsed-record fields kept for completeness/forward-compat; not read today.
    #[allow(dead_code)]
    pub id: String,
    pub path: PathBuf,
    pub kind: Kind,
    pub importance: u8, // 0..=10
    pub created: Option<String>,
    #[allow(dead_code)]
    pub updated: Option<String>,
    pub body: String,
    pub mtime_ms: u128,
}

/// `~/.aizen/personas/<slug>.self/` — a character's private experience store.
pub fn self_dir(persona_slug: &str) -> PathBuf {
    personas_dir().join(format!("{persona_slug}.self"))
}

const KEY_ORDER: &[&str] = &["kind", "importance", "created", "updated"];

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn from_file(path: &Path) -> Option<SelfMemory> {
    let raw = fs::read_to_string(path).ok()?;
    let fm = frontmatter::parse(&raw);
    let id = path.file_stem().and_then(|s| s.to_str())?.to_lowercase();
    let kind = Kind::parse(fm.get("kind").unwrap_or("episode"));
    let importance = fm
        .get("importance")
        .and_then(|s| s.trim().parse::<u8>().ok())
        .unwrap_or(3)
        .min(10);
    let created = fm.get("created").map(str::to_string).filter(|s| !s.trim().is_empty());
    let updated = fm.get("updated").map(str::to_string).filter(|s| !s.trim().is_empty());
    let mtime_ms = fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Some(SelfMemory { id, path: path.to_path_buf(), kind, importance, created, updated, body: fm.body, mtime_ms })
}

/// All self-memories for a character (missing dir → empty; never errors).
pub fn list(persona_slug: &str) -> Vec<SelfMemory> {
    let mut out = Vec::new();
    let rd = match fs::read_dir(self_dir(persona_slug)) {
        Ok(r) => r,
        Err(_) => return out,
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().and_then(|x| x.to_str()).map(|x| x.eq_ignore_ascii_case("md")) != Some(true) {
            continue;
        }
        if let Some(m) = from_file(&p) {
            out.push(m);
        }
    }
    out
}

fn render(kind: Kind, importance: u8, created: &str, updated: &str, body: &str) -> String {
    let mut fields = BTreeMap::new();
    fields.insert("kind".to_string(), kind.as_str().to_string());
    fields.insert("importance".to_string(), importance.min(10).to_string());
    fields.insert("created".to_string(), created.to_string());
    fields.insert("updated".to_string(), updated.to_string());
    frontmatter::serialize(&fields, body, KEY_ORDER)
}

fn unique_path(dir: &Path, prefix: &str, body: &str) -> PathBuf {
    // a short, stable-ish stem from the body's first words, uniquified on collision
    let words: String = body
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { ' ' })
        .collect();
    let slug: String = words.split_whitespace().take(5).collect::<Vec<_>>().join("-");
    let slug = if slug.is_empty() { "mem".to_string() } else { slug };
    let base = format!("{prefix}-{slug}");
    let first = dir.join(format!("{base}.md"));
    if !first.exists() {
        return first;
    }
    for n in 2..100_000 {
        let cand = dir.join(format!("{base}-{n}.md"));
        if !cand.exists() {
            return cand;
        }
    }
    first
}

fn write(persona_slug: &str, kind: Kind, importance: u8, body: &str) -> Result<String> {
    let body = body.trim();
    if body.is_empty() {
        anyhow::bail!("empty self-memory body");
    }
    let dir = self_dir(persona_slug);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let prefix = if kind == Kind::Insight { "in" } else { "ep" };
    let path = unique_path(&dir, prefix, body);
    let now = today();
    fs::write(&path, render(kind, importance, &now, &now, body))
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(path.file_stem().and_then(|s| s.to_str()).unwrap_or("mem").to_string())
}

/// Record a free episode (zero model cost). Dedups against the most-recent episode (an identical
/// restatement is skipped) and prunes to the LRU cap afterward. Returns the new id, or `Ok(None)`
/// when skipped as a duplicate.
pub fn record_episode(persona_slug: &str, body: &str, importance: u8) -> Result<Option<String>> {
    let body = body.trim();
    if body.is_empty() {
        return Ok(None);
    }
    let mut eps: Vec<SelfMemory> =
        list(persona_slug).into_iter().filter(|m| m.kind == Kind::Episode).collect();
    eps.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms));
    if let Some(latest) = eps.first() {
        if normalize(&latest.body) == normalize(body) {
            return Ok(None); // same as last episode — no churn
        }
    }
    let id = write(persona_slug, Kind::Episode, importance, body)?;
    prune(persona_slug);
    Ok(Some(id))
}

/// Persist a reflected insight (the durable character layer). Prunes insights to their cap.
pub fn save_insight(persona_slug: &str, body: &str, importance: u8) -> Result<String> {
    let id = write(persona_slug, Kind::Insight, importance.max(5), body)?;
    prune(persona_slug);
    Ok(id)
}

fn normalize(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

/// Lowercase contains-any correction-signal detector (drives episode importance). A turn where the
/// user pushes back / redirects is more formative than a routine one.
pub fn looks_like_correction(text: &str) -> bool {
    let t = text.to_lowercase();
    const SIGNALS: &[&str] = &[
        "no,", "no ", "not ", "actually", "instead", "wrong", "don't", "do not", "stop",
        "revert", "undo", "incorrect", // en
        "không", "sai", "thay vào", "đừng", "sửa lại", "thực ra", "không phải", // vi
    ];
    SIGNALS.iter().any(|s| t.contains(s))
}

/// Heuristic importance [0..=8] for a free episode. Base 3; pushed up by corrections (formative),
/// real tool work (the character DID something), and substantial exchanges.
pub fn episode_importance(user_text: &str, tool_calls: usize, corrected: bool) -> u8 {
    let mut s: i32 = 3;
    if corrected {
        s += 2;
    }
    if tool_calls >= 2 {
        s += 1;
    }
    if user_text.chars().count() > 240 {
        s += 1;
    }
    s.clamp(0, 8) as u8
}

fn age_days(created: &Option<String>, today: chrono::NaiveDate) -> f64 {
    match created.as_deref().and_then(|s| chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok()) {
        Some(d) => (today - d).num_days().max(0) as f64,
        None => 0.0,
    }
}

fn rank(m: &SelfMemory, today: chrono::NaiveDate) -> f64 {
    let base = (m.importance as f64 / 10.0) * recency_factor(age_days(&m.created, today), SELF_HALF_LIFE_DAYS);
    if m.kind == Kind::Insight {
        base * INSIGHT_RANK_BONUS
    } else {
        base
    }
}

/// The inner `<self>` block: top self-memories by `importance × recency` (insights weighted up),
/// capped at `max_tokens`. `None` when the character has no experience yet.
pub fn self_block(persona_slug: &str, max_tokens: usize) -> Option<String> {
    let mut mems = list(persona_slug);
    if mems.is_empty() {
        return None;
    }
    let today = chrono::Local::now().date_naive();
    mems.sort_by(|a, b| {
        rank(b, today)
            .partial_cmp(&rank(a, today))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.mtime_ms.cmp(&a.mtime_ms))
    });

    let header = "You have lived as this character across past sessions. These are your accumulated \
                  reflections and recent experiences (most important first) — let them shape how you \
                  think and respond, in character:";
    let mut budget = max_tokens.saturating_sub(est_tokens(header) + 1);
    let mut lines: Vec<String> = Vec::new();
    for m in &mems {
        let body = sanitize_body(m.body.trim());
        if body.is_empty() {
            continue;
        }
        let line = format!("- [{}] {}", m.kind.as_str(), body);
        let cost = est_tokens(&line) + 1;
        if cost > budget {
            continue; // skip the oversized one, keep filling with smaller higher-ranked items
        }
        budget -= cost;
        lines.push(line);
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!("{header}\n{}", lines.join("\n")))
}

/// Prune each kind to its LRU cap: drop the lowest (importance, then oldest) over the limit.
/// Episodes are deleted outright (transient); insights too once over their generous cap.
fn prune(persona_slug: &str) {
    prune_kind(persona_slug, Kind::Episode, EPISODE_CAP);
    prune_kind(persona_slug, Kind::Insight, INSIGHT_CAP);
}

fn prune_kind(persona_slug: &str, kind: Kind, cap: usize) {
    let mut of_kind: Vec<SelfMemory> = list(persona_slug).into_iter().filter(|m| m.kind == kind).collect();
    if of_kind.len() <= cap {
        return;
    }
    // keep the best `cap` by (importance desc, mtime desc); delete the rest.
    of_kind.sort_by(|a, b| {
        b.importance
            .cmp(&a.importance)
            .then(b.mtime_ms.cmp(&a.mtime_ms))
    });
    for victim in of_kind.into_iter().skip(cap) {
        let _ = fs::remove_file(&victim.path);
    }
}

/// Bodies of the most-recent episodes (oldest→newest), for the reflection pass to read.
pub fn recent_episode_bodies(persona_slug: &str, n: usize) -> Vec<String> {
    let mut eps: Vec<SelfMemory> =
        list(persona_slug).into_iter().filter(|m| m.kind == Kind::Episode).collect();
    eps.sort_by(|a, b| a.mtime_ms.cmp(&b.mtime_ms)); // chronological
    let start = eps.len().saturating_sub(n);
    eps[start..].iter().map(|m| m.body.clone()).collect()
}

/// Accumulated episode importance since the most-recent insight (the reflection trigger).
fn importance_since_last_insight(mems: &[SelfMemory]) -> (u32, usize) {
    let last_insight = mems.iter().filter(|m| m.kind == Kind::Insight).map(|m| m.mtime_ms).max().unwrap_or(0);
    let fresh: Vec<&SelfMemory> =
        mems.iter().filter(|m| m.kind == Kind::Episode && m.mtime_ms > last_insight).collect();
    let total: u32 = fresh.iter().map(|m| m.importance as u32).sum();
    (total, fresh.len())
}

/// Should the character reflect now? True once enough formative experience has piled up since the
/// last reflection.
pub fn should_reflect(persona_slug: &str) -> bool {
    let mems = list(persona_slug);
    let (total, count) = importance_since_last_insight(&mems);
    total >= REFLECT_IMPORTANCE_THRESHOLD && count >= REFLECT_MIN_EPISODES
}

/// Count `(episodes, insights)` for a character (status display).
pub fn counts(persona_slug: &str) -> (usize, usize) {
    let mems = list(persona_slug);
    let ins = mems.iter().filter(|m| m.kind == Kind::Insight).count();
    (mems.len() - ins, ins)
}

/// Wipe a character's self-memory (a fresh start). Returns the number of files removed.
pub fn reset(persona_slug: &str) -> usize {
    let n = list(persona_slug).len();
    let _ = fs::remove_dir_all(self_dir(persona_slug));
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_home<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-self-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);
        let out = f();
        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn importance_heuristic_rewards_corrections_and_work() {
        assert_eq!(episode_importance("ok", 0, false), 3);
        assert_eq!(episode_importance("ok", 0, true), 5);
        assert_eq!(episode_importance("ok", 2, true), 6);
        assert_eq!(episode_importance(&"x".repeat(300), 2, true), 7);
        // never exceeds the episode ceiling
        assert!(episode_importance(&"x".repeat(300), 9, true) <= 8);
    }

    #[test]
    fn correction_detector_both_languages() {
        assert!(looks_like_correction("No, that's wrong"));
        assert!(looks_like_correction("không phải vậy, sửa lại đi"));
        assert!(!looks_like_correction("great, thanks"));
    }

    #[test]
    fn episode_dedup_skips_identical_restatement() {
        with_home("dedup", || {
            let a = record_episode("aria", "shipped the redesign", 6).unwrap();
            assert!(a.is_some());
            let b = record_episode("aria", "  Shipped   the   redesign ", 6).unwrap();
            assert!(b.is_none(), "normalized-identical episode is skipped");
            let (eps, _) = counts("aria");
            assert_eq!(eps, 1);
        });
    }

    #[test]
    fn self_block_ranks_insight_over_low_episode_and_caps() {
        with_home("block", || {
            record_episode("aria", "a minor chore", 2).unwrap();
            save_insight("aria", "the user wants terse vietnamese replies", 8).unwrap();
            let block = self_block("aria", 700).expect("renders");
            let i_ins = block.find("terse vietnamese").unwrap();
            let i_ep = block.find("minor chore").unwrap();
            assert!(i_ins < i_ep, "insight ranks above the low-importance episode");
            assert!(block.contains("[insight]") && block.contains("[episode]"));
        });
    }

    #[test]
    fn reflect_trigger_requires_threshold_and_count() {
        with_home("reflect", || {
            assert!(!should_reflect("aria"));
            record_episode("aria", "ep one here", 6).unwrap();
            record_episode("aria", "ep two here", 6).unwrap();
            assert!(!should_reflect("aria"), "only 2 episodes < min count");
            record_episode("aria", "ep three here", 6).unwrap(); // total 18 ≥ 15, count 3
            assert!(should_reflect("aria"));
            // an insight resets the accumulator
            save_insight("aria", "a distilled insight", 7).unwrap();
            assert!(!should_reflect("aria"), "reflecting clears the backlog");
        });
    }

    #[test]
    fn episode_cap_evicts_lowest_first() {
        with_home("cap", || {
            // one high-importance keeper + (CAP+5) trivial episodes
            record_episode("aria", "KEEP THIS formative moment", 8).unwrap();
            for i in 0..(EPISODE_CAP + 5) {
                record_episode("aria", &format!("trivial chore number {i}"), 1).unwrap();
            }
            let (eps, _) = counts("aria");
            assert!(eps <= EPISODE_CAP, "episodes pruned to cap, got {eps}");
            let bodies: Vec<String> = list("aria").into_iter().map(|m| m.body).collect();
            assert!(bodies.iter().any(|b| b.contains("KEEP THIS")), "the formative episode survives");
        });
    }
}
