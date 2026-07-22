//! Persona-scoped self-memory — the `<self>` layer + the substrate the reflection pass reads/writes.
//!
//! Evolutionary design (lean Generative-Agents × MemoryBank × CoALA × A-MEM, free-first):
//! - **Event-gated episodes** — never log every turn. Only formative signals (correction, preference,
//!   remember, substantial work, explicit CLI). Small-talk dies at the gate (fixes the "hello" spam).
//! - **Typed free notes** — `bond | correction | work | preference | explicit` bodies, no model call
//!   on write (A-MEM-style structure without paid note construction).
//! - **Episodic → semantic** — raw episodes are transient; periodic reflection distills **character /
//!   relationship / working-style** insights (CoALA: semantic is always-on, episodic is substrate).
//! - **Insight-first injection** — `<self>` is mostly durable insights; only a few high-importance
//!   "hot" episodes leak in. Matches MemoryBank/MemGPT working-context discipline.
//! - **Ebbinghaus-ish rank** — `importance × recency` with a long half-life; near-dup window skips
//!   restatements instead of writing `-2/-3` noise files.
//!
//! Storage: `~/.aizen/personas/<slug>.self/*.md`, one memory per file:
//! ```text
//! ---
//! kind: insight        # episode | insight
//! importance: 7        # 0..=10
//! created: 2026-06-21
//! updated: 2026-06-21
//! ---
//! body…
//! ```

use crate::memory::frontmatter;
use crate::memory::learning::signals::{self, SignalKind};
use crate::memory::render::{est_tokens, sanitize_body};
use crate::memory::score::recency_factor;
use crate::persona::personas_dir;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Keep at most this many episodes per character (LRU by importance then age). Raw experience is
/// transient — the reflection pass lifts the load-bearing parts into durable insights.
pub const EPISODE_CAP: usize = 40;
/// Keep at most this many reflected insights (the durable character layer).
pub const INSIGHT_CAP: usize = 40;
/// Reflect once *formative* episode importance since the last insight crosses this.
/// (Generative Agents uses ~150 with LLM-scored poignancy 1–10; our free scale is denser, so 12.)
pub const REFLECT_IMPORTANCE_THRESHOLD: u32 = 12;
/// …and only if at least this many fresh *formative* episodes have piled up.
pub const REFLECT_MIN_EPISODES: usize = 2;
/// Episodes below this never count toward reflection and never enter `<self>` (noise floor).
pub const FORMATIVE_MIN: u8 = 5;
/// Hot episodes that may join the always-on block (insights still win).
const HOT_EPISODE_INJECT_MIN: u8 = 6;
/// Max raw episodes allowed in `<self>` after insights take the budget.
const MAX_HOT_EPISODES_IN_BLOCK: usize = 2;
/// Near-dup window: skip if body is identical/near-identical to any of the N newest episodes.
const DEDUP_WINDOW: usize = 12;
/// Recency half-life (days). Longer than user-fact half-life — formative character experience fades slowly.
const SELF_HALF_LIFE_DAYS: f64 = 45.0;
/// Insights heavily outrank episodes of equal importance/age (distilled > raw).
const INSIGHT_RANK_BONUS: f64 = 2.0;

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
    let lock_path = crate::core::workspace_txn::store_lock("persona_self", persona_slug);
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        std::time::Duration::from_secs(5),
    )?;
    let prefix = if kind == Kind::Insight { "in" } else { "ep" };
    let path = unique_path(&dir, prefix, body);
    let now = today();
    crate::core::persist::atomic_write_owner_only(
        &path,
        render(kind, importance, &now, &now, body).as_bytes(),
    )?;
    Ok(path.file_stem().and_then(|s| s.to_str()).unwrap_or("mem").to_string())
}

/// Record a free episode (zero model cost). Near-dups against a window of recent episodes *and*
/// existing insights are skipped; then prune to the LRU cap. Returns the new id, or `Ok(None)`
/// when skipped as empty/duplicate.
pub fn record_episode(persona_slug: &str, body: &str, importance: u8) -> Result<Option<String>> {
    let body = body.trim();
    if body.is_empty() {
        return Ok(None);
    }
    // Floor: never persist sub-formative noise even if a caller forgot to gate.
    if importance < FORMATIVE_MIN {
        return Ok(None);
    }
    if is_near_duplicate(persona_slug, body) {
        return Ok(None);
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

/// Token set for cheap near-dup (Jaccard). Short stopwords stripped so "the user asked hello"
/// doesn't thrash on function words.
fn content_tokens(s: &str) -> std::collections::HashSet<String> {
    const STOP: &[&str] = &[
        "the", "a", "an", "i", "me", "my", "you", "your", "and", "or", "to", "of", "in", "on", "for",
        "is", "are", "was", "were", "it", "this", "that", "with", "as", "at", "be", "have", "has",
        "user", "asked", "answered", "directly", "via", "steps", "tool", "tools",
    ];
    normalize(s)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2 && !STOP.contains(t))
        .map(str::to_string)
        .collect()
}

fn jaccard(a: &std::collections::HashSet<String>, b: &std::collections::HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 1.0;
    }
    let inter = a.intersection(b).count() as f64;
    let union = a.union(b).count() as f64;
    if union == 0.0 {
        0.0
    } else {
        inter / union
    }
}

/// Near-dup against any of the N newest episodes (not just the last one — that let hello-2/3 slip).
fn is_near_duplicate(persona_slug: &str, body: &str) -> bool {
    let cand = content_tokens(body);
    let norm = normalize(body);
    let mut eps: Vec<SelfMemory> =
        list(persona_slug).into_iter().filter(|m| m.kind == Kind::Episode).collect();
    eps.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms));
    for m in eps.into_iter().take(DEDUP_WINDOW) {
        if normalize(&m.body) == norm {
            return true;
        }
        if jaccard(&cand, &content_tokens(&m.body)) >= 0.82 {
            return true;
        }
    }
    // Also skip if an insight already covers the same content (evolution already distilled it).
    for m in list(persona_slug).into_iter().filter(|m| m.kind == Kind::Insight) {
        if jaccard(&cand, &content_tokens(&m.body)) >= 0.75 {
            return true;
        }
    }
    false
}

/// Kind of formative event (free typed note — A-MEM structure without a paid LLM write).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventKind {
    Correction,
    Preference,
    Remember,
    Work,
    /// Reserved for relationship-only moments (explicit CLI / future detectors).
    #[allow(dead_code)]
    Bond,
    Explicit,
}

impl EventKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EventKind::Correction => "correction",
            EventKind::Preference => "preference",
            EventKind::Remember => "preference",
            EventKind::Work => "work",
            EventKind::Bond => "bond",
            EventKind::Explicit => "explicit",
        }
    }
}

/// What the free gate decided about a turn. `None` importance ⇒ skip (not formative).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnSalience {
    pub kind: EventKind,
    pub importance: u8,
}

/// Lowercase contains-any correction-signal detector (drives episode importance). A turn where the
/// user pushes back / redirects is more formative than a routine one.
pub fn looks_like_correction(text: &str) -> bool {
    // Prefer the shared learning signal (EN+VI regex) when it fires; fall back to a short bilingual
    // keyword list so persona evolution stays useful even if signal rules drift.
    if signals::detect(text).kind == SignalKind::Correction {
        return true;
    }
    let t = text.to_lowercase();
    const SIGNALS: &[&str] = &[
        "no,", "nope", "actually", "instead", "wrong", "don't", "do not", "stop", "revert", "undo",
        "incorrect", "not right", "that's not", // en
        "không phải", "sai rồi", "sai ròi", "đừng", "sửa lại", "thực ra", "thay vào", // vi
    ];
    SIGNALS.iter().any(|s| t.contains(s))
}

/// Cheap small-talk / phatic detector. These turns must NEVER become self-memory.
pub fn is_smalltalk(text: &str) -> bool {
    let t = normalize(text);
    if t.is_empty() {
        return true;
    }
    // Very short pure greetings / acks.
    const PHATIC: &[&str] = &[
        "hi", "hello", "hey", "yo", "sup", "hola", "ok", "okay", "k", "kk", "thanks", "thank you",
        "ty", "thx", "bye", "good morning", "good night", "good evening", "gm", "gn",
        "xin chào", "chào", "chào bạn", "chào em", "chào anh", "cảm ơn", "cam on", "ok luôn",
        "ừ", "uh", "ừm", "umm", "alo", "test", "ping",
    ];
    if PHATIC.iter().any(|p| t == *p) {
        return true;
    }
    // "hello!" / "chào 👋" / "hi there" with almost no content.
    let alpha: String = t.chars().filter(|c| c.is_alphanumeric() || c.is_whitespace()).collect();
    let words: Vec<&str> = alpha.split_whitespace().collect();
    if words.len() <= 2 {
        const GREET_HEAD: &[&str] =
            &["hi", "hello", "hey", "chào", "chao", "thanks", "thank", "ok", "okay", "xin"];
        if words.first().map(|w| GREET_HEAD.contains(w)).unwrap_or(false) {
            return true;
        }
    }
    false
}

/// Free formative gate. Returns `None` when the turn is not worth remembering (the common case).
///
/// Salience ladder (highest wins):
/// 1. correction  → 7–8
/// 2. remember / preference signal → 6–7
/// 3. substantial tool work (≥2 calls + non-trivial user text) → 5–6
/// 4. explicit CLI remember → 6–8 (caller forces Explicit)
///
/// Small-talk and passive short turns always die here — no base-3 floor.
pub fn classify_turn(user_text: &str, tool_calls: usize) -> Option<TurnSalience> {
    let text = user_text.trim();
    if text.is_empty() || is_smalltalk(text) {
        return None;
    }
    let sig = signals::detect(text);
    let corrected = looks_like_correction(text);
    let long = text.chars().count() > 160;
    let substantial_work = tool_calls >= 2 && text.chars().count() >= 24;

    if corrected || sig.kind == SignalKind::Correction {
        let imp = if long { 8 } else { 7 };
        return Some(TurnSalience { kind: EventKind::Correction, importance: imp });
    }
    if sig.kind == SignalKind::Remember {
        return Some(TurnSalience {
            kind: EventKind::Remember,
            importance: if long { 7 } else { 6 },
        });
    }
    if sig.kind == SignalKind::Preference {
        return Some(TurnSalience {
            kind: EventKind::Preference,
            importance: if long { 7 } else { 6 },
        });
    }
    if substantial_work {
        // Work alone is weaker than a correction/preference — still formative enough to reflect on
        // "how we work together", not every bugfix.
        let imp = if tool_calls >= 5 || long { 6 } else { 5 };
        return Some(TurnSalience { kind: EventKind::Work, importance: imp });
    }
    None
}

/// Backward-compatible importance helper (CLI `persona remember` + tests). Prefer [`classify_turn`].
pub fn episode_importance(user_text: &str, tool_calls: usize, corrected: bool) -> u8 {
    if corrected {
        return if user_text.chars().count() > 160 { 8 } else { 7 };
    }
    classify_turn(user_text, tool_calls).map(|s| s.importance).unwrap_or(0)
}

/// Build a typed free episode body (no model cost). Keeps the reflection substrate clean.
pub fn format_episode_body(
    kind: EventKind,
    user_text: &str,
    tool_calls: usize,
    assistant_gist: &str,
) -> String {
    let ask = truncate_words(user_text.trim(), 28);
    match kind {
        EventKind::Correction => format!("correction: user redirected me — \"{ask}\""),
        EventKind::Preference | EventKind::Remember => {
            format!("preference: user wants — \"{ask}\"")
        }
        EventKind::Work => {
            let gist = assistant_gist.trim();
            if gist.is_empty() {
                format!("work: handled \"{ask}\" via {tool_calls} tool steps")
            } else {
                format!(
                    "work: handled \"{ask}\" via {tool_calls} tool steps — {}",
                    truncate_words(gist, 18)
                )
            }
        }
        EventKind::Bond | EventKind::Explicit => format!("{}: \"{ask}\"", kind.as_str()),
    }
}

fn truncate_words(s: &str, max_words: usize) -> String {
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.len() <= max_words {
        return words.join(" ");
    }
    format!("{}…", words[..max_words].join(" "))
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

/// The inner `<self>` block: **insight-first** (CoALA semantic / MemoryBank profile), then at most
/// a couple of hot formative episodes. Capped at `max_tokens`. `None` when empty.
pub fn self_block(persona_slug: &str, max_tokens: usize) -> Option<String> {
    let mems = list(persona_slug);
    if mems.is_empty() {
        return None;
    }
    let today = chrono::Local::now().date_naive();
    let mut insights: Vec<&SelfMemory> = mems.iter().filter(|m| m.kind == Kind::Insight).collect();
    let mut hot_eps: Vec<&SelfMemory> = mems
        .iter()
        .filter(|m| m.kind == Kind::Episode && m.importance >= HOT_EPISODE_INJECT_MIN)
        .collect();
    insights.sort_by(|a, b| {
        rank(b, today)
            .partial_cmp(&rank(a, today))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.mtime_ms.cmp(&a.mtime_ms))
    });
    hot_eps.sort_by(|a, b| {
        rank(b, today)
            .partial_cmp(&rank(a, today))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.mtime_ms.cmp(&a.mtime_ms))
    });

    let header = "You have lived as this character across past sessions. These are your distilled \
                  insights (and a few recent formative moments) — let them shape how you think and \
                  respond, in character. Prefer insights over raw episodes:";
    let mut budget = max_tokens.saturating_sub(est_tokens(header) + 1);
    let mut lines: Vec<String> = Vec::new();

    // Pass 1: insights fill the budget.
    for m in &insights {
        let body = sanitize_body(m.body.trim());
        if body.is_empty() {
            continue;
        }
        let line = format!("- [insight] {body}");
        let cost = est_tokens(&line) + 1;
        if cost > budget {
            continue;
        }
        budget -= cost;
        lines.push(line);
    }
    // Pass 2: at most MAX_HOT_EPISODES_IN_BLOCK high-importance episodes.
    let mut eps_taken = 0usize;
    for m in &hot_eps {
        if eps_taken >= MAX_HOT_EPISODES_IN_BLOCK {
            break;
        }
        let body = sanitize_body(m.body.trim());
        if body.is_empty() {
            continue;
        }
        let line = format!("- [episode] {body}");
        let cost = est_tokens(&line) + 1;
        if cost > budget {
            continue;
        }
        budget -= cost;
        lines.push(line);
        eps_taken += 1;
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

/// Bodies of the most-recent *formative* episodes (oldest→newest) for the reflection pass.
/// Noise episodes (importance < FORMATIVE_MIN) are excluded so reflection never distills "hello".
pub fn recent_episode_bodies(persona_slug: &str, n: usize) -> Vec<String> {
    let mut eps: Vec<SelfMemory> = list(persona_slug)
        .into_iter()
        .filter(|m| m.kind == Kind::Episode && m.importance >= FORMATIVE_MIN)
        .collect();
    eps.sort_by(|a, b| a.mtime_ms.cmp(&b.mtime_ms)); // chronological
    let start = eps.len().saturating_sub(n);
    eps[start..].iter().map(|m| m.body.clone()).collect()
}

/// Accumulated *formative* episode importance since the most-recent insight (reflection trigger).
fn importance_since_last_insight(mems: &[SelfMemory]) -> (u32, usize) {
    let last_insight = mems.iter().filter(|m| m.kind == Kind::Insight).map(|m| m.mtime_ms).max().unwrap_or(0);
    let fresh: Vec<&SelfMemory> = mems
        .iter()
        .filter(|m| {
            m.kind == Kind::Episode
                && m.importance >= FORMATIVE_MIN
                && m.mtime_ms > last_insight
        })
        .collect();
    let total: u32 = fresh.iter().map(|m| m.importance as u32).sum();
    (total, fresh.len())
}

/// Should the character reflect now? True once enough formative experience has piled up since the
/// last reflection (Generative-Agents-style importance threshold, free-scale).
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
    fn smalltalk_and_passive_die_at_gate() {
        assert!(classify_turn("hello", 0).is_none());
        assert!(classify_turn("hi!", 0).is_none());
        assert!(classify_turn("chào", 0).is_none());
        assert!(classify_turn("thanks", 0).is_none());
        assert!(classify_turn("ok", 1).is_none());
        // Passive short question with no tool work → not formative for the character.
        assert!(classify_turn("what time is it", 0).is_none());
        assert!(is_smalltalk("hello"));
        assert!(!is_smalltalk("I prefer terse Vietnamese replies"));
    }

    #[test]
    fn formative_gate_ranks_correction_preference_work() {
        let c = classify_turn("No, that's wrong — use tabs not spaces", 0).unwrap();
        assert_eq!(c.kind, EventKind::Correction);
        assert!(c.importance >= 7);

        let p = classify_turn("I prefer pnpm over npm for everything", 0).unwrap();
        assert_eq!(p.kind, EventKind::Preference);
        assert!(p.importance >= 6);

        let w = classify_turn("please fix the flaky parse test in config.rs", 4).unwrap();
        assert_eq!(w.kind, EventKind::Work);
        assert!(w.importance >= 5);

        // One tool + short passive → still skip.
        assert!(classify_turn("open that file", 1).is_none());
    }

    #[test]
    fn importance_helper_no_base_floor() {
        assert_eq!(episode_importance("ok", 0, false), 0);
        assert_eq!(episode_importance("hello", 0, false), 0);
        assert!(episode_importance("no, that's wrong", 0, true) >= 7);
        assert!(episode_importance("please fix the flaky parse test carefully", 4, false) >= 5);
    }

    #[test]
    fn correction_detector_both_languages() {
        assert!(looks_like_correction("No, that's wrong"));
        assert!(looks_like_correction("không phải vậy, sửa lại đi"));
        assert!(!looks_like_correction("great, thanks"));
    }

    #[test]
    fn typed_body_is_not_a_transcript_dump() {
        let b = format_episode_body(EventKind::Correction, "No use tabs", 0, "");
        assert!(b.starts_with("correction:"));
        assert!(!b.contains("I answered directly"));
        let w = format_episode_body(EventKind::Work, "fix the parse test", 3, "patched config.rs");
        assert!(w.starts_with("work:"));
        assert!(w.contains("3 tool"));
    }

    #[test]
    fn episode_dedup_window_not_only_last() {
        with_home("dedup", || {
            let a = record_episode("aria", "correction: user redirected me — \"use tabs\"", 7).unwrap();
            assert!(a.is_some());
            // Intervening different episode.
            let mid = record_episode("aria", "work: handled \"fix parse\" via 3 tool steps", 6).unwrap();
            assert!(mid.is_some());
            // Near-identical to the FIRST episode (not last) must still skip.
            let b = record_episode("aria", "correction: user redirected me — \"use tabs\"", 7).unwrap();
            assert!(b.is_none(), "near-dup against window must skip");
            let (eps, _) = counts("aria");
            assert_eq!(eps, 2);
        });
    }

    #[test]
    fn record_rejects_sub_formative_importance() {
        with_home("floor", || {
            let r = record_episode("aria", "hello there friend", 3).unwrap();
            assert!(r.is_none(), "importance < FORMATIVE_MIN must not persist");
            let (eps, _) = counts("aria");
            assert_eq!(eps, 0);
        });
    }

    #[test]
    fn self_block_insight_first_hides_low_episodes() {
        with_home("block", || {
            // Low/work-floor episode should NOT appear (below HOT inject min 6).
            record_episode("aria", "work: handled \"chore\" via 2 tool steps", 5).unwrap();
            save_insight("aria", "the user wants terse vietnamese replies", 8).unwrap();
            let block = self_block("aria", 700).expect("renders");
            assert!(block.contains("terse vietnamese"), "insight present");
            assert!(block.contains("[insight]"));
            assert!(
                !block.contains("chore"),
                "low-importance episode must not pollute always-on self: {block}"
            );
        });
    }

    #[test]
    fn self_block_allows_hot_episode() {
        with_home("hot", || {
            record_episode("aria", "correction: user redirected me — \"never force-push main\"", 8).unwrap();
            let block = self_block("aria", 700).expect("hot episode injects when no insights");
            assert!(block.contains("[episode]"));
            assert!(block.contains("force-push"));
        });
    }

    #[test]
    fn reflect_trigger_uses_formative_only() {
        with_home("reflect", || {
            assert!(!should_reflect("aria"));
            // Two formative corrections (7+7=14 ≥ 12, count 2) → fire.
            record_episode("aria", "correction: user redirected me — \"tabs\"", 7).unwrap();
            assert!(!should_reflect("aria"), "need min episode count");
            record_episode("aria", "correction: user redirected me — \"spaces no\"", 7).unwrap();
            assert!(should_reflect("aria"));
            save_insight("aria", "user prefers tabs over spaces", 7).unwrap();
            assert!(!should_reflect("aria"), "insight clears backlog");
        });
    }

    #[test]
    fn episode_cap_evicts_lowest_first() {
        with_home("cap", || {
            record_episode("aria", "KEEP THIS formative moment about trust", 8).unwrap();
            for i in 0..(EPISODE_CAP + 5) {
                // importance 5 meets floor so they actually write, then get pruned.
                record_episode("aria", &format!("work: handled \"chore number {i}\" via 2 tool steps"), 5)
                    .unwrap();
            }
            let (eps, _) = counts("aria");
            assert!(eps <= EPISODE_CAP, "episodes pruned to cap, got {eps}");
            let bodies: Vec<String> = list("aria").into_iter().map(|m| m.body).collect();
            assert!(bodies.iter().any(|b| b.contains("KEEP THIS")), "the formative episode survives");
        });
    }
}
