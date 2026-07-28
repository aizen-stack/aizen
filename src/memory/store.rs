//! The CLI-owned markdown memory store (source of truth) under `~/.nextgen/cli-memory/entries`.
//!
//! One fact per `*.md` file with frontmatter `name|description|type|created`.
//! P1 scope: load/list/add/get over plain markdown. Atomic-write + locking +
//! caps/eviction land in P4; this keeps the write path simple and correct first.

use crate::core::config;
use crate::memory::dimension::Dimension;
use crate::memory::frontmatter::{self, Frontmatter};
use crate::memory::path_scope::Tier;
use crate::memory::provenance::ProvenanceKind;
use crate::memory::tokenize::tokenize;
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryType {
    User,
    Feedback,
    Project,
    Reference,
}

impl MemoryType {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryType::User => "user",
            MemoryType::Feedback => "feedback",
            MemoryType::Project => "project",
            MemoryType::Reference => "reference",
        }
    }
    /// Unknown / missing → `reference` (adopted fallback).
    pub fn parse(s: &str) -> MemoryType {
        match s.trim().to_lowercase().as_str() {
            "user" => MemoryType::User,
            "feedback" => MemoryType::Feedback,
            "project" => MemoryType::Project,
            _ => MemoryType::Reference,
        }
    }

    /// Like [`MemoryType::parse`] but `None` on an unrecognized value, so a HUMAN's (or model's)
    /// typo is reported instead of silently filed as `reference`. The lenient `parse` stays the
    /// right choice for reading files off disk, where a legacy/unknown `type:` must not fail a load.
    pub fn parse_strict(s: &str) -> Option<MemoryType> {
        match s.trim().to_lowercase().as_str() {
            "user" => Some(MemoryType::User),
            "feedback" => Some(MemoryType::Feedback),
            "project" => Some(MemoryType::Project),
            "reference" => Some(MemoryType::Reference),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub id: String, // filename basename, lowercase, no extension — the stable id
    pub path: PathBuf,
    pub name: String,
    pub description: String,
    pub mtype: MemoryType,
    pub created: Option<String>,
    pub body: String,
    pub mtime_ms: u128,
    /// tokens over name + description + body (built on load).
    pub tokens: Vec<String>,
    // ── learned-fact metadata (P3) — default-trivial for hand-authored files ──
    /// Where the fact came from (trust axis). Missing → `manual` (a human wrote the file).
    pub source: ProvenanceKind,
    /// Free-extractor confidence at write time (0..1). Manual entries → 1.0.
    pub confidence: f64,
    /// Times this fact was re-observed (reinforcement). Drives P4 core-promotion.
    pub reinforced: u32,
    /// Distinct sessions that reinforced it. Drives P4 core-promotion (`≥2`).
    pub sessions: u32,
    /// Last session id that touched it (so reinforce only bumps `sessions` on a NEW session).
    // Provenance metadata: round-tripped to/from the `lastSession` frontmatter field but not read by
    // ranking/business logic (kept for audit + future session-scoped queries).
    #[allow(dead_code)]
    pub last_session: Option<String>,
    /// `YYYY-MM-DD` this fact was last RETRIEVED into context (implicit-reuse signal, P8).
    /// Drives the salience recency term; bumped at most once per day (`record_retrieval`).
    pub last_retrieved: Option<String>,
    /// `YYYY-MM-DD` of the last write/reinforce.
    pub updated: Option<String>,
    // ── bi-temporal supersession (P4) — never delete, mark superseded ──
    /// `YYYY-MM-DD` this fact stopped being true (None = still valid). `created` is `valid_from`.
    pub valid_to: Option<String>,
    /// Id of the fact that replaced this one (set together with `valid_to`).
    pub superseded_by: Option<String>,
    /// Set when the user EXPLICITLY denied promoting this fact to the always-on core (the CorePromote
    /// deny path). The fact stays searchable in the long tail but is excluded from `frozen_core::build`,
    /// so an explicit "no" is honored. Serialized as `noCore: true`; absent → false.
    pub core_denied: bool,
    /// Workspace scope: `None` = global (applies everywhere — also the parse of a legacy file with
    /// no `scope:` key, so the pre-scoping store keeps working untouched); `Some(slug)` = only the
    /// project zone `config::project_slug()` names. Filters the frozen core + default search.
    /// DEPRECATED in favor of `tier`/`anchor`/`device` — still read from disk for migration.
    pub scope: Option<String>,
    /// Optional region inside the project (`src/agent` style, `/`-normalized) — a soft ranking
    /// boost when the user works under it, never a hard partition.
    pub subpath: Option<String>,
    // ── tier/anchor/device (Phase 1) — replaces scope/subpath ──
    /// What the fact is about: user/device/place. Missing on legacy files → inferred on load.
    pub tier: Tier,
    /// Normalized absolute path anchor for `place` facts. Only set when `tier == Tier::Place`.
    /// Matching: segment-safe prefix (nearest ancestor wins).
    pub anchor: Option<String>,
    /// Stable device id for `device` facts. Only set when `tier == Tier::Device`.
    pub device: Option<String>,
    /// Reinforcement count, capped at 3 for aging purposes (replaces raw `reinforced` in decay).
    /// Seeded from `reinforced` on legacy files via `min(reinforced, 3)`.
    pub confirmations: u32,
    /// `YYYY-MM-DD` this fact was last explicitly confirmed/used.
    /// Seeded from `updated` → `created` on legacy files.
    pub last_used: Option<String>,
    /// Id of the fact this entry supersedes (set at write time, used by Phase 4 reconciliation).
    pub supersedes: Option<String>,
    /// Topical dimension (B1) — DERIVED on load by `dimension::classify`, not stored.
    pub dimension: Dimension,
    /// Content category (P3 CoALA typing) — DERIVED on load by `category::classify`, not stored.
    /// Orthogonal to `mtype` (scope) and `dimension` (user-profile facet).
    pub category: crate::memory::category::Category,
}

impl Default for MemoryEntry {
    fn default() -> Self {
        MemoryEntry {
            id: String::new(),
            path: PathBuf::new(),
            name: String::new(),
            description: String::new(),
            mtype: MemoryType::Reference,
            created: None,
            body: String::new(),
            mtime_ms: 0,
            tokens: Vec::new(),
            source: ProvenanceKind::Manual,
            confidence: 1.0,
            reinforced: 0,
            sessions: 0,
            last_session: None,
            last_retrieved: None,
            updated: None,
            valid_to: None,
            superseded_by: None,
            core_denied: false,
            scope: None,
            subpath: None,
            tier: Tier::Place,
            anchor: None,
            device: None,
            confirmations: 0,
            last_used: None,
            supersedes: None,
            dimension: Dimension::Other,
            category: crate::memory::category::Category::None,
        }
    }
}

impl MemoryEntry {
    fn from_file(path: &Path) -> Result<MemoryEntry> {
        let raw = fs::read_to_string(path)
            .with_context(|| format!("reading memory file {}", path.display()))?;
        let fm: Frontmatter = frontmatter::parse(&raw);
        let id = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_lowercase();
        let name = fm
            .get("name")
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| id.clone());
        let description = fm.get("description").unwrap_or("").trim().to_string();
        let mtype = MemoryType::parse(fm.get("type").unwrap_or(""));
        let created = fm
            .get("created")
            .or_else(|| fm.get("createdAt"))
            .map(str::to_string);
        let mtime_ms = fs::metadata(path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let tokens = tokenize(&format!("{name}\n{description}\n{}", fm.body));
        let classify_text = format!("{name} {description} {}", fm.body);
        let dimension = crate::memory::dimension::classify(&classify_text);
        let category = crate::memory::category::classify(&classify_text);
        // learned-fact meta (absent on hand-authored files → trust-as-manual defaults)
        let source = match fm.get("source") {
            Some(s) => ProvenanceKind::parse(s),
            None => ProvenanceKind::Manual,
        };
        let confidence = fm
            .get("confidence")
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(1.0)
            .clamp(0.0, 1.0);
        let reinforced = fm
            .get("reinforced")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let sessions = fm
            .get("sessions")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let last_session = fm
            .get("lastSession")
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty());
        let last_retrieved = fm
            .get("lastRetrieved")
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty());
        let updated = fm
            .get("updated")
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty());
        let valid_to = fm
            .get("validTo")
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty());
        let superseded_by = fm
            .get("supersededBy")
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty());
        let core_denied = fm
            .get("noCore")
            .map(|s| s.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false);
        let scope = parse_scope(fm.get("scope"));
        let subpath = fm
            .get("subpath")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.replace('\\', "/"));
        // ── tier/anchor/device (Phase 1) — legacy files get inferred ──
        let tier = match fm.get("tier").and_then(|s| Tier::parse_strict(s)) {
            Some(t) => t,
            None => infer_tier_from_legacy(&scope),
        };
        let anchor = fm
            .get("anchor")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase());
        let device = fm
            .get("device")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let confirmations: u32 = fm
            .get("confirmations")
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or_else(|| reinforced.min(3));
        // RAW: `None` when the key is absent, with NO fallback to `updated`/`created`.
        //
        // Falling back here made a fact written today read as "already used today", so
        // `confirm_use`'s once-per-day guard skipped it and it could never earn its first
        // confirmation — the M1 ladder's only input, permanently stuck at zero for same-day use.
        //
        // The fallback belongs where the question is "how long has this been idle?", and
        // `decay::idle_days` already asks it that way (`last_used` → `updated` → `created`).
        let last_used = fm
            .get("lastUsed")
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty());
        let supersedes = fm
            .get("supersedes")
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty());
        Ok(MemoryEntry {
            id,
            path: path.to_path_buf(),
            name,
            description,
            mtype,
            created,
            body: fm.body,
            mtime_ms,
            tokens,
            source,
            confidence,
            reinforced,
            sessions,
            last_session,
            last_retrieved,
            updated,
            valid_to,
            superseded_by,
            core_denied,
            scope,
            subpath,
            tier,
            anchor,
            device,
            confirmations,
            last_used,
            supersedes,
            dimension,
            category,
        })
    }
}

/// Frontmatter `scope:` → entry scope. Absent, empty, or the literal `global` all mean global
/// (`None`) — a legacy pre-scoping file therefore loads as global with zero migration.
fn parse_scope(raw: Option<&str>) -> Option<String> {
    let s = raw?.trim();
    if s.is_empty() || s.eq_ignore_ascii_case("global") {
        None
    } else {
        Some(s.to_string())
    }
}

/// Tier for a file written before `tier:` existed (no migration pass — inference on load).
///
/// A legacy `scope` slug is a HASH of a path, so it cannot be turned back into an anchor here:
/// the entry loads as `Place` with `anchor: None`, i.e. an **orphan** — never read (see
/// `Lineage::specificity`), never deleted, and listed by `aizen memory doctor`. Fail-closed:
/// a fact whose place we can no longer identify must not leak into the wrong tree.
///
/// Scope-absent means the fact applied everywhere, and the tier that means "applies everywhere" is
/// `User` — so it becomes core-eligible even if its `type:` is not `user` (the old always-on gate
/// also required `mtype == User`). That is a deliberate widening: placement, not the type tag, now
/// decides residency. It is bounded by the core's token cap and salience-greedy packing, so the
/// most-reused facts win the prefix and the rest spill to the search tail as before.
fn infer_tier_from_legacy(scope: &Option<String>) -> Tier {
    match scope {
        None => Tier::User,
        Some(_) => Tier::Place,
    }
}

impl MemoryEntry {
    /// A currently-valid (non-superseded) fact.
    pub fn is_active(&self) -> bool {
        self.valid_to.is_none() && self.superseded_by.is_none()
    }

    /// A short one-line gist for confirmations/listings: the description when there is one, else the
    /// head of the body flattened to a single line. Never empty-looking, so a "deleted X" message
    /// always says WHAT was deleted.
    pub fn description_or_body_head(&self) -> String {
        let src = if self.description.trim().is_empty() {
            self.body.trim()
        } else {
            self.description.trim()
        };
        let flat = src.split_whitespace().collect::<Vec<_>>().join(" ");
        let head: String = flat.chars().take(80).collect();
        if head.is_empty() {
            "(empty)".to_string()
        } else if flat.chars().count() > head.chars().count() {
            format!("{head}…")
        } else {
            head
        }
    }
}

/// Load every entry from the long-tail store. Missing dir → empty (never errors).
pub fn load_all() -> Result<Vec<MemoryEntry>> {
    load_from(&config::entries_dir())
}

/// Load every `*.md` entry under `dir` (entries dir, review queue, archive). Missing → empty.
pub fn load_from(dir: &Path) -> Result<Vec<MemoryEntry>> {
    let mut out = Vec::new();
    let rd = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return Ok(out), // not created yet
    };
    for ent in rd.flatten() {
        let path = ent.path();
        let is_md = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("md"))
            .unwrap_or(false);
        if !is_md {
            continue;
        }
        match MemoryEntry::from_file(&path) {
            Ok(e) => out.push(e),
            // Through the TUI funnel, never `eprintln!`: retrieval calls this on EVERY turn, so a raw
            // print writes into the terminal behind the retained renderer's back and corrupts the
            // frame (see `ui::tui::note_line`). One unreadable file would otherwise garble the UI
            // once per turn, forever.
            Err(e) => crate::ui::tui::note_line(&format!(
                "[warn] skipping unreadable memory {}: {e}",
                path.display()
            )),
        }
    }
    Ok(out)
}

/// Slugify a name into a safe filename stem.
pub fn slugify(name: &str) -> String {
    let mut s = String::new();
    let mut prev_dash = false;
    for c in name.trim().to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c);
            prev_dash = false;
        } else if !prev_dash && !s.is_empty() {
            s.push('-');
            prev_dash = true;
        }
    }
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "memory".to_string()
    } else {
        s
    }
}

/// Add a new entry; returns the id (filename stem). Errors if id collides
/// (use `edit` to update an existing one — "check-existing-then-UPDATE" discipline).
pub fn add(name: &str, description: &str, mtype: MemoryType, body: &str) -> Result<String> {
    add_scoped(name, description, mtype, body, None)
}

/// `add` with a workspace scope (`None` = global). Separate entry point so every existing caller
/// keeps its signature; scoping callers (`#remember`, the learning router) opt in explicitly.
///
/// Writes the tier axis EXPLICITLY rather than leaving it to `infer_tier_from_legacy`. A file this
/// build just wrote must never need guessing on the way back in — inference exists for files written
/// by older builds, and letting a fresh write fall through it means the guess becomes the contract.
pub fn add_scoped(
    name: &str,
    description: &str,
    mtype: MemoryType,
    body: &str,
    scope: Option<&str>,
) -> Result<String> {
    let dir = config::entries_dir();
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let id = slugify(name);
    let path = dir.join(format!("{id}.md"));
    if path.exists() {
        anyhow::bail!(
            "a memory '{id}' already exists ({}); edit it instead of adding a duplicate",
            path.display()
        );
    }
    let mut fields = BTreeMap::new();
    fields.insert("name".to_string(), name.trim().to_string());
    if !description.trim().is_empty() {
        fields.insert("description".to_string(), description.trim().to_string());
    }
    fields.insert("type".to_string(), mtype.as_str().to_string());
    fields.insert("created".to_string(), today());
    let zone = scope
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("global"));
    if let Some(s) = zone {
        fields.insert("scope".to_string(), s.to_string());
    }
    let (tier, anchor) = scoped_tier_choice(zone);
    fields.insert("tier".to_string(), tier.as_str().to_string());
    if let Some(a) = anchor {
        fields.insert("anchor".to_string(), a);
    }
    let content = frontmatter::serialize(&fields, body, LEARNED_KEY_ORDER);
    write_atomic(&path, &content)?;
    Ok(id)
}

/// Placement for a hand-authored / tool-driven `add_scoped`, derived from the `scope` argument
/// alone so that it agrees with [`infer_tier_from_legacy`] **exactly**.
///
/// That agreement is the point: re-reading one file must yield the same tier whether or not it
/// carries a `tier:` key. If this function decided placement from `mtype` while inference decided
/// it from `scope`, a fact written by this build and the identical fact written by the previous one
/// would live in different partitions — invisible to each other, so neither could ever supersede
/// or dedup the other.
///
/// - no zone → the fact applied everywhere, which is `Tier::User`.
/// - the CURRENT zone → a place fact, anchored where that zone actually is.
/// - some OTHER zone → an **orphan** `Place`. The slug is a hash of a path, so the directory it
///   stood for cannot be recovered; the fact keeps its text and its legacy `scope:` for the
///   explicit `--scope`/all views, but no lineage will admit it. Fail-closed beats anchoring it at
///   whatever directory the caller happened to be standing in.
fn scoped_tier_choice(zone: Option<&str>) -> (Tier, Option<String>) {
    match zone {
        None => (Tier::User, None),
        Some(z) if z == config::project_slug() => {
            let lin = crate::memory::path_scope::Lineage::current();
            (Tier::Place, Some(lin.narrowest_project_or_cwd()))
        }
        Some(_) => (Tier::Place, None),
    }
}

/// Today's date, `YYYY-MM-DD`.
fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

/// A fact the learning pipeline wants to persist (P3).
pub struct LearnedWrite<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub mtype: MemoryType,
    pub body: &'a str,
    pub source: ProvenanceKind,
    pub confidence: f64,
    pub session_id: &'a str,
    /// Exclude this fact from the always-on frozen core (set on the CorePromote deny-downgrade).
    pub no_core: bool,
    /// Workspace scope (`None` = global; `Some(slug)` = one project zone). LEGACY — prefer `tier`.
    pub scope: Option<String>,
    /// Optional region inside the project (soft ranking boost).
    pub subpath: Option<String>,
    /// Which axis this fact lives on. `Tier::User` = everywhere, `Device` = this machine,
    /// `Place` = under `anchor`.
    pub tier: Tier,
    /// Normalized absolute path (lowercased) for `Tier::Place`.
    pub anchor: Option<String>,
    /// Stable device id for `Tier::Device`.
    pub device: Option<String>,
    /// Id of a fact this one replaces (written in the SAME write — no journal needed).
    pub supersedes: Option<String>,
}

/// **Tests only, deliberately.** Production call sites must name every field, so that adding a
/// field to [`LearnedWrite`] is a compile error at each write path instead of a silent default —
/// that error is what caught all four call sites when the tier axis landed. Test fixtures care
/// about two or three fields and would otherwise restate a dozen; they opt into the default with
/// `..Default::default()`. The default is `Tier::User` / no anchor, i.e. an ordinary global fact.
#[cfg(test)]
impl<'a> Default for LearnedWrite<'a> {
    fn default() -> Self {
        LearnedWrite {
            name: "",
            description: "",
            mtype: MemoryType::Reference,
            body: "",
            source: ProvenanceKind::Inferred,
            confidence: 0.8,
            session_id: "s",
            no_core: false,
            scope: None,
            subpath: None,
            tier: Tier::User,
            anchor: None,
            device: None,
            supersedes: None,
        }
    }
}

const LEARNED_KEY_ORDER: &[&str] = &[
    "name",
    "description",
    "type",
    "tier",
    "anchor",
    "device",
    "scope",
    "subpath",
    "source",
    "confidence",
    "created",
    "updated",
    "reinforced",
    "confirmations",
    "sessions",
    "lastSession",
    "lastRetrieved",
    "lastUsed",
    "validTo",
    "supersededBy",
    "supersedes",
    "noCore",
];

/// Pick a free filename stem for `name`, appending `-2`, `-3`, … on collision.
fn unique_id(dir: &Path, name: &str) -> String {
    let base = slugify(name);
    if !dir.join(format!("{base}.md")).exists() {
        return base;
    }
    for n in 2..10_000 {
        let cand = format!("{base}-{n}");
        if !dir.join(format!("{cand}.md")).exists() {
            return cand;
        }
    }
    base // pathological fallback (will overwrite) — practically unreachable
}

/// All persisted fields of a learned-style fact. Named fields (not a positional arg list) so
/// adding a metadata field — e.g. `last_retrieved` (P8) — is a one-line, miscount-proof change.
///
/// **Every field here must have a key in [`LEARNED_KEY_ORDER`]** — the arithmetic test
/// `render_learned_covers_every_key_in_order` compares the two counts, so the next person to add a
/// field and forget the key gets a red test instead of a silently-dropped column.
struct LearnedRecord<'a> {
    name: &'a str,
    description: &'a str,
    mtype: MemoryType,
    body: &'a str,
    source: ProvenanceKind,
    confidence: f64,
    created: &'a str,
    updated: &'a str,
    reinforced: u32,
    sessions: u32,
    last_session: &'a str,
    last_retrieved: &'a str,
    valid_to: &'a str,
    superseded_by: &'a str,
    no_core: bool,
    scope: &'a str,
    subpath: &'a str,
    // ── tier/anchor axis (Phase 1) ──
    tier: Tier,
    anchor: &'a str,
    device: &'a str,
    confirmations: u32,
    last_used: &'a str,
    supersedes: &'a str,
}

fn render_learned(r: &LearnedRecord) -> String {
    let mut fields = BTreeMap::new();
    fields.insert("name".to_string(), r.name.trim().to_string());
    if !r.description.trim().is_empty() {
        fields.insert("description".to_string(), r.description.trim().to_string());
    }
    fields.insert("type".to_string(), r.mtype.as_str().to_string());
    fields.insert("source".to_string(), r.source.as_str().to_string());
    fields.insert(
        "confidence".to_string(),
        format!("{:.2}", r.confidence.clamp(0.0, 1.0)),
    );
    fields.insert("created".to_string(), r.created.to_string());
    fields.insert("updated".to_string(), r.updated.to_string());
    fields.insert("reinforced".to_string(), r.reinforced.to_string());
    fields.insert("sessions".to_string(), r.sessions.to_string());
    if !r.last_session.trim().is_empty() {
        fields.insert("lastSession".to_string(), r.last_session.trim().to_string());
    }
    if !r.last_retrieved.trim().is_empty() {
        fields.insert(
            "lastRetrieved".to_string(),
            r.last_retrieved.trim().to_string(),
        );
    }
    if !r.valid_to.trim().is_empty() {
        fields.insert("validTo".to_string(), r.valid_to.trim().to_string());
    }
    if !r.superseded_by.trim().is_empty() {
        fields.insert(
            "supersededBy".to_string(),
            r.superseded_by.trim().to_string(),
        );
    }
    if r.no_core {
        fields.insert("noCore".to_string(), "true".to_string());
    }
    if !r.scope.trim().is_empty() {
        fields.insert("scope".to_string(), r.scope.trim().to_string());
    }
    if !r.subpath.trim().is_empty() {
        fields.insert("subpath".to_string(), r.subpath.trim().to_string());
    }
    // tier is ALWAYS written (it is the read filter — an absent tier means "guess", and guessing
    // is what the legacy inference path is for; a fact we just wrote must never need guessing).
    fields.insert("tier".to_string(), r.tier.as_str().to_string());
    if !r.anchor.trim().is_empty() {
        fields.insert("anchor".to_string(), r.anchor.trim().to_ascii_lowercase());
    }
    if !r.device.trim().is_empty() {
        fields.insert("device".to_string(), r.device.trim().to_string());
    }
    fields.insert("confirmations".to_string(), r.confirmations.to_string());
    if !r.last_used.trim().is_empty() {
        fields.insert("lastUsed".to_string(), r.last_used.trim().to_string());
    }
    if !r.supersedes.trim().is_empty() {
        fields.insert("supersedes".to_string(), r.supersedes.trim().to_string());
    }
    frontmatter::serialize(&fields, r.body, LEARNED_KEY_ORDER)
}

/// Persist a freshly-extracted fact to the live entry store (auto-uniquifies the slug).
pub fn add_learned(w: &LearnedWrite) -> Result<String> {
    add_learned_in(&config::entries_dir(), w)
}

/// Persist a learned fact under `dir` (entries dir for live, review dir for the human gate).
/// Starts at `reinforced=1, sessions=1` — its first observation in this session.
pub fn add_learned_in(dir: &Path, w: &LearnedWrite) -> Result<String> {
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let id = unique_id(dir, w.name);
    let path = dir.join(format!("{id}.md"));
    let now = today();
    let content = render_learned(&LearnedRecord {
        name: w.name,
        description: w.description,
        mtype: w.mtype,
        body: w.body,
        source: w.source,
        confidence: w.confidence,
        created: &now,
        updated: &now,
        reinforced: 1,
        sessions: 1,
        last_session: w.session_id,
        last_retrieved: "",
        valid_to: "",
        superseded_by: "",
        no_core: w.no_core,
        scope: w.scope.as_deref().unwrap_or(""),
        subpath: w.subpath.as_deref().unwrap_or(""),
        tier: w.tier,
        anchor: w.anchor.as_deref().unwrap_or(""),
        device: w.device.as_deref().unwrap_or(""),
        // Birth is NOT a confirmation, and it is not a use.
        //
        // `confirmations` is the M1 ladder's only input, i.e. "how many times did this fact prove
        // useful". Being written is the claim, not evidence for it — starting at 1 would put every
        // fact on the second rung (a 3x half-life) for free.
        //
        // `lastUsed` empty for a sharper reason: `confirm_use` is once-per-day, so stamping today at
        // birth means a fact written and then genuinely used in the same session can never earn its
        // FIRST confirmation. The idle clock falls back to `updated`/`created` (both today), so
        // leaving it empty costs nothing in ranking.
        confirmations: 0,
        last_used: "",
        supersedes: w.supersedes.as_deref().unwrap_or(""),
    });
    write_atomic(&path, &content)?;
    Ok(id)
}

/// Read the tier/anchor axis back off a re-parsed file for the bookkeeping writers
/// (`reinforce`, `mark_superseded`) that re-render a fixed record shape.
///
/// Both of those re-read the file and rebuild every field by hand, so ANY field they forget is
/// silently dropped — that is how `scope`/`subpath` were nearly lost, and the axis is worse: a
/// dropped `tier` turns a place fact into one that applies everywhere. Centralized here so the two
/// callers cannot drift, preferring the on-disk value and falling back to the loaded entry.
fn carry_axis(
    fm: &Frontmatter,
    entry: &MemoryEntry,
) -> (Tier, String, String, u32, String, String) {
    let tier = fm
        .get("tier")
        .and_then(Tier::parse_strict)
        .unwrap_or(entry.tier);
    let anchor = fm
        .get("anchor")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| entry.anchor.clone())
        .unwrap_or_default();
    let device = fm
        .get("device")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| entry.device.clone())
        .unwrap_or_default();
    let confirmations = fm
        .get("confirmations")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(entry.confirmations);
    let last_used = fm
        .get("lastUsed")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| entry.last_used.clone())
        .unwrap_or_default();
    let supersedes = fm
        .get("supersedes")
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| entry.supersedes.clone())
        .unwrap_or_default();
    (tier, anchor, device, confirmations, last_used, supersedes)
}

/// Reinforce an existing learned fact: bump `reinforced`, and bump `sessions` only when
/// `session_id` differs from the last one that touched it. Re-reads the file so concurrent
/// edits / extra fields are not clobbered. Returns the new `reinforced` count.
pub fn reinforce(entry: &MemoryEntry, session_id: &str) -> Result<u32> {
    let raw = fs::read_to_string(&entry.path)
        .with_context(|| format!("reading {} to reinforce", entry.path.display()))?;
    let fm = frontmatter::parse(&raw);

    let cur_reinforced: u32 = fm
        .get("reinforced")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let cur_sessions: u32 = fm
        .get("sessions")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    let last = fm.get("lastSession").unwrap_or("").trim().to_string();

    let reinforced = cur_reinforced.saturating_add(1);
    let sessions = if last == session_id {
        cur_sessions.max(1)
    } else {
        cur_sessions.saturating_add(1).max(1)
    };

    let name = fm
        .get("name")
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(&entry.name);
    let description = fm.get("description").unwrap_or(&entry.description);
    let mtype = MemoryType::parse(fm.get("type").unwrap_or(entry.mtype.as_str()));
    let source = ProvenanceKind::parse(fm.get("source").unwrap_or(entry.source.as_str()));
    let confidence = fm
        .get("confidence")
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(entry.confidence);
    let created = fm
        .get("created")
        .map(str::to_string)
        .or_else(|| entry.created.clone())
        .unwrap_or_else(today);

    let valid_to = fm.get("validTo").unwrap_or("").to_string();
    let superseded_by = fm.get("supersededBy").unwrap_or("").to_string();
    let last_retrieved = fm.get("lastRetrieved").unwrap_or("").to_string();
    let no_core = fm
        .get("noCore")
        .map(|s| s.trim().eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    // scope/subpath survive reinforcement verbatim — a reinforce that dropped them would silently
    // promote a project fact to global.
    let scope = fm.get("scope").unwrap_or("").to_string();
    let subpath = fm.get("subpath").unwrap_or("").to_string();
    // tier/anchor/device survive reinforcement verbatim, for the same reason scope does: a
    // reinforce that dropped them would silently promote a place fact to "applies everywhere".
    let (tier, anchor, device, confirmations, last_used, supersedes) = carry_axis(&fm, entry);
    let content = render_learned(&LearnedRecord {
        name,
        description,
        mtype,
        body: &fm.body,
        source,
        confidence,
        created: &created,
        updated: &today(),
        reinforced,
        sessions,
        last_session: session_id,
        last_retrieved: &last_retrieved,
        valid_to: &valid_to,
        superseded_by: &superseded_by,
        no_core, // preserve an explicit deny across reinforcement
        scope: &scope,
        subpath: &subpath,
        tier,
        anchor: &anchor,
        device: &device,
        confirmations,
        last_used: &last_used,
        supersedes: &supersedes,
    });
    write_atomic(&entry.path, &content)?;
    Ok(reinforced)
}

/// Bi-temporal supersession: mark `entry` as no-longer-valid (`validTo`=today, `supersededBy`=
/// `by_id`) WITHOUT deleting it — the history stays queryable via `ng memory as-of`.
///
/// Edits the parsed FIELD MAP, like [`update`] and [`unsupersede`]: an earlier version re-rendered
/// a fixed record shape here, which silently dropped every frontmatter key this build doesn't model
/// — so retiring a fact quietly destroyed part of it, on the one path whose entire promise is that
/// nothing is destroyed. Retiring also must not RELOCATE a fact: `tier`/`anchor` are left exactly as
/// they are so the history view shows the fact as it actually was.
pub fn mark_superseded(entry: &MemoryEntry, by_id: &str) -> Result<()> {
    let raw = fs::read_to_string(&entry.path)
        .with_context(|| format!("reading {} to supersede", entry.path.display()))?;
    let fm = frontmatter::parse(&raw);
    let mut fields = fm.fields.clone();
    fields.insert("validTo".to_string(), today());
    fields.insert("supersededBy".to_string(), by_id.trim().to_string());
    fields.insert("updated".to_string(), today());
    // A well-formed entry always carries a type; a fence-less hand-authored file may not.
    fields
        .entry("type".to_string())
        .or_insert_with(|| entry.mtype.as_str().to_string());
    fields.entry("created".to_string()).or_insert_with(today);
    let content = frontmatter::serialize(&fields, &fm.body, LEARNED_KEY_ORDER);
    write_atomic(&entry.path, &content)?;
    Ok(())
}

/// Undo a supersession: clear `validTo` + `supersededBy` so the fact returns to the `active()`
/// view. The inverse of [`mark_superseded`], and the reason an automatic `contradict` branch is
/// allowed to exist at all — without a way back, one wrong reconciliation is indistinguishable
/// from data loss (the bytes survive, but nothing can ever surface them again).
///
/// Edits the parsed FIELD MAP rather than re-rendering a fixed record shape, so every key this
/// build doesn't know about survives verbatim — the same reason [`update`] works that way.
/// `updated` is deliberately NOT stamped: reviving a fact is a correction of bookkeeping, not a
/// fresh use of the fact, and stamping it would hand the revived row a false recency.
pub fn unsupersede(entry: &MemoryEntry) -> Result<()> {
    let raw = fs::read_to_string(&entry.path)
        .with_context(|| format!("reading {} to revive", entry.path.display()))?;
    let fm = frontmatter::parse(&raw);
    let mut fields = fm.fields.clone();
    let was_retired = fields.remove("validTo").is_some() | fields.remove("supersededBy").is_some();
    if !was_retired {
        anyhow::bail!("'{}' is not superseded (nothing to revive)", entry.id);
    }
    fields
        .entry("type".to_string())
        .or_insert_with(|| entry.mtype.as_str().to_string());
    let content = frontmatter::serialize(&fields, &fm.body, LEARNED_KEY_ORDER);
    write_atomic(&entry.path, &content)?;
    Ok(())
}

/// Drop a `supersedes: <old_id>` claim from whichever live fact carries it. The other half of a
/// revive: `active()` also hides a fact that some LIVE fact claims to supersede, so clearing only
/// the retired side would leave it hidden by the survivor's forward pointer. Returns the ids of
/// the facts edited (normally 0 or 1). Best-effort per file — a single unreadable row is skipped
/// rather than failing the whole revive.
pub fn clear_supersedes_claims(entries: &[MemoryEntry], old_id: &str) -> Result<Vec<String>> {
    let mut cleared = Vec::new();
    for e in entries
        .iter()
        .filter(|e| e.supersedes.as_deref() == Some(old_id))
    {
        let Ok(raw) = fs::read_to_string(&e.path) else {
            continue;
        };
        let fm = frontmatter::parse(&raw);
        let mut fields = fm.fields.clone();
        if fields.remove("supersedes").is_none() {
            continue;
        }
        fields
            .entry("type".to_string())
            .or_insert_with(|| e.mtype.as_str().to_string());
        let content = frontmatter::serialize(&fields, &fm.body, LEARNED_KEY_ORDER);
        if write_atomic(&e.path, &content).is_ok() {
            cleared.push(e.id.clone());
        }
    }
    Ok(cleared)
}

/// Implicit-reuse reinforcement (the P8 evolution spine): record that `entry` was retrieved
/// into context today. Bumps `reinforced` and stamps `lastRetrieved`/`updated`, **at most once
/// per day per fact** (`lastRetrieved == today` → no-op) so a session firing many searches can't
/// inflate the count. Re-reads the file and PRESERVES every existing field (incl. unknown ones).
/// Returns `Ok(true)` if it wrote, `Ok(false)` if already counted today. Best-effort by design —
/// callers ignore the error so a read-only store never breaks retrieval.
pub fn record_retrieval(entry: &MemoryEntry, today: &str) -> Result<bool> {
    // Fast path: the already-loaded entry shows it was retrieved today → skip ALL file I/O. A
    // session that fires many memory_search calls thus does zero read+write churn for facts already
    // counted today (the common case), instead of an O(hits) read+rename cycle every search.
    if entry.last_retrieved.as_deref().map(str::trim) == Some(today) {
        return Ok(false);
    }
    let raw = fs::read_to_string(&entry.path)
        .with_context(|| format!("reading {} to record reuse", entry.path.display()))?;
    let fm = frontmatter::parse(&raw);
    if fm.get("lastRetrieved").map(str::trim) == Some(today) {
        return Ok(false); // already counted today (re-checked on disk: another process may have stamped it)
    }
    let mut fields = fm.fields.clone();
    let cur: u32 = fm
        .get("reinforced")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    fields.insert("reinforced".to_string(), cur.saturating_add(1).to_string());
    fields.insert("lastRetrieved".to_string(), today.to_string());
    fields.insert("updated".to_string(), today.to_string());
    // type is required for a well-formed entry; a fence-less manual file may lack it.
    fields
        .entry("type".to_string())
        .or_insert_with(|| entry.mtype.as_str().to_string());
    let content = frontmatter::serialize(&fields, &fm.body, LEARNED_KEY_ORDER);
    write_atomic(&entry.path, &content)?;
    Ok(true)
}

/// Record that a fact **actually helped** on this turn: `confirmations += 1`, `lastUsed = today`.
///
/// This is the ONLY input to the M1 half-life ladder ([`crate::memory::bloat::decay`]), and the
/// distinction from [`record_retrieval`] is the whole point: retrieval means the fact was *offered*
/// to the model, which the model does not choose and which therefore measures exposure. A
/// confirmation means the model reported it as load-bearing for the answer it just gave.
///
/// Once per day per fact, like `record_retrieval`: a turn that leans on the same fact twice is one
/// day's worth of evidence, not two.
///
/// Goes through the FIELD MAP (`fm.fields` + `serialize`), not `render_learned`: this is a
/// bookkeeping write, so any frontmatter key this build does not know about has to survive it. The
/// fixed-record writers (`reinforce`, `mark_superseded`) are the pattern to avoid here.
///
/// Best-effort by design — the caller ignores the error, so a read-only store cannot break a turn.
pub fn confirm_use(entry: &MemoryEntry, today: &str) -> Result<bool> {
    if entry.last_used.as_deref().map(str::trim) == Some(today) {
        return Ok(false); // already counted today (cheap path: no file I/O at all)
    }
    let raw = fs::read_to_string(&entry.path)
        .with_context(|| format!("reading {} to confirm use", entry.path.display()))?;
    let fm = frontmatter::parse(&raw);
    if fm.get("lastUsed").map(str::trim) == Some(today) {
        return Ok(false); // another process stamped it since we loaded
    }
    let mut fields = fm.fields.clone();
    let cur: u32 = fm
        .get("confirmations")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    fields.insert(
        "confirmations".to_string(),
        cur.saturating_add(1).to_string(),
    );
    fields.insert("lastUsed".to_string(), today.to_string());
    // `updated` is deliberately NOT stamped: it is the aging clock the archive sweep and ranking
    // read, and a confirmation already refreshes the clock through `lastUsed`. Stamping both would
    // let one confirmation reset the fact's age twice over.
    fields
        .entry("type".to_string())
        .or_insert_with(|| entry.mtype.as_str().to_string());
    // A well-formed entry always carries its tier; a legacy file may not, and this write must not be
    // the thing that turns an inferred tier into an absent one on the way back out.
    fields
        .entry("tier".to_string())
        .or_insert_with(|| entry.tier.as_str().to_string());
    let content = frontmatter::serialize(&fields, &fm.body, LEARNED_KEY_ORDER);
    write_atomic(&entry.path, &content)?;
    Ok(true)
}

/// A partial update to an existing entry: `None` = leave that field exactly as it is on disk.
/// Every field the patch does not name — including frontmatter keys this build doesn't know about —
/// survives verbatim, because [`update`] edits the parsed field map instead of re-rendering a
/// fixed record shape (the mistake `reinforce`/`mark_superseded` make, where each new metadata
/// field has to be threaded through by hand or it is silently dropped).
#[derive(Debug, Default, Clone)]
pub struct EntryPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub mtype: Option<MemoryType>,
    pub body: Option<String>,
    /// `Some(None)` = make it global; `Some(Some(slug))` = move to that zone; `None` = leave alone.
    pub scope: Option<Option<String>>,
    /// Keep `updated:` exactly as it is on disk instead of stamping today. For BOOKKEEPING-only
    /// patches (zone migration retag): `updated` is the store's aging clock (decay, the inferred
    /// LRU cap, recency ranking), and a mass retag that stamped it would make every migrated
    /// fact look freshly touched — evicting the user's genuinely-active facts first.
    pub preserve_updated: bool,
    /// Drop this fact's `supersedes:` claim, releasing whatever it was hiding. The forward half of
    /// a revive, reachable as an ordinary patch so `memory edit` can undo a bad claim without
    /// needing the full `cmd_revive` path.
    pub clear_supersede: bool,
}

impl EntryPatch {
    /// Does this patch actually change anything?
    ///
    /// `clear_supersede` counts: a patch that only drops a claim still rewrites the file, and
    /// omitting it here would make that patch bail with "nothing to update".
    pub fn is_empty(&self) -> bool {
        self.name.is_none()
            && self.description.is_none()
            && self.mtype.is_none()
            && self.body.is_none()
            && self.scope.is_none()
            && !self.clear_supersede
    }
}

/// Apply a partial update to an existing entry in place, preserving every field the patch does not
/// name, and stamping `updated` = today. Re-reads the file first so a concurrent reinforce isn't
/// clobbered. The file NAME (and therefore the id) never changes — renaming would break
/// `supersededBy` pointers and the co-retrieval graph, so `name:` is display-only here.
pub fn update(entry: &MemoryEntry, patch: &EntryPatch) -> Result<()> {
    if patch.is_empty() {
        anyhow::bail!("nothing to update (pass at least one field)");
    }
    let raw = fs::read_to_string(&entry.path)
        .with_context(|| format!("reading {} to update", entry.path.display()))?;
    let fm = frontmatter::parse(&raw);
    let mut fields = fm.fields.clone();
    if let Some(n) = patch
        .name
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        fields.insert("name".to_string(), n.to_string());
    }
    if let Some(d) = patch.description.as_ref() {
        let d = d.trim();
        if d.is_empty() {
            fields.remove("description");
        } else {
            fields.insert("description".to_string(), d.to_string());
        }
    }
    if let Some(t) = patch.mtype {
        fields.insert("type".to_string(), t.as_str().to_string());
    }
    if let Some(s) = patch.scope.as_ref() {
        match s.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            Some(slug) => {
                fields.insert("scope".to_string(), slug.to_string());
            }
            None => {
                fields.remove("scope");
            }
        }
    }
    if patch.clear_supersede {
        fields.remove("supersedes");
    }
    // A well-formed entry always carries a type; a fence-less hand-authored file may not.
    fields
        .entry("type".to_string())
        .or_insert_with(|| entry.mtype.as_str().to_string());
    fields.entry("created".to_string()).or_insert_with(today);
    if !patch.preserve_updated {
        fields.insert("updated".to_string(), today());
    }
    let body = patch.body.as_deref().unwrap_or(&fm.body);
    let content = frontmatter::serialize(&fields, body, LEARNED_KEY_ORDER);
    write_atomic(&entry.path, &content)?;
    Ok(())
}

/// Rewrite a fact's body in place, **keeping its id**, and keep the previous text as a revision
/// snapshot at `archive/<id>-r<N>.md`.
///
/// This is the write half of the batch pass's `refine` verdict, and every part of the shape is load
/// bearing:
///
/// - **The id survives.** A refine is the same claim stated better, so `supersededBy`, `supersedes`,
///   and every co-retrieval edge naming this fact must keep resolving. Writing a NEW fact and
///   retiring the old one would be the supersede path — correct for a contradiction, wrong here,
///   because it splits one fact's history across two ids and orphans its edges.
/// - **`confirmations` drops to `min(c, 1)`.** The count measures agreement with the words that were
///   there; new words have not earned the old count. Zeroing it would be too harsh (the fact itself
///   was never in dispute), so one confirmation is kept as the floor.
/// - **The old body is copied, not discarded.** A refine is the one automatic path that overwrites
///   user-visible text, so the previous wording has to remain readable — `<id>-r1`, `<id>-r2`, … in
///   revision order.
///
/// Returns the revision id the old text was parked under.
pub fn refine_in_place(entry: &MemoryEntry, new_body: &str, today: &str) -> Result<String> {
    let new_body = new_body.trim();
    if new_body.is_empty() {
        anyhow::bail!("refusing to refine '{}' to an empty body", entry.id);
    }
    let raw = fs::read_to_string(&entry.path)
        .with_context(|| format!("reading {} to refine", entry.path.display()))?;
    let fm = frontmatter::parse(&raw);

    // Park the OLD text first: if this fails we have not touched the live fact yet, so the refine
    // simply does not happen. The reverse order could lose the previous wording outright.
    let adir = config::archive_dir();
    fs::create_dir_all(&adir).with_context(|| format!("creating {}", adir.display()))?;
    let (rev_path, rev_id) = next_revision(&adir, &entry.id);
    fs::write(&rev_path, raw.as_bytes())
        .with_context(|| format!("writing revision {}", rev_path.display()))?;

    let mut fields = fm.fields.clone();
    let cur: u32 = fm
        .get("confirmations")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0);
    fields.insert("confirmations".to_string(), cur.min(1).to_string());
    fields.insert("lastUsed".to_string(), today.to_string());
    fields.insert("updated".to_string(), today.to_string());
    fields
        .entry("type".to_string())
        .or_insert_with(|| entry.mtype.as_str().to_string());
    fields
        .entry("created".to_string())
        .or_insert_with(|| today.to_string());
    fields
        .entry("tier".to_string())
        .or_insert_with(|| entry.tier.as_str().to_string());
    let content = frontmatter::serialize(&fields, new_body, LEARNED_KEY_ORDER);
    write_atomic(&entry.path, &content)?;
    Ok(rev_id)
}

/// The next free `<id>-r<N>.md` under `dir`, as `(path, id)`. Numbered from 1 and never reused, so
/// the revision files read in the order the refinements happened.
fn next_revision(dir: &Path, id: &str) -> (std::path::PathBuf, String) {
    for n in 1..10_000 {
        let rev = format!("{id}-r{n}");
        let p = dir.join(format!("{rev}.md"));
        if !p.exists() {
            return (p, rev);
        }
    }
    let rev = format!("{id}-r-overflow");
    (dir.join(format!("{rev}.md")), rev)
}

/// Soft-delete: move the entry into the recoverable archive (`aizen memory restore <id>` brings it
/// back). This is the ONLY delete exposed to the agent — the store's whole design is
/// "never lose a fact", so a model deciding something is obsolete must not be able to destroy it.
/// Returns the archived id.
pub fn retire(entry: &MemoryEntry) -> Result<String> {
    crate::memory::bloat::caps::archive_entry(entry)
}

/// Hard-delete an ARCHIVED entry's file. Human-only (the CLI's `memory purge`); irreversible.
pub fn purge_archived(id: &str) -> Result<()> {
    let path = config::archive_dir().join(format!("{}.md", id.to_lowercase()));
    if !path.exists() {
        anyhow::bail!("no archived memory '{id}' ({})", path.display());
    }
    fs::remove_file(&path).with_context(|| format!("deleting {}", path.display()))?;
    Ok(())
}

/// Write atomically: temp file + rename. The temp name is unique per (process, write) so two `ng`
/// processes (or threads) writing the same entry never collide on a shared `.entry.md.tmp` and
/// clobber each other's rename. (P4 adds advisory locking + drift check.)
pub(crate) fn write_atomic(path: &Path, content: &str) -> Result<()> {
    let key = path.to_string_lossy();
    let lock_path = crate::core::workspace_txn::store_lock("memory_entry", &key);
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        std::time::Duration::from_secs(5),
    )?;
    crate::core::persist::atomic_write(path, content.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugify_basic() {
        assert_eq!(slugify("Auth Strategy!"), "auth-strategy");
        assert_eq!(slugify("  pnpm   over npm  "), "pnpm-over-npm");
        assert_eq!(slugify("***"), "memory");
    }

    #[test]
    fn type_parse_fallback() {
        assert_eq!(MemoryType::parse("user"), MemoryType::User);
        assert_eq!(MemoryType::parse("nonsense"), MemoryType::Reference);
        assert_eq!(MemoryType::parse(""), MemoryType::Reference);
    }

    #[test]
    fn scope_round_trips_and_survives_reinforce_and_supersede() {
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-scope-rt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);

        let w = LearnedWrite {
            name: "zone fact",
            description: "",
            mtype: MemoryType::Project,
            body: "the service deploys from ci",
            source: crate::memory::provenance::ProvenanceKind::Inferred,
            confidence: 0.8,
            session_id: "s1",
            no_core: false,
            scope: Some("myproj-0a1b2c3d".into()),
            subpath: Some("src/agent".into()),
            tier: Tier::Place,
            anchor: Some("c:/work/myproj".into()),
            ..Default::default()
        };
        let id = add_learned(&w).unwrap();
        let reload = |id: &str| {
            let id = id.to_string();
            load_all()
                .unwrap()
                .into_iter()
                .find(|e| e.id == id)
                .unwrap()
        };
        let e = reload(&id);
        assert_eq!(e.scope.as_deref(), Some("myproj-0a1b2c3d"));
        assert_eq!(e.subpath.as_deref(), Some("src/agent"));
        assert_eq!(e.tier, Tier::Place);
        assert_eq!(e.anchor.as_deref(), Some("c:/work/myproj"));

        // reinforce must carry scope AND the tier axis through verbatim (the easiest place to
        // silently lose either — a dropped anchor turns a one-repo fact into an always-on one).
        reinforce(&e, "s2").unwrap();
        let e2 = reload(&id);
        assert_eq!(
            e2.scope.as_deref(),
            Some("myproj-0a1b2c3d"),
            "reinforce kept the zone"
        );
        assert_eq!(e2.subpath.as_deref(), Some("src/agent"));
        assert_eq!(e2.tier, Tier::Place, "reinforce kept the tier");
        assert_eq!(
            e2.anchor.as_deref(),
            Some("c:/work/myproj"),
            "reinforce kept the anchor"
        );

        // ...and so must supersession
        mark_superseded(&e2, "replacement-id").unwrap();
        let e3 = reload(&id);
        assert_eq!(
            e3.scope.as_deref(),
            Some("myproj-0a1b2c3d"),
            "supersede kept the zone"
        );
        assert_eq!(e3.tier, Tier::Place, "supersede kept the tier");
        assert_eq!(
            e3.anchor.as_deref(),
            Some("c:/work/myproj"),
            "supersede kept the anchor"
        );

        // legacy + explicit-global both load as None
        let legacy = add("legacy fact", "", MemoryType::User, "plain body").unwrap();
        assert!(reload(&legacy).scope.is_none(), "no scope key → global");
        let g = add_scoped("explicit global", "", MemoryType::User, "b", Some("global")).unwrap();
        assert!(reload(&g).scope.is_none(), "literal 'global' → None");

        std::env::remove_var("NEXTGEN_HOME");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_patches_named_fields_and_preserves_the_rest() {
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-update-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);

        let w = LearnedWrite {
            name: "deploy note",
            description: "old summary",
            mtype: MemoryType::Project,
            body: "deploys from ci",
            source: crate::memory::provenance::ProvenanceKind::Inferred,
            confidence: 0.7,
            session_id: "s1",
            no_core: true,
            scope: Some("proj-0a1b2c3d".into()),
            subpath: Some("src/agent".into()),
            tier: Tier::Place,
            anchor: Some("c:/work/proj".into()),
            ..Default::default()
        };
        let id = add_learned(&w).unwrap();
        let reload = |id: &str| {
            let id = id.to_string();
            load_all()
                .unwrap()
                .into_iter()
                .find(|e| e.id == id)
                .unwrap()
        };
        let before = reload(&id);

        // patch the body only — every other field must survive untouched
        update(
            &before,
            &EntryPatch {
                body: Some("deploys from the tag workflow".into()),
                ..Default::default()
            },
        )
        .unwrap();
        let after = reload(&id);
        assert_eq!(after.body, "deploys from the tag workflow");
        assert_eq!(after.description, "old summary", "unnamed field kept");
        assert_eq!(after.mtype, MemoryType::Project);
        assert_eq!(
            after.scope.as_deref(),
            Some("proj-0a1b2c3d"),
            "zone not silently promoted to global"
        );
        assert_eq!(
            after.subpath.as_deref(),
            Some("src/agent"),
            "unknown-to-the-patch field kept"
        );
        assert!(after.core_denied, "an explicit core deny survives an edit");
        assert_eq!(after.confidence, 0.7);
        assert_eq!(after.created, before.created, "created is immutable");
        assert_eq!(
            after.updated.as_deref(),
            Some(today().as_str()),
            "updated is stamped"
        );

        // the id (filename) never moves, even when the display name changes
        update(
            &after,
            &EntryPatch {
                name: Some("deploy note v2".into()),
                mtype: Some(MemoryType::Reference),
                ..Default::default()
            },
        )
        .unwrap();
        let renamed = reload(&id);
        assert_eq!(
            renamed.id, id,
            "id is stable so supersededBy / graph edges stay valid"
        );
        assert_eq!(renamed.name, "deploy note v2");
        assert_eq!(renamed.mtype, MemoryType::Reference);
        assert_eq!(
            renamed.body, "deploys from the tag workflow",
            "body kept across a name-only patch"
        );

        // description → empty clears the key; scope → Some(None) re-globalizes deliberately
        update(
            &renamed,
            &EntryPatch {
                description: Some(String::new()),
                scope: Some(None),
                ..Default::default()
            },
        )
        .unwrap();
        let cleared = reload(&id);
        assert!(cleared.description.is_empty());
        assert!(
            cleared.scope.is_none(),
            "explicit Some(None) moves the fact global"
        );

        assert!(
            update(&cleared, &EntryPatch::default()).is_err(),
            "an empty patch is a caller bug"
        );

        std::env::remove_var("NEXTGEN_HOME");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn retire_is_recoverable_and_purge_is_not() {
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-retire-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);

        let id = add("obsolete fact", "", MemoryType::User, "no longer true").unwrap();
        let e = load_all()
            .unwrap()
            .into_iter()
            .find(|x| x.id == id)
            .unwrap();

        let archived = retire(&e).unwrap();
        assert!(
            !load_all().unwrap().iter().any(|x| x.id == id),
            "retired fact leaves the live store"
        );
        assert!(
            crate::memory::bloat::caps::list_archive()
                .unwrap()
                .iter()
                .any(|x| x.id == archived),
            "…but is recoverable from the archive"
        );
        crate::memory::bloat::caps::restore(&archived, None).unwrap();
        assert!(
            load_all().unwrap().iter().any(|x| x.id == id),
            "restore brings it back"
        );

        // purge is the human-only irreversible path, and only reaches ARCHIVED files
        let e = load_all()
            .unwrap()
            .into_iter()
            .find(|x| x.id == id)
            .unwrap();
        assert!(
            purge_archived(&id).is_err(),
            "a LIVE fact is never purgeable"
        );
        let archived = retire(&e).unwrap();
        purge_archived(&archived).unwrap();
        assert!(
            crate::memory::bloat::caps::list_archive()
                .unwrap()
                .is_empty(),
            "purge really deletes"
        );
        assert!(
            purge_archived(&archived).is_err(),
            "purging twice is an error, not a silent no-op"
        );

        std::env::remove_var("NEXTGEN_HOME");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn unsupersede_restores_a_fact_to_the_active_view() {
        // The reverse gear. Without it a wrong `contradict` is an effective data loss even though
        // every byte is still on disk — which is why this ships BEFORE any automatic contradict
        // branch. Also proves the field-map write keeps keys this build doesn't model.
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-unsup-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);

        let w = LearnedWrite {
            name: "package manager",
            body: "the project uses npm",
            mtype: MemoryType::User,
            tier: Tier::Place,
            anchor: Some("c:/work/proj".into()),
            ..Default::default()
        };
        let id = add_learned(&w).unwrap();
        let reload = |id: &str| {
            let id = id.to_string();
            load_all()
                .unwrap()
                .into_iter()
                .find(|e| e.id == id)
                .unwrap()
        };
        // Plant a key this build does not model, to prove the field-map path preserves it.
        let path = config::entries_dir().join(format!("{id}.md"));
        let raw = fs::read_to_string(&path).unwrap();
        fs::write(&path, raw.replacen("---\n", "---\nfutureKey: keep-me\n", 1)).unwrap();

        mark_superseded(&reload(&id), "replacement").unwrap();
        let dead = reload(&id);
        assert!(!dead.is_active(), "superseded");
        assert!(dead.valid_to.is_some() && dead.superseded_by.is_some());

        unsupersede(&dead).unwrap();
        let revived = reload(&id);
        assert!(revived.is_active(), "revive puts it back in the live view");
        assert!(revived.valid_to.is_none(), "validTo cleared");
        assert!(revived.superseded_by.is_none(), "supersededBy cleared");
        assert_eq!(
            revived.body, "the project uses npm",
            "body untouched by the round trip"
        );
        assert_eq!(revived.tier, Tier::Place, "tier survives");
        assert_eq!(
            revived.anchor.as_deref(),
            Some("c:/work/proj"),
            "anchor survives"
        );
        assert!(
            fs::read_to_string(&path)
                .unwrap()
                .contains("futureKey: keep-me"),
            "an unknown frontmatter key survives the reverse gear"
        );
        // Reviving something already live is a caller bug, not a silent no-op.
        assert!(unsupersede(&revived).is_err(), "double revive errors");

        std::env::remove_var("NEXTGEN_HOME");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_retrieval_dedups_per_day_and_bumps_reinforced() {
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-reuse-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);

        let id = add(
            "reuse target",
            "",
            MemoryType::Reference,
            "a fact to be reused",
        )
        .unwrap();
        let reload = || {
            load_all()
                .unwrap()
                .into_iter()
                .find(|e| e.id == id)
                .unwrap()
        };

        let e0 = reload();
        assert_eq!(e0.reinforced, 0);
        assert!(e0.last_retrieved.is_none());

        // first retrieval today → writes, bumps to 1
        assert!(record_retrieval(&e0, "2026-06-25").unwrap());
        let e1 = reload();
        assert_eq!(e1.reinforced, 1);
        assert_eq!(e1.last_retrieved.as_deref(), Some("2026-06-25"));

        // second retrieval SAME day → no-op (per-day dedup), count unchanged
        assert!(!record_retrieval(&e1, "2026-06-25").unwrap());
        assert_eq!(reload().reinforced, 1);

        // a new day → bumps again
        assert!(record_retrieval(&reload(), "2026-06-26").unwrap());
        let e2 = reload();
        assert_eq!(e2.reinforced, 2);
        assert_eq!(e2.last_retrieved.as_deref(), Some("2026-06-26"));
        // original authored content survives the metadata patches
        assert_eq!(e2.body, "a fact to be reused");

        std::env::remove_var("NEXTGEN_HOME");
        let _ = fs::remove_dir_all(&dir);
    }
}
