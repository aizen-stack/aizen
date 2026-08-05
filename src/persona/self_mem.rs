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
    let created = fm
        .get("created")
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());
    let updated = fm
        .get("updated")
        .map(str::to_string)
        .filter(|s| !s.trim().is_empty());
    let mtime_ms = fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis())
        .unwrap_or(0);
    Some(SelfMemory {
        id,
        path: path.to_path_buf(),
        kind,
        importance,
        created,
        updated,
        body: fm.body,
        mtime_ms,
    })
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
        if p.extension()
            .and_then(|x| x.to_str())
            .map(|x| x.eq_ignore_ascii_case("md"))
            != Some(true)
        {
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

/// Words worth putting in a filename — the boilerplate lead-in that [`format_episode_body`] stamps
/// on every episode is not one of them.
///
/// Each body opens with its own type label (`correction: user redirected me — "…"`), so the first
/// four or five words are identical across every episode of that kind. A stem taken from the head of
/// the body therefore described the FORMAT, not the memory: 12 files on a real machine all read
/// `ep-correction-user-redirected-me-todo`, distinguished only by a `-2`…`-12` counter. Dropping the
/// label words lets the stem start where the content does.
fn stem_source(body: &str) -> &str {
    let b = body.trim_start();
    for label in [
        "correction: user redirected me —",
        "preference: user wants —",
        "work: handled",
        "bond:",
        "explicit:",
        "remember:",
    ] {
        if let Some(rest) = b.strip_prefix(label) {
            return rest.trim_start_matches([' ', '"', '—', '-']);
        }
    }
    b
}

/// Words that carry no meaning in a filename. `[todo-poke]`-style scaffolding repeats across
/// unrelated memories, so it would re-create the collision `stem_source` just removed.
fn is_filler(word: &str) -> bool {
    const FILLER: &[&str] = &[
        "todo", "poke", "session", "todos", "are", "still", "incomplete", "you", "may", "not",
        "finish", "yet", "the", "a", "an", "and", "or", "to", "of", "in", "on", "for", "is", "it",
        "this", "that", "with", "i", "me", "my", "user", "va", "la", "cua", "co", "khong", "cac",
    ];
    FILLER.contains(&word)
}

/// The stem a body would be filed under: `<prefix>-<words>-<hash4>`, without the collision counter.
///
/// Split out of [`unique_path`] so `migrate_stems` can recompute the correct name for a file already
/// on disk. Deterministic and side-effect-free — the same body always yields the same stem.
pub(crate) fn stem_for(prefix: &str, body: &str) -> String {
    let words: Vec<String> = crate::core::slug::slug_words(stem_source(body), usize::MAX)
        .split('-')
        .filter(|w| w.chars().count() >= 2 && !is_filler(w))
        .take(6)
        .map(str::to_string)
        .collect();
    let slug = words.join("-");
    let slug = if slug.is_empty() {
        "mem".to_string()
    } else {
        crate::core::slug::truncate_at_word(&slug, 48)
    };
    // Content hash over the WHOLE body, so two memories sharing a lead-in still differ.
    format!("{prefix}-{slug}-{:04x}", simple_hash(body.trim()) as u16)
}

/// What the migration needs from a file on disk: its kind and its body.
pub(crate) struct MigrationRow {
    pub is_insight: bool,
    pub body: String,
}

/// Parse just enough of a self-memory file for `migrate_stems` to recompute its stem. `None` when
/// there is no body to recompute from — inventing a name would be exactly the guess to avoid.
pub(crate) fn parse_for_migration(raw: &str) -> Option<MigrationRow> {
    let fm = frontmatter::parse(raw);
    let body = fm.body.trim();
    if body.is_empty() {
        return None;
    }
    let is_insight = fm
        .get("kind")
        .map(|k| k.trim().eq_ignore_ascii_case("insight"))
        .unwrap_or(false);
    Some(MigrationRow {
        is_insight,
        body: body.to_string(),
    })
}

/// A filename for a new self-memory: `<prefix>-<words>-<hash4>.md`.
///
/// Two separate defects were fixed here, and they need different medicine:
///
/// 1. **Shredding.** The old stem tested one codepoint at a time against `is_ascii_alphanumeric`, so
///    every accented letter became a word separator and cut inside words — 45 of 89 files on a real
///    machine were named like `in-t-i-n-n-l-9`. Folding through [`crate::core::slug`] fixes that; it
///    is the same helper the memory store and session names use.
/// 2. **Collision.** Even with perfect folding, bodies that share a lead-in share a stem. Widening
///    the word count would not fix it (the shared prefix just gets longer), so the stem skips the
///    type label and filler, and a short content hash is appended. The hash makes the name unique by
///    construction rather than by a `-2`…`-12` counter, so a stem now identifies ONE memory.
///
/// The collision loop stays as the last resort: a hash is 16 bits, and two memories that truly
/// collide must still both be writable.
fn unique_path(dir: &Path, prefix: &str, body: &str) -> PathBuf {
    let base = stem_for(prefix, body);
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

/// FNV-1a, folded to 32 bits — a filename disambiguator, not a security primitive.
fn simple_hash(s: &str) -> u32 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (h ^ (h >> 32)) as u32
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
    Ok(path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("mem")
        .to_string())
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

/// Wire a freshly-saved insight to the facts recalled while it was formed (Hebbian, cross-kind).
///
/// Best-effort and silent, like the fact-to-fact spine: no recall ledger this turn, or a disabled
/// graph, simply means no signal. What it buys is the ability to ask later "which of this character's
/// lessons belong to the situation we are in now" from the facts alone.
pub fn note_insight_cofire(persona_slug: &str, insight_id: &str) {
    if !crate::memory::graph::recording_enabled() {
        return;
    }
    let facts = crate::memory::pending::current();
    if facts.is_empty() {
        return;
    }
    let node = crate::memory::graph::node_persona(persona_slug, insight_id);
    let mut ids: Vec<&str> = vec![node.as_str()];
    ids.extend(facts.iter().map(|p| p.id.as_str()));
    let today = crate::memory::bloat::decay::today();
    let _ = crate::memory::graph::record_coretrieval(&ids, &today);
}

fn normalize(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Token set for cheap near-dup (Jaccard). Short stopwords stripped so "the user asked hello"
/// doesn't thrash on function words.
///
/// The list is BILINGUAL because the episodes are. With English stopwords only, two Vietnamese
/// insights saying the same thing shared almost nothing but function words: measured across 40 real
/// insights the highest pairwise Jaccard was 0.15 against a 0.75 threshold, so the dedup gate never
/// fired and one idea accumulated a dozen near-identical copies. Stripping Vietnamese function words
/// leaves the content words the comparison is actually about.
fn content_tokens(s: &str) -> std::collections::HashSet<String> {
    const STOP: &[&str] = &[
        // English
        "the", "a", "an", "i", "me", "my", "you", "your", "and", "or", "to", "of", "in", "on",
        "for", "is", "are", "was", "were", "it", "this", "that", "with", "as", "at", "be", "have",
        "has", "user", "asked", "answered", "directly", "via", "steps", "tool", "tools",
        // Vietnamese — pronouns, copulas, prepositions, determiners, discourse particles. These are
        // the words that dominate a short Vietnamese sentence, so leaving them in made every pair
        // look 15% alike and no pair look 75% alike.
        "tôi", "toi", "bạn", "ban", "anh", "em", "mình", "minh", "người", "nguoi", "dùng", "dung",
        "là", "la", "và", "va", "của", "cua", "cho", "với", "voi", "khi", "một", "mot", "này",
        "nay", "đó", "do", "được", "duoc", "có", "co", "không", "khong", "thì", "thi", "mà", "ma",
        "nên", "nen", "cần", "can", "phải", "phai", "sẽ", "se", "đã", "da", "đang", "thay", "vì",
        "vi", "để", "de", "các", "cac", "những", "nhung", "ở", "trong", "ra", "vào", "vao", "lại",
        "lai", "rồi", "roi", "nữa", "nua", "hơn", "hon", "rất", "rat", "cũng", "cung", "chỉ",
        "chi", "theo", "sau", "trước", "truoc", "hay", "hoặc", "hoac", "nếu", "neu", "bằng",
        "bang", "về", "ve", "từ", "tu", "đến", "den", "gì", "gi", "nào", "nao", "sao", "thế",
        "the", "việc", "viec", "cái", "cai",
    ];
    normalize(s)
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 2 && !STOP.contains(t))
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
    let mut eps: Vec<SelfMemory> = list(persona_slug)
        .into_iter()
        .filter(|m| m.kind == Kind::Episode)
        .collect();
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
    for m in list(persona_slug)
        .into_iter()
        .filter(|m| m.kind == Kind::Insight)
    {
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
        "no,",
        "nope",
        "actually",
        "instead",
        "wrong",
        "don't",
        "do not",
        "stop",
        "revert",
        "undo",
        "incorrect",
        "not right",
        "that's not", // en
        "không phải",
        "sai rồi",
        "sai ròi",
        "đừng",
        "sửa lại",
        "thực ra",
        "thay vào", // vi
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
        "hi",
        "hello",
        "hey",
        "yo",
        "sup",
        "hola",
        "ok",
        "okay",
        "k",
        "kk",
        "thanks",
        "thank you",
        "ty",
        "thx",
        "bye",
        "good morning",
        "good night",
        "good evening",
        "gm",
        "gn",
        "xin chào",
        "chào",
        "chào bạn",
        "chào em",
        "chào anh",
        "cảm ơn",
        "cam on",
        "ok luôn",
        "ừ",
        "uh",
        "ừm",
        "umm",
        "alo",
        "test",
        "ping",
    ];
    if PHATIC.iter().any(|p| t == *p) {
        return true;
    }
    // "hello!" / "chào 👋" / "hi there" with almost no content.
    let alpha: String = t
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect();
    let words: Vec<&str> = alpha.split_whitespace().collect();
    if words.len() <= 2 {
        const GREET_HEAD: &[&str] = &[
            "hi", "hello", "hey", "chào", "chao", "thanks", "thank", "ok", "okay", "xin",
        ];
        if words
            .first()
            .map(|w| GREET_HEAD.contains(w))
            .unwrap_or(false)
        {
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
        return Some(TurnSalience {
            kind: EventKind::Correction,
            importance: imp,
        });
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
        return Some(TurnSalience {
            kind: EventKind::Work,
            importance: imp,
        });
    }
    None
}

/// Backward-compatible importance helper (CLI `persona remember` + tests). Prefer [`classify_turn`].
pub fn episode_importance(user_text: &str, tool_calls: usize, corrected: bool) -> u8 {
    if corrected {
        return if user_text.chars().count() > 160 {
            8
        } else {
            7
        };
    }
    classify_turn(user_text, tool_calls)
        .map(|s| s.importance)
        .unwrap_or(0)
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
    match created
        .as_deref()
        .and_then(|s| chrono::NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").ok())
    {
        Some(d) => (today - d).num_days().max(0) as f64,
        None => 0.0,
    }
}

fn rank(m: &SelfMemory, today: chrono::NaiveDate) -> f64 {
    let base = (m.importance as f64 / 10.0)
        * recency_factor(age_days(&m.created, today), SELF_HALF_LIFE_DAYS);
    if m.kind == Kind::Insight {
        base * INSIGHT_RANK_BONUS
    } else {
        base
    }
}

/// The inner `<self>` block: **insight-first** (CoALA semantic / MemoryBank profile), then at most
/// a couple of hot formative episodes. Capped at `max_tokens`. `None` when empty.
///
/// This rides the **dynamic system lane**, which carries its own cache breakpoint, so its steady-state
/// cost is a cache read rather than fresh tokens. That is why it is deliberately NOT gated on the
/// user's query the way [`crate::skills::turn_block`] and [`crate::memory::recall_block`] are: a
/// per-turn selection here would rewrite lane 1 every turn and force the whole transcript after it
/// to re-bill uncached, costing far more than a tighter block could save.
///
/// A redundancy gate was tried here and removed: measured across the 40 real insights of a saturated
/// persona, the highest pairwise Jaccard was 0.455, so any threshold worth setting (≥0.5) collapsed
/// zero pairs. Genuine near-duplicates in this store restate an idea in different words, which is an
/// embedding question, not a token-overlap one. The honest fix is curation — see `persona insights`.
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

/// Prune each kind to its LRU cap: move the lowest (importance, then oldest) over the limit
/// into `<slug>.self/.archive/`. Nothing is hard-deleted — a character's accumulated experience
/// is user data, and at a saturated cap this path runs after *every* formative turn.
fn prune(persona_slug: &str) {
    prune_kind(persona_slug, Kind::Episode, EPISODE_CAP);
    prune_kind(persona_slug, Kind::Insight, INSIGHT_CAP);
}

/// Where evicted self-memories go. Not scanned by `list()` (it only reads `*.md` directly under
/// `self_dir`, and a directory has no `.md` extension), so archived items leave the live set.
pub fn archive_dir(persona_slug: &str) -> PathBuf {
    self_dir(persona_slug).join(".archive")
}

fn prune_kind(persona_slug: &str, kind: Kind, cap: usize) {
    let mut of_kind: Vec<SelfMemory> = list(persona_slug)
        .into_iter()
        .filter(|m| m.kind == kind)
        .collect();
    if of_kind.len() <= cap {
        return;
    }
    // keep the best `cap` by (importance desc, mtime desc); archive the rest.
    of_kind.sort_by(|a, b| {
        b.importance
            .cmp(&a.importance)
            .then(b.mtime_ms.cmp(&a.mtime_ms))
    });
    let adir = archive_dir(persona_slug);
    if fs::create_dir_all(&adir).is_err() {
        return; // can't archive → keep the file rather than lose it
    }
    for victim in of_kind.into_iter().skip(cap) {
        // `unique_in` uniquifies on collision: two victims sharing a stem must not overwrite
        // each other, and neither may overwrite an item archived on an earlier turn.
        let dest = crate::memory::bloat::caps::unique_in(&adir, &victim.id);
        let _ = fs::rename(&victim.path, &dest);
    }
}

/// Retire one self-memory by id — soft, into the same `.archive/` dir [`prune_kind`] uses.
///
/// The only way an insight left this store before now was [`prune_kind`] evicting the *lowest-ranked*
/// one once the cap was already full. That makes the cap self-enforcing but leaves no way to remove a
/// specific bad insight: a wrong-but-important one outranks the eviction order forever, and at a
/// saturated 40/40 cap it also blocks the slot a better one would take. This is the missing verb —
/// the same soft-delete-plus-restore contract memory facts and skills already have.
pub fn forget(persona_slug: &str, id: &str) -> Result<PathBuf> {
    let src = self_dir(persona_slug).join(format!("{id}.md"));
    if !src.exists() {
        anyhow::bail!("no self-memory '{id}' for persona '{persona_slug}'");
    }
    let adir = archive_dir(persona_slug);
    fs::create_dir_all(&adir).with_context(|| format!("creating {}", adir.display()))?;
    let dest = crate::memory::bloat::caps::unique_in(&adir, id);
    fs::rename(&src, &dest).with_context(|| format!("retiring {}", src.display()))?;
    Ok(dest)
}

/// Bring a retired self-memory back. Refuses to overwrite a live one — the id is what `forget` and
/// the graph's `persona:` endpoints name, so two files answering to one id would be ambiguous.
pub fn restore(persona_slug: &str, id: &str) -> Result<PathBuf> {
    let src = archive_dir(persona_slug).join(format!("{id}.md"));
    if !src.exists() {
        anyhow::bail!("no retired self-memory '{id}' for persona '{persona_slug}'");
    }
    let dest = self_dir(persona_slug).join(format!("{id}.md"));
    if dest.exists() {
        anyhow::bail!("a live self-memory '{id}' already exists — retire it first");
    }
    fs::create_dir_all(self_dir(persona_slug)).ok();
    fs::rename(&src, &dest).with_context(|| format!("restoring {}", src.display()))?;
    Ok(dest)
}

/// Retired self-memories, newest first, for the review surface.
pub fn list_archive(persona_slug: &str) -> Vec<SelfMemory> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(archive_dir(persona_slug)) else {
        return out;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.extension()
            .and_then(|x| x.to_str())
            .map(|x| x.eq_ignore_ascii_case("md"))
            != Some(true)
        {
            continue;
        }
        if let Some(m) = from_file(&p) {
            out.push(m);
        }
    }
    out.sort_by(|a, b| b.mtime_ms.cmp(&a.mtime_ms));
    out
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
    let last_insight = mems
        .iter()
        .filter(|m| m.kind == Kind::Insight)
        .map(|m| m.mtime_ms)
        .max()
        .unwrap_or(0);
    let fresh: Vec<&SelfMemory> = mems
        .iter()
        .filter(|m| {
            m.kind == Kind::Episode && m.importance >= FORMATIVE_MIN && m.mtime_ms > last_insight
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
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-self-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("AIZEN_HOME", &dir);
        let out = f();
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    /// The dedup gate compares CONTENT words, so its stopword list has to cover the language the
    /// episodes are written in. With English stopwords only, a Vietnamese restatement scored ~0.15
    /// against a 0.75 threshold — the gate never fired and one idea piled up a dozen copies.
    #[test]
    fn vietnamese_function_words_do_not_hide_a_restatement() {
        let a =
            "Tôi nên làm việc chủ động, đi đến kết quả đã triển khai và kiểm chứng thay vì chỉ \
                 giải thích cách làm; khi còn hạng mục dang dở, tôi cần tiếp tục hoàn thiện chúng \
                 trước khi kết thúc.";
        let restated = "Tôi nên làm việc chủ động và kiểm chứng kết quả, khi còn hạng mục dang dở \
                        thì tôi cần tiếp tục hoàn thiện chúng trước khi kết thúc.";
        let j = jaccard(&content_tokens(a), &content_tokens(restated));
        assert!(
            j >= 0.75,
            "a restatement must reach the insight dedup threshold, got {j:.2}"
        );

        // Vietnamese pronouns/copulas carry no signal: two unrelated sentences built from them must
        // NOT look alike, or the gate would start swallowing genuinely new insights.
        let x = "Tôi cần chạy kiểm thử trước khi báo cáo kết quả.";
        let y = "Tôi nên hỏi lại người dùng về phạm vi thay đổi.";
        let ju = jaccard(&content_tokens(x), &content_tokens(y));
        assert!(ju < 0.5, "unrelated sentences must stay apart, got {ju:.2}");

        // Single-syllable Vietnamese words are multi-BYTE: a `len() >= 2` filter kept "ở"/"ừ" as
        // tokens while dropping 2-char ASCII. The filter counts CHARS.
        assert!(
            !content_tokens("ở trong nhà").contains("ở"),
            "one-char tokens are not content"
        );
    }

    #[test]
    fn forget_archives_an_insight_and_restore_brings_it_back() {
        with_home("forget", || {
            let id = save_insight("kira", "I should verify before reporting done", 9).unwrap();
            assert_eq!(counts("kira").1, 1, "one insight live");

            let archived = forget("kira", &id).expect("retire succeeds");
            assert!(archived.exists(), "the file moved, it was not deleted");
            assert_eq!(counts("kira").1, 0, "gone from the live set");
            assert!(
                !self_block("kira", 700)
                    .unwrap_or_default()
                    .contains("verify before reporting"),
                "a retired insight leaves the always-on block"
            );
            assert_eq!(list_archive("kira").len(), 1, "listed as retired");

            restore("kira", &id).expect("restore succeeds");
            assert_eq!(counts("kira").1, 1, "back in the live set");
            assert!(
                self_block("kira", 700)
                    .unwrap()
                    .contains("verify before reporting"),
                "and back in the block, verbatim"
            );
            assert!(list_archive("kira").is_empty());
        });
    }

    #[test]
    fn forget_refuses_unknown_ids_and_restore_refuses_to_collide() {
        with_home("forget-edge", || {
            assert!(
                forget("kira", "nope").is_err(),
                "retiring what isn't there is an error, not a silent no-op"
            );
            let id = save_insight("kira", "some durable lesson worth keeping", 8).unwrap();
            forget("kira", &id).unwrap();
            // A fresh insight can land on the same body-derived stem while the old one sits archived;
            // restoring on top of it would leave two files answering to one id.
            let again = save_insight("kira", "some durable lesson worth keeping", 8).unwrap();
            if again == id {
                assert!(
                    restore("kira", &id).is_err(),
                    "restore must not overwrite a live self-memory"
                );
            }
            assert!(restore("kira", "never-existed").is_err());
        });
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
        let w = format_episode_body(
            EventKind::Work,
            "fix the parse test",
            3,
            "patched config.rs",
        );
        assert!(w.starts_with("work:"));
        assert!(w.contains("3 tool"));
    }

    #[test]
    fn episode_dedup_window_not_only_last() {
        with_home("dedup", || {
            let a =
                record_episode("aria", "correction: user redirected me — \"use tabs\"", 7).unwrap();
            assert!(a.is_some());
            // Intervening different episode.
            let mid =
                record_episode("aria", "work: handled \"fix parse\" via 3 tool steps", 6).unwrap();
            assert!(mid.is_some());
            // Near-identical to the FIRST episode (not last) must still skip.
            let b =
                record_episode("aria", "correction: user redirected me — \"use tabs\"", 7).unwrap();
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
            record_episode(
                "aria",
                "correction: user redirected me — \"never force-push main\"",
                8,
            )
            .unwrap();
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
                record_episode(
                    "aria",
                    &format!("work: handled \"chore number {i}\" via 2 tool steps"),
                    5,
                )
                .unwrap();
            }
            let (eps, _) = counts("aria");
            assert!(eps <= EPISODE_CAP, "episodes pruned to cap, got {eps}");
            let bodies: Vec<String> = list("aria").into_iter().map(|m| m.body).collect();
            assert!(
                bodies.iter().any(|b| b.contains("KEEP THIS")),
                "the formative episode survives"
            );
        });
    }

    #[test]
    fn prune_archives_instead_of_deleting() {
        with_home("prune-archive", || {
            // Bodies must differ ENOUGH to clear the 0.82 near-dup gate, or nothing is ever
            // written past the first one and prune never runs (the older cap test writes
            // "chore number {i}" bodies whose content tokens are identical after stopword
            // stripping, so it asserts `eps <= CAP` against a store holding a single episode).
            let want = EPISODE_CAP + 4;
            for i in 0..want {
                record_episode(
                    "aria",
                    &format!("work: fixed bug ab{i:02} in module cd{i:02}"),
                    5,
                )
                .unwrap();
            }
            let (eps, _) = counts("aria");
            assert_eq!(eps, EPISODE_CAP, "live set held exactly at the cap");

            // The evicted experience is on disk under `.archive/`, not gone. At a saturated cap this
            // path runs after EVERY formative turn, so a hard delete here bleeds user data.
            let archived: Vec<PathBuf> = fs::read_dir(archive_dir("aria"))
                .expect(".archive exists once something was evicted")
                .flatten()
                .map(|e| e.path())
                .collect();
            assert!(
                !archived.is_empty(),
                "evicted episodes are archived, never deleted"
            );

            // …and the archive is invisible to the live set (`list` only reads `*.md` directly under
            // `self_dir`, and a directory carries no `.md` extension).
            let live = self_dir("aria");
            assert!(
                list("aria")
                    .iter()
                    .all(|m| m.path.parent() == Some(live.as_path())),
                "archived items must not come back through list()"
            );
        });
    }

    #[test]
    fn archive_never_overwrites_same_stem() {
        with_home("archive-collide", || {
            let dir = self_dir("aria");
            let adir = archive_dir("aria");
            fs::create_dir_all(&dir).unwrap();
            fs::create_dir_all(&adir).unwrap();
            let render = |body: &str| format!("---\nkind: episode\nimportance: 5\n---\n{body}");

            // An item archived on an earlier turn. Its stem is now FREE in the live dir, so
            // `unique_path` will happily hand it out again — which is how a naive rename
            // would silently overwrite this file later.
            fs::write(adir.join("ep-old.md"), render("first life")).unwrap();
            fs::write(dir.join("ep-old.md"), render("second life")).unwrap();

            prune_kind("aria", Kind::Episode, 0);

            let archived: Vec<String> = fs::read_dir(&adir)
                .unwrap()
                .flatten()
                .filter_map(|e| fs::read_to_string(e.path()).ok())
                .collect();
            assert_eq!(
                archived.len(),
                2,
                "collision uniquifies instead of overwriting"
            );
            assert!(
                archived.iter().any(|c| c.contains("first life")),
                "the older archive survives"
            );
            assert!(
                archived.iter().any(|c| c.contains("second life")),
                "the new eviction lands too"
            );
            assert_eq!(list("aria").len(), 0, "live set is empty at cap 0");
        });
    }

    /// The shredding defect, asserted on the real body shape. 45 of 89 files on a live machine were
    /// named like `in-t-i-n-n-l-9` because each accented letter tested false for
    /// `is_ascii_alphanumeric` and became a separator.
    #[test]
    fn stem_folds_vietnamese_into_whole_words() {
        with_home("stem-fold", || {
            let dir = self_dir("aria");
            fs::create_dir_all(&dir).unwrap();
            let p = unique_path(&dir, "in", "Người dùng giao tiếp bằng tiếng Việt");
            let stem = p.file_stem().unwrap().to_str().unwrap();
            assert!(stem.is_ascii(), "diacritics reached the filename: {stem}");
            assert!(
                stem.starts_with("in-nguoi-dung-giao-tiep-bang-tieng"),
                "expected folded whole words, got {stem}"
            );
            assert_eq!(
                stem.split('-').filter(|w| w.chars().count() == 1).count(),
                0,
                "still shredded into one-letter fragments: {stem}"
            );
        });
    }

    /// The collision defect. Twelve real episodes shared the stem
    /// `ep-correction-user-redirected-me-todo` because every body opens with the same type label and
    /// the same `[todo-poke]` scaffolding — the distinguishing content came later. Widening the word
    /// count would not help; the stem has to skip the boilerplate and carry a content hash.
    #[test]
    fn stems_differ_when_bodies_share_a_lead_in() {
        with_home("stem-collide", || {
            let dir = self_dir("aria");
            fs::create_dir_all(&dir).unwrap();
            let bodies = [
                "correction: user redirected me — \"[todo-poke] Session todos are still incomplete — you may not finish yet. Incomplete: [>] Hoàn thiện landing page\"",
                "correction: user redirected me — \"[todo-poke] Session todos are still incomplete — you may not finish yet. Incomplete: [>] Build and verify tsc\"",
                "correction: user redirected me — \"[todo-poke] Session todos are still incomplete — you may not finish yet. Incomplete: [>] Điều tra import graph\"",
            ];
            let stems: Vec<String> = bodies
                .iter()
                .map(|b| {
                    let p = unique_path(&dir, "ep", b);
                    // Write it, so the next call sees the collision the old code papered over.
                    fs::write(&p, "x").unwrap();
                    p.file_stem().unwrap().to_str().unwrap().to_string()
                })
                .collect();
            for s in &stems {
                assert!(
                    !s.contains("correction-user-redirected"),
                    "the type label still dominates the stem: {s}"
                );
                // The counter suffix is the last resort, not the normal disambiguator.
                assert!(
                    !s.ends_with("-2") && !s.ends_with("-3"),
                    "fell back to the collision counter: {s}"
                );
            }
            let unique: std::collections::HashSet<&String> = stems.iter().collect();
            assert_eq!(unique.len(), 3, "stems still collide: {stems:?}");
            // Each stem reaches its own distinguishing content.
            assert!(stems.iter().any(|s| s.contains("hoan-thien")), "{stems:?}");
            assert!(stems.iter().any(|s| s.contains("build")), "{stems:?}");
            assert!(stems.iter().any(|s| s.contains("dieu-tra")), "{stems:?}");
        });
    }

    /// The same body must always produce the same stem, or a re-run would write a second copy of one
    /// memory (and the near-dup gate is not a filename check).
    #[test]
    fn stem_is_stable_for_one_body() {
        with_home("stem-stable", || {
            let dir = self_dir("aria");
            fs::create_dir_all(&dir).unwrap();
            let a = unique_path(&dir, "in", "user prefers tabs over spaces");
            let b = unique_path(&dir, "in", "user prefers tabs over spaces");
            assert_eq!(a, b);
        });
    }

    /// A body with nothing slugabble still gets a usable, unique filename.
    #[test]
    fn stem_falls_back_when_nothing_survives() {
        with_home("stem-fallback", || {
            let dir = self_dir("aria");
            fs::create_dir_all(&dir).unwrap();
            let p = unique_path(&dir, "ep", "!!! ???");
            let stem = p.file_stem().unwrap().to_str().unwrap();
            assert!(stem.starts_with("ep-mem-"), "got {stem}");
        });
    }
}
