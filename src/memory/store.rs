//! The CLI-owned markdown memory store (source of truth) under `~/.nextgen/cli-memory/entries`.
//!
//! One fact per `*.md` file with frontmatter `name|description|type|created`.
//! P1 scope: load/list/add/get over plain markdown. Atomic-write + locking +
//! caps/eviction land in P4; this keeps the write path simple and correct first.

use crate::core::config;
use crate::memory::dimension::Dimension;
use crate::memory::frontmatter::{self, Frontmatter};
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
    pub scope: Option<String>,
    /// Optional region inside the project (`src/agent` style, `/`-normalized) — a soft ranking
    /// boost when the user works under it, never a hard partition.
    pub subpath: Option<String>,
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
        let created = fm.get("created").or_else(|| fm.get("createdAt")).map(str::to_string);
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
        let reinforced = fm.get("reinforced").and_then(|s| s.trim().parse().ok()).unwrap_or(0);
        let sessions = fm.get("sessions").and_then(|s| s.trim().parse().ok()).unwrap_or(0);
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
        let core_denied =
            fm.get("noCore").map(|s| s.trim().eq_ignore_ascii_case("true")).unwrap_or(false);
        let scope = parse_scope(fm.get("scope"));
        let subpath = fm
            .get("subpath")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.replace('\\', "/"));
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

impl MemoryEntry {
    /// A currently-valid (non-superseded) fact.
    pub fn is_active(&self) -> bool {
        self.valid_to.is_none() && self.superseded_by.is_none()
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
            Err(e) => eprintln!("[warn] skipping unreadable memory {}: {e}", path.display()),
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
pub fn add(
    name: &str,
    description: &str,
    mtype: MemoryType,
    body: &str,
) -> Result<String> {
    add_scoped(name, description, mtype, body, None)
}

/// `add` with a workspace scope (`None` = global). Separate entry point so every existing caller
/// keeps its signature; scoping callers (`#remember`, the learning router) opt in explicitly.
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
    if let Some(s) = scope.map(str::trim).filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("global")) {
        fields.insert("scope".to_string(), s.to_string());
    }
    let content =
        frontmatter::serialize(&fields, body, &["name", "description", "type", "scope", "created"]);
    write_atomic(&path, &content)?;
    Ok(id)
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
    /// Workspace scope (`None` = global; `Some(slug)` = one project zone).
    pub scope: Option<String>,
    /// Optional region inside the project (soft ranking boost).
    pub subpath: Option<String>,
}

const LEARNED_KEY_ORDER: &[&str] = &[
    "name",
    "description",
    "type",
    "scope",
    "subpath",
    "source",
    "confidence",
    "created",
    "updated",
    "reinforced",
    "sessions",
    "lastSession",
    "lastRetrieved",
    "validTo",
    "supersededBy",
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
}

fn render_learned(r: &LearnedRecord) -> String {
    let mut fields = BTreeMap::new();
    fields.insert("name".to_string(), r.name.trim().to_string());
    if !r.description.trim().is_empty() {
        fields.insert("description".to_string(), r.description.trim().to_string());
    }
    fields.insert("type".to_string(), r.mtype.as_str().to_string());
    fields.insert("source".to_string(), r.source.as_str().to_string());
    fields.insert("confidence".to_string(), format!("{:.2}", r.confidence.clamp(0.0, 1.0)));
    fields.insert("created".to_string(), r.created.to_string());
    fields.insert("updated".to_string(), r.updated.to_string());
    fields.insert("reinforced".to_string(), r.reinforced.to_string());
    fields.insert("sessions".to_string(), r.sessions.to_string());
    if !r.last_session.trim().is_empty() {
        fields.insert("lastSession".to_string(), r.last_session.trim().to_string());
    }
    if !r.last_retrieved.trim().is_empty() {
        fields.insert("lastRetrieved".to_string(), r.last_retrieved.trim().to_string());
    }
    if !r.valid_to.trim().is_empty() {
        fields.insert("validTo".to_string(), r.valid_to.trim().to_string());
    }
    if !r.superseded_by.trim().is_empty() {
        fields.insert("supersededBy".to_string(), r.superseded_by.trim().to_string());
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
    });
    write_atomic(&path, &content)?;
    Ok(id)
}

/// Reinforce an existing learned fact: bump `reinforced`, and bump `sessions` only when
/// `session_id` differs from the last one that touched it. Re-reads the file so concurrent
/// edits / extra fields are not clobbered. Returns the new `reinforced` count.
pub fn reinforce(entry: &MemoryEntry, session_id: &str) -> Result<u32> {
    let raw = fs::read_to_string(&entry.path)
        .with_context(|| format!("reading {} to reinforce", entry.path.display()))?;
    let fm = frontmatter::parse(&raw);

    let cur_reinforced: u32 = fm.get("reinforced").and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let cur_sessions: u32 = fm.get("sessions").and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    let last = fm.get("lastSession").unwrap_or("").trim().to_string();

    let reinforced = cur_reinforced.saturating_add(1);
    let sessions = if last == session_id {
        cur_sessions.max(1)
    } else {
        cur_sessions.saturating_add(1).max(1)
    };

    let name = fm.get("name").filter(|s| !s.trim().is_empty()).unwrap_or(&entry.name);
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
    let no_core = fm.get("noCore").map(|s| s.trim().eq_ignore_ascii_case("true")).unwrap_or(false);
    // scope/subpath survive reinforcement verbatim — a reinforce that dropped them would silently
    // promote a project fact to global.
    let scope = fm.get("scope").unwrap_or("").to_string();
    let subpath = fm.get("subpath").unwrap_or("").to_string();
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
    });
    write_atomic(&entry.path, &content)?;
    Ok(reinforced)
}

/// Bi-temporal supersession: mark `entry` as no-longer-valid (`validTo`=today, `supersededBy`=
/// `by_id`) WITHOUT deleting it — the history stays queryable via `ng memory as-of`. Re-reads
/// the file so all other fields are preserved.
pub fn mark_superseded(entry: &MemoryEntry, by_id: &str) -> Result<()> {
    let raw = fs::read_to_string(&entry.path)
        .with_context(|| format!("reading {} to supersede", entry.path.display()))?;
    let fm = frontmatter::parse(&raw);
    let name = fm.get("name").filter(|s| !s.trim().is_empty()).unwrap_or(&entry.name);
    let description = fm.get("description").unwrap_or(&entry.description);
    let mtype = MemoryType::parse(fm.get("type").unwrap_or(entry.mtype.as_str()));
    let source = ProvenanceKind::parse(fm.get("source").unwrap_or(entry.source.as_str()));
    let confidence = fm
        .get("confidence")
        .and_then(|s| s.trim().parse::<f64>().ok())
        .unwrap_or(entry.confidence);
    let created = fm.get("created").map(str::to_string).or_else(|| entry.created.clone()).unwrap_or_else(today);
    let reinforced: u32 = fm.get("reinforced").and_then(|s| s.trim().parse().ok()).unwrap_or(entry.reinforced);
    let sessions: u32 = fm.get("sessions").and_then(|s| s.trim().parse().ok()).unwrap_or(entry.sessions);
    let last_session = fm.get("lastSession").unwrap_or("").to_string();
    let last_retrieved = fm.get("lastRetrieved").unwrap_or("").to_string();
    let no_core = fm.get("noCore").map(|s| s.trim().eq_ignore_ascii_case("true")).unwrap_or(false);
    let scope = fm.get("scope").unwrap_or("").to_string();
    let subpath = fm.get("subpath").unwrap_or("").to_string();
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
        last_session: &last_session,
        last_retrieved: &last_retrieved,
        valid_to: &today(),
        superseded_by: by_id,
        no_core,
        scope: &scope,
        subpath: &subpath,
    });
    write_atomic(&entry.path, &content)?;
    Ok(())
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
    let cur: u32 = fm.get("reinforced").and_then(|s| s.trim().parse().ok()).unwrap_or(0);
    fields.insert("reinforced".to_string(), cur.saturating_add(1).to_string());
    fields.insert("lastRetrieved".to_string(), today.to_string());
    fields.insert("updated".to_string(), today.to_string());
    // type is required for a well-formed entry; a fence-less manual file may lack it.
    fields.entry("type".to_string()).or_insert_with(|| entry.mtype.as_str().to_string());
    let content = frontmatter::serialize(&fields, &fm.body, LEARNED_KEY_ORDER);
    write_atomic(&entry.path, &content)?;
    Ok(true)
}

/// Write atomically: temp file + rename. The temp name is unique per (process, write) so two `ng`
/// processes (or threads) writing the same entry never collide on a shared `.entry.md.tmp` and
/// clobber each other's rename. (P4 adds advisory locking + drift check.)
pub(crate) fn write_atomic(path: &Path, content: &str) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let dir = path.parent().context("memory path has no parent")?;
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("entry"),
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    fs::write(&tmp, content).with_context(|| format!("writing temp {}", tmp.display()))?;
    fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
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
        let _g = config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
        };
        let id = add_learned(&w).unwrap();
        let reload = |id: &str| {
            let id = id.to_string();
            load_all().unwrap().into_iter().find(|e| e.id == id).unwrap()
        };
        let e = reload(&id);
        assert_eq!(e.scope.as_deref(), Some("myproj-0a1b2c3d"));
        assert_eq!(e.subpath.as_deref(), Some("src/agent"));

        // reinforce must carry scope through verbatim (the easiest place to silently lose it)
        reinforce(&e, "s2").unwrap();
        let e2 = reload(&id);
        assert_eq!(e2.scope.as_deref(), Some("myproj-0a1b2c3d"), "reinforce kept the zone");
        assert_eq!(e2.subpath.as_deref(), Some("src/agent"));

        // ...and so must supersession
        mark_superseded(&e2, "replacement-id").unwrap();
        assert_eq!(reload(&id).scope.as_deref(), Some("myproj-0a1b2c3d"), "supersede kept the zone");

        // legacy + explicit-global both load as None
        let legacy = add("legacy fact", "", MemoryType::User, "plain body").unwrap();
        assert!(reload(&legacy).scope.is_none(), "no scope key → global");
        let g = add_scoped("explicit global", "", MemoryType::User, "b", Some("global")).unwrap();
        assert!(reload(&g).scope.is_none(), "literal 'global' → None");

        std::env::remove_var("NEXTGEN_HOME");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_retrieval_dedups_per_day_and_bumps_reinforced() {
        let _g = config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-reuse-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);

        let id = add("reuse target", "", MemoryType::Reference, "a fact to be reused").unwrap();
        let reload = || load_all().unwrap().into_iter().find(|e| e.id == id).unwrap();

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
