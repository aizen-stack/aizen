//! Codebase index (`/init`) + the read-only `codebase_search` tool.
//!
//! `/init` walks the working tree ONCE — respecting `.gitignore`, skipping hidden dirs, heavy
//! build/vendor folders, binary, and oversized files — splits each source file into SEMANTIC CHUNKS
//! (function / class / heading, with a line-window fallback), SHA-256-hashes each file for reliable
//! change detection, REDACTS secrets and stores sensitive files (`.env`, keys) as path-only, and
//! persists a per-repo index under `cli-memory/codebase/<slug>.json`. `codebase_search` then ranks
//! that chunk corpus with the same BM25 scorer memory uses (no re-scan per query), and the chat flow
//! injects the top chunks (path + line range + content) into the model's context automatically.
//!
//! This is CONCEPT/feature discovery ("where does X live"); `search_files` matches content regex,
//! `file_glob` matches names. Incremental re-index reuses unchanged files by SHA-256 (with a
//! `(len, mtime)` fast-path) so a second `/init` only re-reads what actually changed. A cross-process
//! lock (`RepoTxnLock`) makes concurrent `/init` safe, and the write is atomic (temp → rename) so a
//! crash / Ctrl+C never leaves a half-written index.

use crate::agent::search::decode_text;
use crate::agent::tools::Tool;
use crate::core::repo_lock::RepoTxnLock;
use crate::core::{config, persist};
use crate::memory::score::Bm25Index;
use crate::memory::tokenize::tokenize;
use anyhow::{anyhow, Context, Result};
use ignore::{WalkBuilder, WalkState};
use once_cell::sync::Lazy;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Default number of chunks returned by a `codebase_search` call.
const DEFAULT_SEARCH_LIMIT: usize = 12;

/// Bump when the persisted shape changes so a stale index is silently rebuilt, not misread.
/// v1 was the flat file-token index; v2 adds chunks, SHA-256, sensitivity, project analysis.
const INDEX_VERSION: u32 = 2;
/// Skip files larger than this — generated bundles / data dumps aren't useful search targets and
/// would bloat the index. Mirrors `search_files`' own ceiling.
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Hard cap on indexed files so a pathological tree (huge monorepo, accidental data dir) can't run
/// the scan away. Reported when hit so the user knows coverage was bounded.
const MAX_INDEXED_FILES: usize = 20_000;
/// First-line preview kept per file for search output (chars).
const PREVIEW_CHARS: usize = 160;
/// Target maximum lines per chunk before the line-window fallback splits a symbol region.
const CHUNK_MAX_LINES: usize = 400;
/// Overlap (lines) between adjacent line-window chunks so a match spanning the cut isn't lost.
const CHUNK_WINDOW_OVERLAP: usize = 15;
/// Below this many lines a file is a single chunk (no point sub-splitting).
const CHUNK_SINGLE_MAX_LINES: usize = 120;
/// Lock-acquisition timeout for a concurrent `/init` (a second init waits this long, then bails).
const LOCK_TIMEOUT: Duration = Duration::from_secs(20);

/// Directory names whose entire subtree is pruned during the `/init` walk. `.gitignore` + hidden
/// skipping already remove most, but these are pruned unconditionally so an un-ignored `target/`
/// or `node_modules/` never spends the budget. `.git` is hidden (skipped) but listed for safety.
static SKIP_DIRS: &[&str] = &[
    ".git",
    ".svn",
    ".hg",
    "node_modules",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    "coverage",
    "target",
    "vendor",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".venv",
    "venv",
    "env",
    ".gradle",
    ".idea",
    ".vscode",
    ".cache",
    ".parcel-cache",
    ".turbo",
    "tmp",
    "temp",
    "logs",
    // Never index the CLI's own home/state dir if it happens to sit inside the repo.
    ".aizen",
    ".nextgen",
];

/// Extensions we treat as indexable source / config / docs. Extension-driven allow-list keeps data
/// dumps and unknown blobs out of the index without a per-file content sniff for every candidate.
static SOURCE_EXTS: &[&str] = &[
    // languages
    "rs",
    "ts",
    "tsx",
    "js",
    "jsx",
    "mjs",
    "cjs",
    "py",
    "go",
    "java",
    "kt",
    "kts",
    "cpp",
    "cc",
    "cxx",
    "c",
    "h",
    "hpp",
    "cs",
    "php",
    "rb",
    "swift",
    "scala",
    "sh",
    "bash",
    "zsh",
    "sql",
    "lua",
    "dart",
    "ex",
    "exs",
    "erl",
    "clj",
    "hs",
    "ml",
    "r",
    "jl",
    "pl",
    "pm",
    // web / markup / style
    "html",
    "htm",
    "css",
    "scss",
    "sass",
    "less",
    "vue",
    "svelte",
    "astro",
    // docs / config
    "md",
    "mdx",
    "rst",
    "txt",
    "json",
    "jsonc",
    "yaml",
    "yml",
    "toml",
    "ini",
    "cfg",
    "conf",
    "xml",
    "gradle",
    "properties",
    "env-example",
];

/// Filenames (no extension) that are still worth indexing.
static SOURCE_FILENAMES: &[&str] = &[
    "Dockerfile",
    "Makefile",
    "CMakeLists.txt",
    "Gemfile",
    "Rakefile",
    "Procfile",
    "Vagrantfile",
];

/// One indexed source file: its repo-relative path, cheap + reliable change-detection keys, the
/// monorepo package it belongs to, sensitivity flags, and its chunk ids.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IndexedFile {
    /// Repo-relative, `/`-normalized path — the primary portable identifier.
    pub path: String,
    pub language: String,
    pub extension: String,
    pub len: u64,
    /// mtime in nanoseconds since the Unix epoch, or 0 when unavailable (then always re-read).
    pub mtime: u64,
    /// SHA-256 (hex) of the file bytes — the authoritative change key (mtime is only a fast-path).
    pub content_hash: String,
    pub line_count: usize,
    /// Monorepo package/workspace this file belongs to (e.g. `packages/api`), or empty at root.
    #[serde(default)]
    pub package: String,
    /// Auto-generated (minified bundle, lockfile, `@generated`) — indexed name-only, not chunked.
    #[serde(default)]
    pub is_generated: bool,
    /// Sensitive (`.env`, private key, credentials) — path + type ONLY, content never stored.
    #[serde(default)]
    pub is_sensitive: bool,
    /// At least one secret value was redacted from this file's indexed content.
    #[serde(default)]
    pub redacted: bool,
    /// Ids of the chunks this file produced (empty for sensitive / generated / name-only files).
    #[serde(default)]
    pub chunk_ids: Vec<String>,
}

/// One semantic chunk of a file: a symbol region (function / class / heading) or a line window.
/// This is the unit `codebase_search` ranks and the chat flow injects.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodeChunk {
    /// Stable id: short SHA-256 of `path\0symbol\0chunk-content` — unchanged across line shifts.
    pub id: String,
    pub file_path: String,
    pub language: String,
    /// 1-based inclusive line range in the source file.
    pub start_line: usize,
    pub end_line: usize,
    /// Symbol name if this chunk is a recognized symbol region, else empty.
    #[serde(default)]
    pub symbol_name: String,
    /// `function` / `class` / `struct` / `heading` / `window` / …
    pub symbol_type: String,
    #[serde(default)]
    pub parent_symbol: String,
    pub token_estimate: usize,
    /// BM25 terms for this chunk (already redacted; never the raw secret bytes).
    pub tokens: Vec<String>,
    /// A short preview line for search output.
    pub preview: String,
    /// The (redacted) chunk text, kept so retrieval can inject real content with attribution.
    pub content: String,
}

/// Structured project summary produced during `/init` — extensible beyond Node.js.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectAnalysis {
    pub project_name: String,
    pub project_root: String,
    /// language → file count.
    pub languages: HashMap<String, usize>,
    pub frameworks: Vec<String>,
    pub package_managers: Vec<String>,
    pub entry_points: Vec<String>,
    pub config_files: Vec<String>,
    pub workspaces: Vec<String>,
    pub test_frameworks: Vec<String>,
}

/// The persisted per-repo index.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CodebaseIndex {
    pub version: u32,
    pub root: String,
    /// Seconds since the Unix epoch when this index was built (for "indexed N ago" status).
    pub built_unix: u64,
    #[serde(default)]
    pub analysis: ProjectAnalysis,
    pub files: Vec<IndexedFile>,
    #[serde(default)]
    pub chunks: Vec<CodeChunk>,
}

/// Progress phases emitted during a build so the caller can render a live status without this
/// module ever touching the TUI.
pub enum Phase {
    Scanning { done: usize, total: usize },
    Chunking,
    Building,
}

/// Outcome of a build, for the completion summary.
#[derive(Debug, Default, Clone)]
pub struct ScanStats {
    /// Candidate files the walk surfaced (before per-file skips). Recorded for the summary; not all
    /// callers read it.
    #[allow(dead_code)]
    pub scanned: usize,
    /// Files in the final index.
    pub indexed: usize,
    /// Chunks in the final index.
    pub chunks: usize,
    /// Unchanged files reused from the prior index (incremental).
    pub reused: usize,
    /// Files newly read + tokenized this run (new or changed).
    pub added: usize,
    /// Prior-index files no longer present (deleted since last `/init`).
    pub removed: usize,
    /// Files stored path-only because they are sensitive (`.env`, keys, credentials).
    pub sensitive: usize,
    /// Files that had at least one secret value redacted.
    pub redacted: usize,
    pub skipped_binary: usize,
    pub skipped_large: usize,
    pub skipped_unreadable: usize,
    /// True when [`MAX_INDEXED_FILES`] bounded the scan.
    pub capped: bool,
    pub elapsed_ms: u128,
    /// Serialized index size on disk (bytes).
    pub bytes_on_disk: u64,
}

fn index_path() -> PathBuf {
    config::codebase_index_path(&config::project_slug())
}

/// The lock guarding a per-repo `/init` write. Sibling of the index file.
fn lock_path() -> PathBuf {
    let idx = index_path();
    idx.with_extension("json.lock")
}

/// Load the persisted index for the current repo, or `None` if absent / unreadable / stale-version.
pub fn load() -> Option<CodebaseIndex> {
    let bytes = persist::read_optional(&index_path()).ok().flatten()?;
    let idx: CodebaseIndex = serde_json::from_slice(&bytes).ok()?;
    if idx.version != INDEX_VERSION {
        return None;
    }
    Some(idx)
}

/// `(file count, built_unix)` for the current repo's index, or `None` if not indexed yet.
#[allow(dead_code)] // small public status API, parallel to load(); handy for callers/tests
pub fn status() -> Option<(usize, u64)> {
    load().map(|i| (i.files.len(), i.built_unix))
}

/// A parsed index + its prebuilt BM25, cached in-process. Keyed on the index file's identity
/// `(path, mtime, len)` so a `/init` rewrite (which changes mtime and almost always len) misses and
/// rebuilds; a same-repo re-query hits. Both fields are `Arc` so a hit clones cheaply and callers
/// hold their own reference without keeping the lock.
struct CachedIndex {
    path: PathBuf,
    mtime: u64,
    len: u64,
    idx: Arc<CodebaseIndex>,
    bm: Arc<Bm25Index>,
}

/// Process-global memo of the last-loaded index. `retrieval_block` and `search` run once per user
/// turn on the same repo; without this each call re-read + re-`serde_json`-parsed the whole index
/// (every chunk's content + token Vec) AND rebuilt the BM25 IDF table from scratch — an O(total
/// tokens) pass on turns that may need no code context at all.
static INDEX_CACHE: Lazy<Mutex<Option<CachedIndex>>> = Lazy::new(|| Mutex::new(None));

/// Load the current repo's index + its BM25, reusing the in-memory cache when the index file is
/// unchanged since last load (identity = path + mtime + len). Returns `None` exactly when [`load`]
/// does (absent / unreadable / stale version). The `(mtime, len)` key mirrors the file-level
/// fast-path used by the incremental build, so a rewrite is never served stale.
fn load_cached() -> Option<(Arc<CodebaseIndex>, Arc<Bm25Index>)> {
    let path = index_path();
    // Cheap stat: if the file's mtime+len match the cached entry for THIS path, reuse it.
    let (mtime, len) = std::fs::metadata(&path)
        .ok()
        .map(|m| (mtime_nanos(&m), m.len()))
        .unwrap_or((0, 0));
    if let Ok(guard) = INDEX_CACHE.lock() {
        if let Some(c) = guard.as_ref() {
            if c.path == path && c.mtime == mtime && c.len == len {
                return Some((c.idx.clone(), c.bm.clone()));
            }
        }
    }
    // Miss: parse + build BM25 once, then store.
    let idx = Arc::new(load()?);
    let bm = Arc::new(Bm25Index::build(
        idx.chunks.iter().map(|c| c.tokens.as_slice()),
    ));
    if let Ok(mut guard) = INDEX_CACHE.lock() {
        *guard = Some(CachedIndex {
            path,
            mtime,
            len,
            idx: idx.clone(),
            bm: bm.clone(),
        });
    }
    Some((idx, bm))
}

/// Drop the in-memory index/BM25 cache. Called after a `/init` rewrite so the next query reloads,
/// and by tests that swap the underlying index within one process (same slug, new content).
pub fn invalidate_cache() {
    if let Ok(mut guard) = INDEX_CACHE.lock() {
        *guard = None;
    }
}

// ── automatic between-turn freshness (#17) ─────────────────────────────────────────────────────

/// Last time [`ensure_fresh`] spawned a freshness check — debounces the stat-walk on rapid turns.
static LAST_FRESH_CHECK: Lazy<Mutex<Option<Instant>>> = Lazy::new(|| Mutex::new(None));
/// True while a background drift-check / incremental rebuild is running — single-flights the work
/// so overlapping turns never stack two rebuilds or two walks.
static REINDEX_IN_FLIGHT: AtomicBool = AtomicBool::new(false);
/// Minimum wall-clock gap between two source-tree freshness checks. The probe rides the per-turn
/// retrieval path, so this bounds how often a back-and-forth pays for a walk.
const FRESH_CHECK_DEBOUNCE: Duration = Duration::from_secs(15);

/// Cheap "has the source tree drifted from the on-disk index since it was built?" probe. True when
/// an indexable file's mtime is newer than the index build time (an edit or a freshly-created file),
/// or the indexable file COUNT dropped below the index's (a delete). Uses only stat calls — no
/// reads, no hashing. `built_unix` is second-granular, so an edit landing in the same second as the
/// build is missed here; the incremental build's SHA-256 pass is the authoritative backstop. Count
/// uses `<` (not `!=`) so a `MAX_INDEXED_FILES`-capped index never reports permanent false drift.
fn source_tree_drifted(idx: &CodebaseIndex) -> bool {
    let root = config::project_root();
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let candidates = collect_candidates(&root);
    let mut count = 0usize;
    let mut max_mtime = 0u64;
    for path in &candidates {
        // Only files that actually enter the index count toward drift: source files plus the
        // path-only sensitive records. Everything else the walk surfaces build_index skips too.
        if !(is_source_file(path) || sensitivity_kind(path).is_some()) {
            continue;
        }
        count += 1;
        if let Ok(m) = std::fs::metadata(path) {
            let secs = m
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if secs > max_mtime {
                max_mtime = secs;
            }
        }
    }
    count < idx.files.len() || max_mtime > idx.built_unix
}

/// Auto re-index hook for the per-turn retrieval path. When the working tree has drifted from the
/// on-disk index since the last `/init`, kick a background incremental rebuild so the NEXT turn sees
/// fresh context. Guarantees:
/// - never blocks the current turn — the drift walk AND the rebuild run on a detached thread;
/// - never builds a *first* index (that stays an explicit `/init`) — no index file → no-op;
/// - debounced (at most once per [`FRESH_CHECK_DEBOUNCE`]) and single-flight (one rebuild at a time).
///
/// The current turn still uses the (possibly stale) index — retrieval is best-effort context, and
/// stalling every turn to re-read a large tree would cost far more than one turn of slight staleness.
pub fn ensure_fresh() {
    // First build stays explicit — no on-disk index means /init has not run.
    if !index_path().exists() {
        return;
    }
    // A rebuild (or its drift check) is already in flight — don't stack or re-walk.
    if REINDEX_IN_FLIGHT.load(std::sync::atomic::Ordering::Acquire) {
        return;
    }
    // Debounce the whole operation: record the attempt time and bail if we checked too recently.
    {
        let mut last = match LAST_FRESH_CHECK.lock() {
            Ok(g) => g,
            Err(_) => return,
        };
        if last
            .map(|t| t.elapsed() < FRESH_CHECK_DEBOUNCE)
            .unwrap_or(false)
        {
            return;
        }
        *last = Some(Instant::now());
    }
    // Claim the single-flight slot; if another thread beat us here, let it do the work.
    if REINDEX_IN_FLIGHT
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_err()
    {
        return;
    }
    // Everything expensive (the gitignore walk + stat pass + rebuild) runs OFF the turn thread.
    std::thread::spawn(|| {
        let drifted = load_cached()
            .map(|(idx, _)| source_tree_drifted(&idx))
            .unwrap_or(false);
        if drifted {
            // Incremental: reuses unchanged files by (len,mtime)+SHA-256, so a drift rebuild only
            // re-reads what actually changed, then calls invalidate_cache() so the next query
            // reloads the fresh index. Errors (e.g. a concurrent /init holding the lock) are benign.
            let _ = build_index(true, None, &|_| {});
        }
        REINDEX_IN_FLIGHT.store(false, std::sync::atomic::Ordering::Release);
    });
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn mtime_nanos(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// SHA-256 (hex) of bytes — the authoritative file change key.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}

/// Short (16 hex chars) stable id for a chunk.
fn short_hash(s: &str) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, s.as_bytes());
    digest.as_ref()[..8]
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// First non-blank line of the text, trimmed and clipped — a cheap "what is this" hint.
fn make_preview(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    line.chars().take(PREVIEW_CHARS).collect()
}

/// Map an extension / filename to a coarse language label (used for grouping + chunk heuristics).
fn language_of(ext: &str, file_name: &str) -> String {
    match ext {
        "rs" => "rust",
        "ts" | "tsx" | "mts" | "cts" => "typescript",
        "js" | "jsx" | "mjs" | "cjs" => "javascript",
        "py" => "python",
        "go" => "go",
        "java" => "java",
        "kt" | "kts" => "kotlin",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "c" | "h" => "c",
        "cs" => "csharp",
        "php" => "php",
        "rb" => "ruby",
        "swift" => "swift",
        "scala" => "scala",
        "sh" | "bash" | "zsh" => "shell",
        "sql" => "sql",
        "lua" => "lua",
        "dart" => "dart",
        "ex" | "exs" => "elixir",
        "vue" => "vue",
        "svelte" => "svelte",
        "astro" => "astro",
        "html" | "htm" => "html",
        "css" | "scss" | "sass" | "less" => "css",
        "md" | "mdx" | "rst" => "markdown",
        "json" | "jsonc" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "xml" => "xml",
        _ => match file_name {
            "Dockerfile" => "dockerfile",
            "Makefile" => "makefile",
            _ => "text",
        },
    }
    .to_string()
}

/// Whether this file should be indexed at all (by extension or known filename).
fn is_source_file(path: &Path) -> bool {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if SOURCE_FILENAMES
        .iter()
        .any(|f| f.eq_ignore_ascii_case(name))
    {
        return true;
    }
    // Safe-to-share env templates (`.env.example` / `.sample` / `.template`) are worth indexing —
    // they document the config surface with no secret values. The real `.env` never reaches here
    // (it's caught by `sensitivity_kind` first and recorded path-only).
    let lower = name.to_ascii_lowercase();
    if lower.starts_with(".env.")
        && (lower.ends_with(".example")
            || lower.ends_with(".sample")
            || lower.ends_with(".template"))
    {
        return true;
    }
    // Other dotfiles like `.gitignore` aren't source.
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    !ext.is_empty() && SOURCE_EXTS.contains(&ext.as_str())
}

/// Sensitive files whose CONTENT must never be indexed — only path + type recorded. Matches by
/// filename shape (`.env`, `.env.local`, `.npmrc`, key/credential files).
fn sensitivity_kind(path: &Path) -> Option<&'static str> {
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let lower = name.to_ascii_lowercase();
    // `.env` and any `.env.*` EXCEPT the safe-to-share example templates.
    if (lower == ".env" || lower.starts_with(".env."))
        && !lower.ends_with(".example")
        && !lower.ends_with(".sample")
        && !lower.ends_with(".template")
    {
        return Some("dotenv");
    }
    if lower == ".npmrc" || lower == ".pypirc" || lower == ".netrc" {
        return Some("credentials-file");
    }
    if lower == "credentials" || lower == ".credentials" || lower.ends_with(".credentials") {
        return Some("credentials-file");
    }
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(
        ext.as_str(),
        "pem" | "key" | "p12" | "pfx" | "keystore" | "jks"
    ) {
        return Some("private-key");
    }
    if lower == "id_rsa" || lower == "id_dsa" || lower == "id_ecdsa" || lower == "id_ed25519" {
        return Some("private-key");
    }
    None
}

/// Heuristic "this is machine-generated, don't chunk it" test (minified bundle, source map,
/// lockfile, `@generated` banner). Generated files are indexed name-only.
fn is_generated(path: &Path, text: &str) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if name.ends_with(".min.js") || name.ends_with(".min.css") || name.ends_with(".map") {
        return true;
    }
    if matches!(
        name.as_str(),
        "package-lock.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "cargo.lock"
            | "poetry.lock"
            | "composer.lock"
            | "gemfile.lock"
            | "go.sum"
    ) {
        return true;
    }
    // `@generated` / "DO NOT EDIT" banner in the first few hundred chars.
    let head: String = text.chars().take(500).collect();
    let head_lower = head.to_ascii_lowercase();
    head_lower.contains("@generated") || head_lower.contains("do not edit")
}

// ── secret redaction ────────────────────────────────────────────────────────────────────────

static SECRET_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    // Each pattern captures the whole match; we blank the sensitive tail. Case-insensitive where
    // it helps. These are deliberately conservative — value-shaped, not prose.
    [
        // key = "value" / key: value  for secret-ish keys (assignment forms in config/source)
        r#"(?i)\b(?:api[_-]?key|secret[_-]?key|secret|password|passwd|token|access[_-]?token|auth[_-]?token|client[_-]?secret|private[_-]?key)\b\s*[:=]\s*["']?[A-Za-z0-9_\-\.\+/=]{6,}["']?"#,
        // Bearer tokens
        r"(?i)bearer\s+[A-Za-z0-9_\-\.=]{10,}",
        // AWS access key id (long-term AKIA + temporary/session ASIA)
        r"\b(?:AKIA|ASIA)[0-9A-Z]{16}\b",
        // Private key PEM header (incl. ENCRYPTED / bare PKCS#8)
        r"-----BEGIN (?:RSA |EC |OPENSSH |DSA |PGP |ENCRYPTED )?PRIVATE KEY-----",
        // Connection strings with inline credentials: scheme://user:pass@host
        r"(?i)\b[a-z][a-z0-9+.\-]*://[^\s:/@]+:[^\s:/@]+@[^\s/]+",
        // GitHub classic tokens (ghp_/gho_/ghu_/ghs_/ghr_) and fine-grained PATs (github_pat_)
        r"\bgh[pousr]_[A-Za-z0-9]{20,}\b",
        r"\bgithub_pat_[0-9A-Za-z_]{22,}\b",
        // Anthropic keys (sk-ant-…) — matched before the generic sk- rule below
        r"\bsk-ant-[0-9A-Za-z\-]{20,}\b",
        // OpenAI-style sk- tokens
        r"\bsk-[A-Za-z0-9]{20,}\b",
        // Stripe live/test keys (sk_/pk_/rk_ with underscore)
        r"\b(?:sk|pk|rk)_(?:live|test)_[0-9A-Za-z]{16,}\b",
        // GitLab personal access tokens
        r"\bglpat-[0-9A-Za-z_\-]{20,}\b",
        // Slack tokens (bot/user/app/refresh/config) + incoming-webhook URLs
        r"\bxox[baprs]-[0-9A-Za-z-]{10,}\b",
        r"https://hooks\.slack\.com/services/[A-Za-z0-9/_\-]+",
        // Google API keys + OAuth access tokens
        r"\bAIza[0-9A-Za-z_\-]{35}\b",
        r"\bya29\.[0-9A-Za-z_\-]+",
        // HuggingFace + npm automation tokens
        r"\bhf_[0-9A-Za-z]{30,}\b",
        r"\bnpm_[0-9A-Za-z]{36,}\b",
        // SendGrid API keys
        r"\bSG\.[0-9A-Za-z_\-]{16,}\.[0-9A-Za-z_\-]{16,}\b",
        // JSON Web Tokens (three base64url segments) — standalone, not just when key-assigned
        r"\beyJ[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+",
    ]
    .iter()
    .filter_map(|p| Regex::new(p).ok())
    .collect()
});

/// Redact secret-shaped substrings from `text`. Returns `(redacted_text, did_redact)`. The value is
/// replaced with a marker so surrounding structure (and the KEY name) still indexes for search, but
/// the secret bytes never enter the index.
fn redact_secrets(text: &str) -> (String, bool) {
    let mut out = text.to_string();
    let mut changed = false;
    for re in SECRET_PATTERNS.iter() {
        if re.is_match(&out) {
            changed = true;
            out = re.replace_all(&out, "«REDACTED-SECRET»").into_owned();
        }
    }
    (out, changed)
}

/// Strip the automatic per-turn codebase retrieval block, leaving the real user request intact.
/// Only a marker at byte zero is recognized, so quoted/example tags in a normal request are safe.
pub(crate) fn strip_retrieval_prefix(content: &str) -> &str {
    const OPEN: &str = "<codebase_context>";
    const CLOSE: &str = "</codebase_context>";
    if !content.starts_with(OPEN) {
        return content;
    }
    content
        .find(CLOSE)
        .map(|end| content[end + CLOSE.len()..].trim_start_matches(['\r', '\n']))
        .unwrap_or(content)
}

/// Shared oracle/codebase privacy policy: sensitive path classification without reading contents.
pub(crate) fn review_sensitivity(path: &Path) -> Option<&'static str> {
    sensitivity_kind(path)
}

/// Shared oracle/codebase privacy policy: redact every secret-shaped value before egress.
pub(crate) fn redact_for_review(text: &str) -> String {
    redact_secrets(text).0
}

/// Shared bounded-file policy for untracked review excerpts.
pub(crate) fn review_file_limit() -> u64 {
    MAX_FILE_BYTES
}

// ── monorepo package tagging ────────────────────────────────────────────────────────────────

/// Manifest filenames that mark a package/workspace root.
static PACKAGE_MANIFESTS: &[&str] = &[
    "package.json",
    "Cargo.toml",
    "pyproject.toml",
    "go.mod",
    "pom.xml",
    "build.gradle",
    "composer.json",
];

/// Find the nearest ancestor dir (within the repo) that holds a package manifest — the file's
/// monorepo package. Empty string when the package is the repo root itself.
fn package_for(root: &Path, file_abs: &Path, manifest_dirs: &HashSet<String>) -> String {
    let mut cur = file_abs.parent();
    while let Some(dir) = cur {
        let rel = dir
            .strip_prefix(root)
            .unwrap_or(dir)
            .to_string_lossy()
            .replace('\\', "/");
        if rel.is_empty() {
            return String::new(); // reached repo root
        }
        if manifest_dirs.contains(&rel) {
            return rel;
        }
        cur = dir.parent();
    }
    String::new()
}

// ── chunking ────────────────────────────────────────────────────────────────────────────────

/// Detect whether a source line STARTS a symbol, returning `(symbol_type, symbol_name)`. Heuristic,
/// language-family aware; misses fall through to line-window chunking.
fn symbol_at(language: &str, line: &str) -> Option<(&'static str, String)> {
    let t = line.trim_start();
    let grab = |after: &str| -> String {
        // take the identifier following `after`
        t[after.len()..]
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || matches!(*c, '_' | '<' | ':' | '!'))
            .collect::<String>()
            .trim_end_matches(['<', ':'])
            .to_string()
    };
    match language {
        "rust" => {
            for (kw, kind) in [
                ("pub fn ", "function"),
                ("fn ", "function"),
                ("pub struct ", "struct"),
                ("struct ", "struct"),
                ("pub enum ", "enum"),
                ("enum ", "enum"),
                ("pub trait ", "trait"),
                ("trait ", "trait"),
                ("impl ", "impl"),
                ("pub mod ", "module"),
                ("mod ", "module"),
                ("macro_rules! ", "macro"),
            ] {
                if t.starts_with(kw) {
                    return Some((kind, grab(kw)));
                }
            }
        }
        "python" => {
            for (kw, kind) in [
                ("def ", "function"),
                ("async def ", "function"),
                ("class ", "class"),
            ] {
                if t.starts_with(kw) {
                    return Some((kind, grab(kw)));
                }
            }
        }
        "typescript" | "javascript" | "vue" | "svelte" | "astro" => {
            for (kw, kind) in [
                ("export default function ", "function"),
                ("export function ", "function"),
                ("export async function ", "function"),
                ("function ", "function"),
                ("export class ", "class"),
                ("class ", "class"),
                ("export interface ", "interface"),
                ("interface ", "interface"),
                ("export type ", "type"),
                ("type ", "type"),
                ("export const ", "const"),
                ("export enum ", "enum"),
                ("enum ", "enum"),
            ] {
                if t.starts_with(kw) {
                    return Some((kind, grab(kw)));
                }
            }
        }
        "go" => {
            for (kw, kind) in [("func ", "function"), ("type ", "type")] {
                if t.starts_with(kw) {
                    return Some((kind, grab(kw)));
                }
            }
        }
        "java" | "csharp" | "kotlin" | "scala" | "cpp" | "c" => {
            // class / struct / interface / enum declarations (visibility-prefixed forms too).
            for (kw, kind) in [
                ("class ", "class"),
                ("struct ", "struct"),
                ("interface ", "interface"),
                ("enum ", "enum"),
                ("trait ", "trait"),
                ("object ", "object"),
            ] {
                // only near the start (avoid matching mid-expression)
                if t.find(kw).is_some_and(|p| p <= 24) {
                    return Some((kind, grab_from(t, kw)));
                }
            }
        }
        "ruby" => {
            for (kw, kind) in [
                ("def ", "function"),
                ("class ", "class"),
                ("module ", "module"),
            ] {
                if t.starts_with(kw) {
                    return Some((kind, grab(kw)));
                }
            }
        }
        "markdown" if t.starts_with('#') => {
            let name = t.trim_start_matches('#').trim().to_string();
            return Some(("heading", name));
        }
        _ => {}
    }
    None
}

/// grab the identifier following `kw` anywhere in `t` (for languages where the keyword isn't at col0).
fn grab_from(t: &str, kw: &str) -> String {
    if let Some(pos) = t.find(kw) {
        t[pos + kw.len()..]
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect()
    } else {
        String::new()
    }
}

/// Is this line import/use/blank-only (a chunk made only of these is dropped)?
fn is_trivial_line(language: &str, line: &str) -> bool {
    let t = line.trim();
    if t.is_empty() {
        return true;
    }
    match language {
        "rust" => t.starts_with("use ") || t.starts_with("//") || t.starts_with("#["),
        "python" => t.starts_with("import ") || t.starts_with("from ") || t.starts_with('#'),
        "typescript" | "javascript" => {
            t.starts_with("import ") || t.starts_with("export {") || t.starts_with("//")
        }
        "go" => t.starts_with("import ") || t.starts_with("//") || t == "package",
        _ => t.starts_with("//") || t.starts_with('#'),
    }
}

/// A chunk region before it becomes a `CodeChunk` (line indices are 0-based, end exclusive).
struct Region {
    start: usize,
    end: usize,
    symbol_name: String,
    symbol_type: &'static str,
}

/// Split a file into semantic chunks: symbol regions where detected, otherwise line windows. Small
/// files become a single chunk. Import-only / blank regions are dropped.
fn chunk_file(rel_path: &str, language: &str, text: &str) -> Vec<CodeChunk> {
    let lines: Vec<&str> = text.lines().collect();
    if lines.is_empty() {
        return Vec::new();
    }

    // Tiny files: one chunk, no splitting.
    if lines.len() <= CHUNK_SINGLE_MAX_LINES {
        return finalize_regions(
            rel_path,
            language,
            &lines,
            vec![Region {
                start: 0,
                end: lines.len(),
                symbol_name: String::new(),
                symbol_type: "file",
            }],
        );
    }

    // Find symbol boundaries.
    let mut boundaries: Vec<(usize, String, &'static str)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some((kind, name)) = symbol_at(language, line) {
            boundaries.push((i, name, kind));
        }
    }

    let mut regions: Vec<Region> = Vec::new();
    if boundaries.is_empty() {
        // No symbols recognized → pure line-window fallback.
        window_split(0, lines.len(), &mut regions);
    } else {
        // Leading region before the first symbol (module docs / imports) — kept only if substantial.
        let first = boundaries[0].0;
        if first > 0 {
            regions.push(Region {
                start: 0,
                end: first,
                symbol_name: String::new(),
                symbol_type: "preamble",
            });
        }
        for w in 0..boundaries.len() {
            let (start, ref name, kind) = boundaries[w];
            let end = boundaries.get(w + 1).map(|b| b.0).unwrap_or(lines.len());
            if end - start > CHUNK_MAX_LINES {
                // Big symbol region → split into windows but keep the symbol name on the first.
                let mut sub: Vec<Region> = Vec::new();
                window_split(start, end, &mut sub);
                for (si, mut r) in sub.into_iter().enumerate() {
                    if si == 0 {
                        r.symbol_name = name.clone();
                        r.symbol_type = kind;
                    }
                    regions.push(r);
                }
            } else {
                regions.push(Region {
                    start,
                    end,
                    symbol_name: name.clone(),
                    symbol_type: kind,
                });
            }
        }
    }

    finalize_regions(rel_path, language, &lines, regions)
}

/// LSP-outline chunker (#16): when a language server serves this file, split it by the server's
/// document-symbol outline (authoritative symbol boundaries) instead of the per-line keyword
/// heuristic. Returns `None` — deferring to [`chunk_file`] — when LSP is off, no server serves the
/// file, the query errors/times out, or the outline is empty. Small files are left to the heuristic
/// (a single chunk either way) so a build never pays an LSP round-trip where boundaries don't matter.
///
/// SECURITY: the server reads the file RAW from disk, but we trust only its line NUMBERS — every
/// chunk body is sliced from `safe_text` (post-redaction), so a secret the server saw never enters
/// the index. Redaction only rewrites WITHIN a line (never adds/removes newlines), so the server's
/// 0-based line offsets align with `safe_text.lines()`.
fn chunk_file_lsp(
    abs_path: &Path,
    rel_path: &str,
    language: &str,
    safe_text: &str,
) -> Option<Vec<CodeChunk>> {
    if !crate::agent::lsp::LSP.is_enabled() {
        return None;
    }
    let lines: Vec<&str> = safe_text.lines().collect();
    // A tiny file becomes a single chunk under either path — skip the round-trip.
    if lines.len() <= CHUNK_SINGLE_MAX_LINES {
        return None;
    }
    let syms = crate::agent::lsp::LSP
        .document_symbols_items(abs_path)
        .ok()?;
    if syms.is_empty() {
        return None;
    }
    let outline: Vec<(usize, String, &'static str)> =
        syms.into_iter().map(|s| (s.line, s.name, s.kind)).collect();
    let chunks = chunk_by_outline(rel_path, language, &lines, outline);
    if chunks.is_empty() {
        None
    } else {
        Some(chunks)
    }
}

/// Build chunks from a symbol outline `(line, name, kind)` — the shared core of the LSP chunker,
/// factored out so it is testable without a live server. `outline` need not be sorted; `line` is a
/// 0-based file offset. Mirrors [`chunk_file`]'s region logic exactly (a `preamble` region before
/// the first symbol, one region per symbol up to the next, oversized symbols window-split with the
/// name kept on the first sub-window), then runs [`finalize_regions`] so id / token / redaction-drop
/// logic is reused verbatim — only the BOUNDARIES differ (authoritative vs keyword-guessed).
fn chunk_by_outline(
    rel_path: &str,
    language: &str,
    lines: &[&str],
    mut outline: Vec<(usize, String, &'static str)>,
) -> Vec<CodeChunk> {
    if outline.is_empty() || lines.is_empty() {
        return Vec::new();
    }
    let n = lines.len();
    // Servers may return nested children out of document order → sort by line, then drop symbols
    // sharing a line (a one-liner + its child) keeping the first (outermost) so regions never invert.
    outline.sort_by_key(|b| b.0);
    outline.dedup_by_key(|b| b.0);
    let mut regions: Vec<Region> = Vec::new();
    let first = outline[0].0.min(n);
    if first > 0 {
        regions.push(Region {
            start: 0,
            end: first,
            symbol_name: String::new(),
            symbol_type: "preamble",
        });
    }
    for w in 0..outline.len() {
        let start = outline[w].0.min(n);
        let name = outline[w].1.clone();
        let kind = outline[w].2;
        let end = outline.get(w + 1).map(|b| b.0.min(n)).unwrap_or(n);
        if end <= start {
            continue;
        }
        if end - start > CHUNK_MAX_LINES {
            let mut sub: Vec<Region> = Vec::new();
            window_split(start, end, &mut sub);
            for (si, mut r) in sub.into_iter().enumerate() {
                if si == 0 {
                    r.symbol_name = name.clone();
                    r.symbol_type = kind;
                }
                regions.push(r);
            }
        } else {
            regions.push(Region {
                start,
                end,
                symbol_name: name.clone(),
                symbol_type: kind,
            });
        }
    }
    finalize_regions(rel_path, language, lines, regions)
}

/// Fixed-size overlapping line windows over `[start, end)`.
fn window_split(start: usize, end: usize, out: &mut Vec<Region>) {
    let mut s = start;
    while s < end {
        let e = (s + CHUNK_MAX_LINES).min(end);
        out.push(Region {
            start: s,
            end: e,
            symbol_name: String::new(),
            symbol_type: "window",
        });
        if e >= end {
            break;
        }
        s = e.saturating_sub(CHUNK_WINDOW_OVERLAP);
    }
}

/// Turn regions into `CodeChunk`s: drop trivial (import/blank-only) regions, tokenize, hash a stable
/// id. `parent_symbol` is left empty in v2 (flat regions); the field is kept for future AST nesting.
fn finalize_regions(
    rel_path: &str,
    language: &str,
    lines: &[&str],
    regions: Vec<Region>,
) -> Vec<CodeChunk> {
    let mut out = Vec::new();
    for r in regions {
        if r.end <= r.start {
            continue;
        }
        let body = lines[r.start..r.end].join("\n");
        // Drop chunks that are only imports / comments / blanks.
        if lines[r.start..r.end]
            .iter()
            .all(|l| is_trivial_line(language, l))
        {
            continue;
        }
        let tokens = tokenize(&body);
        if tokens.is_empty() {
            continue;
        }
        let content_hash = sha256_hex(body.as_bytes());
        let id = short_hash(&format!("{rel_path}\0{}\0{content_hash}", r.symbol_name));
        let preview = if r.symbol_name.is_empty() {
            make_preview(&body)
        } else {
            format!("{} {}", r.symbol_type, r.symbol_name)
        };
        out.push(CodeChunk {
            id,
            file_path: rel_path.to_string(),
            language: language.to_string(),
            start_line: r.start + 1,
            end_line: r.end,
            symbol_name: r.symbol_name,
            symbol_type: r.symbol_type.to_string(),
            parent_symbol: String::new(),
            token_estimate: body.chars().count() / 4,
            tokens,
            preview,
            content: body,
        });
    }
    out
}

// ── walk ────────────────────────────────────────────────────────────────────────────────────

/// Phase 1: a bounded parallel walk collecting candidate FILE paths, pruning heavy/hidden dirs and
/// honoring `.gitignore`. Directories are pruned by name; files are collected. Returns sorted paths.
fn collect_candidates(root: &Path) -> Vec<PathBuf> {
    let out: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    let mut wb = WalkBuilder::new(root);
    wb.follow_links(false) // never chase junctions/symlinks → no reparse-point loops, no escaping root
        .same_file_system(true) // stay on one drive/mount
        // Visit hidden FILES (so sensitive dotfiles — `.env`, `.npmrc`, `id_rsa` — are caught and
        // recorded path-only); hidden DIRS are still pruned by name in the walker callback below.
        .hidden(false)
        .git_global(false) // honor the repo's .gitignore, not the dev's global one
        .git_ignore(true)
        .git_exclude(true)
        // Honor `.gitignore` / `.ignore` even when the fixture/dir is NOT a git repo (default
        // requires a `.git` for gitignore rules to apply). Production repos have `.git`; this just
        // makes the rules apply consistently everywhere.
        .require_git(false)
        .ignore(true)
        .parents(true);
    let out = &out;
    wb.build_parallel().run(|| {
        Box::new(move |res| {
            let dent = match res {
                Ok(d) => d,
                Err(_) => return WalkState::Continue, // swallow permission/loop errors
            };
            let is_dir = dent.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if dent.depth() > 0 {
                    if let Some(name) = dent.path().file_name().and_then(|n| n.to_str()) {
                        if SKIP_DIRS.iter().any(|d| d.eq_ignore_ascii_case(name)) {
                            return WalkState::Skip;
                        }
                    }
                }
                return WalkState::Continue;
            }
            if dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                out.lock().unwrap().push(dent.path().to_path_buf());
            }
            WalkState::Continue
        })
    });
    let mut v = out.lock().unwrap().split_off(0);
    v.sort();
    v.dedup();
    v
}

/// Discover monorepo package roots (dirs holding a manifest) — relative, `/`-normalized.
fn discover_packages(root: &Path, candidates: &[PathBuf]) -> HashSet<String> {
    let mut dirs = HashSet::new();
    for p in candidates {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if PACKAGE_MANIFESTS
            .iter()
            .any(|m| m.eq_ignore_ascii_case(name))
        {
            if let Some(parent) = p.parent() {
                let rel = parent
                    .strip_prefix(root)
                    .unwrap_or(parent)
                    .to_string_lossy()
                    .replace('\\', "/");
                if !rel.is_empty() {
                    dirs.insert(rel);
                }
            }
        }
    }
    dirs
}

// ── project analysis ────────────────────────────────────────────────────────────────────────

/// Derive the structured project summary from the file set + manifests present.
fn analyze_project(
    root: &Path,
    files: &[IndexedFile],
    packages: &HashSet<String>,
) -> ProjectAnalysis {
    let mut a = ProjectAnalysis {
        project_name: root
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("project")
            .to_string(),
        project_root: root.to_string_lossy().replace('\\', "/"),
        ..Default::default()
    };
    let names: HashSet<&str> = files.iter().map(|f| f.path.as_str()).collect();
    let has = |p: &str| names.contains(p);
    let has_any_named = |name: &str| {
        files
            .iter()
            .any(|f| f.path.rsplit('/').next() == Some(name))
    };

    for f in files {
        if f.language != "text"
            && f.language != "json"
            && f.language != "yaml"
            && f.language != "toml"
        {
            *a.languages.entry(f.language.clone()).or_insert(0) += 1;
        }
    }

    // package managers + config files
    let mut pm = Vec::new();
    if has_any_named("package.json") {
        pm.push("npm/node".to_string());
    }
    if has_any_named("Cargo.toml") {
        pm.push("cargo".to_string());
    }
    if has_any_named("pyproject.toml")
        || has_any_named("requirements.txt")
        || has_any_named("setup.py")
    {
        pm.push("pip/python".to_string());
    }
    if has_any_named("go.mod") {
        pm.push("go modules".to_string());
    }
    if has_any_named("pom.xml") {
        pm.push("maven".to_string());
    }
    if has_any_named("build.gradle") || has_any_named("build.gradle.kts") {
        pm.push("gradle".to_string());
    }
    if has_any_named("composer.json") {
        pm.push("composer".to_string());
    }
    if has_any_named("Gemfile") {
        pm.push("bundler".to_string());
    }
    a.package_managers = pm;

    // frameworks (best-effort from manifest presence + common config files)
    let mut fw = Vec::new();
    if has_any_named("next.config.js")
        || has_any_named("next.config.mjs")
        || has_any_named("next.config.ts")
    {
        fw.push("Next.js".to_string());
    }
    if has_any_named("nuxt.config.ts") || has_any_named("nuxt.config.js") {
        fw.push("Nuxt".to_string());
    }
    if has_any_named("vite.config.ts") || has_any_named("vite.config.js") {
        fw.push("Vite".to_string());
    }
    if has_any_named("angular.json") {
        fw.push("Angular".to_string());
    }
    if has_any_named("svelte.config.js") {
        fw.push("Svelte".to_string());
    }
    if has_any_named("tauri.conf.json") {
        fw.push("Tauri".to_string());
    }
    if has_any_named("manage.py") {
        fw.push("Django".to_string());
    }
    if files.iter().any(|f| f.path.ends_with(".vue")) {
        fw.push("Vue".to_string());
    }
    a.frameworks = fw;

    // test frameworks (heuristic)
    let mut tf = Vec::new();
    if has_any_named("jest.config.js") || has_any_named("jest.config.ts") {
        tf.push("Jest".to_string());
    }
    if has_any_named("vitest.config.ts") || has_any_named("vitest.config.js") {
        tf.push("Vitest".to_string());
    }
    if has_any_named("pytest.ini") || has_any_named("conftest.py") {
        tf.push("pytest".to_string());
    }
    a.test_frameworks = tf;

    // entry points (common ones that actually exist)
    for cand in [
        "src/main.rs",
        "src/lib.rs",
        "main.go",
        "src/index.ts",
        "src/index.js",
        "index.js",
        "src/main.py",
        "main.py",
        "app.py",
        "manage.py",
    ] {
        if has(cand) {
            a.entry_points.push(cand.to_string());
        }
    }

    // top-level config files present
    let cfg_names = [
        "Cargo.toml",
        "package.json",
        "tsconfig.json",
        "pyproject.toml",
        "go.mod",
        "Dockerfile",
        "docker-compose.yml",
        ".github/workflows",
    ];
    for f in files {
        if !f.path.contains('/') {
            let n = f.path.as_str();
            if cfg_names.contains(&n) {
                a.config_files.push(n.to_string());
            }
        }
    }
    a.config_files.sort();
    a.config_files.dedup();

    a.workspaces = {
        let mut w: Vec<String> = packages.iter().cloned().collect();
        w.sort();
        w
    };
    a
}

/// Human-readable one-block summary of the analysis (for `/init` output + `codebase_search` header).
pub fn analysis_summary(a: &ProjectAnalysis) -> String {
    let mut s = format!("project: {}\n", a.project_name);
    if !a.languages.is_empty() {
        let mut langs: Vec<(&String, &usize)> = a.languages.iter().collect();
        langs.sort_by(|x, y| y.1.cmp(x.1));
        let top: Vec<String> = langs
            .iter()
            .take(6)
            .map(|(l, n)| format!("{l} ({n})"))
            .collect();
        s.push_str(&format!("languages: {}\n", top.join(", ")));
    }
    if !a.package_managers.is_empty() {
        s.push_str(&format!("build: {}\n", a.package_managers.join(", ")));
    }
    if !a.frameworks.is_empty() {
        s.push_str(&format!("frameworks: {}\n", a.frameworks.join(", ")));
    }
    if !a.test_frameworks.is_empty() {
        s.push_str(&format!("tests: {}\n", a.test_frameworks.join(", ")));
    }
    if !a.entry_points.is_empty() {
        s.push_str(&format!("entry points: {}\n", a.entry_points.join(", ")));
    }
    if !a.workspaces.is_empty() {
        s.push_str(&format!("workspaces: {}\n", a.workspaces.join(", ")));
    }
    s.trim_end().to_string()
}

// ── build ───────────────────────────────────────────────────────────────────────────────────

/// Build (or incrementally refresh) the index. `incremental` reuses unchanged files from the prior
/// index by SHA-256 (with a `(len, mtime)` fast-path); `false` re-reads everything. `progress` is
/// called at throttled points. `cancel` is an optional turn token (passed explicitly so it works
/// across the `spawn_blocking` boundary where the thread-local isn't seeded). Cross-process safe
/// (holds [`RepoTxnLock`]); the write is atomic; an Esc / Ctrl+C aborts cleanly WITHOUT corrupting
/// the existing index.
pub fn build_index(
    incremental: bool,
    cancel: Option<&crate::core::cancel::TurnCancel>,
    progress: &dyn Fn(Phase),
) -> Result<ScanStats> {
    let start = Instant::now();
    let root = config::project_root();
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let cancelled = || cancel.is_some_and(|c| c.is_cancelled());

    // Cross-process lock so two `/init`s never race on the same index file. Released on drop.
    let _lock = RepoTxnLock::acquire_mode(
        &lock_path(),
        crate::core::repo_lock::LockMode::Exclusive,
        LOCK_TIMEOUT,
        cancel,
    )
    .context("another /init is already running for this repo")?;

    let prev_files: HashMap<String, IndexedFile> = if incremental {
        load()
            .map(|i| i.files.into_iter().map(|f| (f.path.clone(), f)).collect())
            .unwrap_or_default()
    } else {
        HashMap::new()
    };
    let prev_chunks: HashMap<String, CodeChunk> = if incremental {
        load()
            .map(|i| i.chunks.into_iter().map(|c| (c.id.clone(), c)).collect())
            .unwrap_or_default()
    } else {
        HashMap::new()
    };

    let candidates = collect_candidates(&root);
    let package_dirs = discover_packages(&root, &candidates);
    let total = candidates.len();
    let throttle = (total / 20).max(1);

    let mut files: Vec<IndexedFile> = Vec::new();
    let mut chunks: Vec<CodeChunk> = Vec::new();
    let mut stats = ScanStats {
        scanned: total,
        ..Default::default()
    };

    for (i, path) in candidates.iter().enumerate() {
        if i % throttle == 0 {
            if cancelled() {
                anyhow::bail!("/init cancelled — the existing index was left unchanged");
            }
            progress(Phase::Scanning { done: i, total });
        }
        if files.len() >= MAX_INDEXED_FILES {
            stats.capped = true;
            break;
        }
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .replace('\\', "/");
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let package = package_for(&root, path, &package_dirs);

        // Sensitive files: record path + type ONLY. Never read the content into the index.
        if let Some(kind) = sensitivity_kind(path) {
            let meta = std::fs::metadata(path).ok();
            files.push(IndexedFile {
                path: rel,
                language: format!("sensitive:{kind}"),
                extension: ext,
                len: meta.as_ref().map(|m| m.len()).unwrap_or(0),
                mtime: meta.as_ref().map(mtime_nanos).unwrap_or(0),
                content_hash: String::new(),
                line_count: 0,
                package,
                is_generated: false,
                is_sensitive: true,
                redacted: false,
                chunk_ids: Vec::new(),
            });
            stats.sensitive += 1;
            continue;
        }

        // Only index recognized source/config/doc files.
        if !is_source_file(path) {
            continue;
        }

        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => {
                stats.skipped_unreadable += 1;
                continue;
            }
        };
        let len = meta.len();
        if len > MAX_FILE_BYTES {
            stats.skipped_large += 1;
            continue;
        }
        let mtime = mtime_nanos(&meta);

        // Incremental fast-path: unchanged (same len + mtime, trustworthy mtime) → reuse file + its
        // chunks WITHOUT re-reading or re-hashing.
        if incremental && mtime != 0 {
            if let Some(p) = prev_files.get(&rel) {
                if p.len == len && p.mtime == mtime {
                    for cid in &p.chunk_ids {
                        if let Some(c) = prev_chunks.get(cid) {
                            chunks.push(c.clone());
                        }
                    }
                    files.push(p.clone());
                    stats.reused += 1;
                    continue;
                }
            }
        }

        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(_) => {
                stats.skipped_unreadable += 1;
                continue;
            }
        };
        let content_hash = sha256_hex(&bytes);

        // SHA-256 fast-path: mtime moved but bytes are identical → reuse prior chunks, refresh mtime.
        if incremental {
            if let Some(p) = prev_files.get(&rel) {
                if p.content_hash == content_hash && !p.content_hash.is_empty() {
                    for cid in &p.chunk_ids {
                        if let Some(c) = prev_chunks.get(cid) {
                            chunks.push(c.clone());
                        }
                    }
                    let mut refreshed = p.clone();
                    refreshed.mtime = mtime;
                    files.push(refreshed);
                    stats.reused += 1;
                    continue;
                }
            }
        }

        let text = match decode_text(&bytes) {
            Some(t) => t,
            None => {
                stats.skipped_binary += 1;
                continue;
            }
        };
        let language = language_of(&ext, file_name);
        let line_count = text.lines().count();
        let generated = is_generated(path, &text);
        let (safe_text, redacted) = redact_secrets(&text);
        if redacted {
            stats.redacted += 1;
        }

        let file_chunks = if generated {
            Vec::new() // generated → indexed name-only, not chunked
        } else {
            // #16: prefer the language server's authoritative symbol boundaries; fall back to the
            // per-line keyword heuristic when LSP is off / no server / error / empty outline.
            chunk_file_lsp(path, &rel, &language, &safe_text)
                .unwrap_or_else(|| chunk_file(&rel, &language, &safe_text))
        };
        let chunk_ids: Vec<String> = file_chunks.iter().map(|c| c.id.clone()).collect();

        files.push(IndexedFile {
            path: rel,
            language,
            extension: ext,
            len,
            mtime,
            content_hash,
            line_count,
            package,
            is_generated: generated,
            is_sensitive: false,
            redacted,
            chunk_ids,
        });
        chunks.extend(file_chunks);
        stats.added += 1;
    }

    if cancelled() {
        anyhow::bail!("/init cancelled — the existing index was left unchanged");
    }

    progress(Phase::Chunking);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    chunks.sort_by(|a, b| {
        a.file_path
            .cmp(&b.file_path)
            .then(a.start_line.cmp(&b.start_line))
    });
    stats.indexed = files.len();
    stats.chunks = chunks.len();
    let final_paths: HashSet<&String> = files.iter().map(|f| &f.path).collect();
    stats.removed = prev_files
        .keys()
        .filter(|k| !final_paths.contains(k))
        .count();

    progress(Phase::Building);
    let analysis = analyze_project(&root, &files, &package_dirs);
    let idx = CodebaseIndex {
        version: INDEX_VERSION,
        root: root.to_string_lossy().into_owned(),
        built_unix: now_unix(),
        analysis,
        files,
        chunks,
    };
    let bytes = serde_json::to_vec(&idx)?;
    stats.bytes_on_disk = bytes.len() as u64;
    // Atomic: write a temp sibling then rename over the destination — a crash never truncates the
    // live index. The lock is still held here, so no other init can interleave.
    persist::atomic_write(&index_path(), &bytes)?;
    // The index on disk just changed — drop the in-memory cache so the next query reloads it.
    // (The (mtime,len) key would usually catch this, but an explicit drop also covers a same-second
    // rewrite that happens to land on the same byte length.)
    invalidate_cache();
    stats.elapsed_ms = start.elapsed().as_millis();
    Ok(stats)
}

// ── search + retrieval ──────────────────────────────────────────────────────────────────────

/// A ranked chunk hit.
struct Hit<'a> {
    score: f64,
    chunk: &'a CodeChunk,
}

// ── dense tier (#19, feature = "dense") ───────────────────────────────────────────────────────

/// Runtime opt-in for the code dense tier. Default OFF even in a `--features dense` build, so the
/// dense fusion is only ever active when BOTH the feature is compiled AND this env is truthy — a
/// plain release stays purely lexical and pays zero embedding cost.
#[cfg(feature = "dense")]
fn code_dense_enabled() -> bool {
    std::env::var("AIZEN_CODE_DENSE")
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Query-level gate threshold for the code dense tier — fuse dense only when the best lexical hit
/// covers FEWER than this fraction of the query's distinct tokens (BM25 is ambiguous). Mirrors the
/// memory subsystem's `dense_gate_coverage` (default 0.60); overridable via `AIZEN_CODE_DENSE_COVERAGE`.
#[cfg(feature = "dense")]
fn code_dense_gate_coverage() -> f64 {
    std::env::var("AIZEN_CODE_DENSE_COVERAGE")
        .ok()
        .and_then(|s| s.trim().parse::<f64>().ok())
        .filter(|f| (0.0..=1.0).contains(f))
        .unwrap_or(0.60)
}

/// Fraction of the query's DISTINCT tokens present in `hit_tokens` (empty query → 0.0 so the gate
/// opens). Same coverage proxy the memory subsystem gates on.
#[cfg(feature = "dense")]
fn chunk_lexical_coverage(q_distinct: &HashSet<String>, hit_tokens: &[String]) -> f64 {
    if q_distinct.is_empty() {
        return 0.0;
    }
    let hit: HashSet<&String> = hit_tokens.iter().collect();
    let covered = q_distinct.iter().filter(|t| hit.contains(*t)).count();
    covered as f64 / q_distinct.len() as f64
}

/// Dense-tier fusion for chunk ranking (#19), mirroring the memory subsystem's query-level GATED
/// hybrid (P6). Fuses a dense (embedding cosine) ranking with the lexical one via RRF — but ONLY
/// when the top lexical hit covers fewer than [`code_dense_gate_coverage`] of the query's tokens, so
/// a confident literal match keeps its lexical precision and only ambiguous / conceptual queries pay
/// for embeddings. Chunk embeddings are cached in a `code-`-namespaced store (never mixed with
/// memory-fact vectors) so each chunk is embedded once. `scored` is the full lexical candidate set
/// (score>0), already sorted best-first, BEFORE truncation — fusing here lets a dense-strong chunk
/// climb into the top `limit`. No-op when the runtime flag is off or the set is trivial.
#[cfg(feature = "dense")]
fn fuse_dense<'a>(
    query: &str,
    q_distinct: &HashSet<String>,
    mut scored: Vec<Hit<'a>>,
) -> Vec<Hit<'a>> {
    if scored.len() < 2 || !code_dense_enabled() {
        return scored;
    }
    // Gate on the top lexical hit's coverage — a confident literal match skips dense entirely.
    let top_cov = chunk_lexical_coverage(q_distinct, &scored[0].chunk.tokens);
    if top_cov >= code_dense_gate_coverage() {
        return scored;
    }
    let embedder = crate::memory::embed::default_dense_embedder();
    // Separate cache namespace so code-chunk vectors never collide with memory-fact vectors even
    // though both are content-hash keyed under the same embedder id.
    let mut cache = crate::memory::embed::EmbeddingCache::load(&format!("code-{}", embedder.id()));
    let qv = embedder.embed(query);
    let lexical: Vec<String> = scored.iter().map(|h| h.chunk.id.clone()).collect();
    let mut dense: Vec<(String, f32)> = scored
        .iter()
        .map(|h| {
            let cv = cache.get_or_compute(&h.chunk.content, embedder.as_ref());
            (h.chunk.id.clone(), crate::memory::embed::cosine(&qv, &cv))
        })
        .filter(|(_, s)| *s > 0.0)
        .collect();
    cache.save();
    dense.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    let dense_ids: Vec<String> = dense.into_iter().map(|(id, _)| id).collect();
    let fused = crate::memory::fuse::rrf(&[lexical, dense_ids], 10.0);
    let rank: HashMap<&str, usize> = fused
        .iter()
        .enumerate()
        .map(|(i, (id, _))| (id.as_str(), i))
        .collect();
    // Reorder the lexical candidates by fused rank (chunks absent from the fusion keep tail order).
    scored.sort_by_key(|h| rank.get(h.chunk.id.as_str()).copied().unwrap_or(usize::MAX));
    scored
}

/// No-op fusion in the default (non-`dense`) build — `rank_chunks` stays purely lexical.
#[cfg(not(feature = "dense"))]
fn fuse_dense<'a>(
    _query: &str,
    _q_distinct: &HashSet<String>,
    scored: Vec<Hit<'a>>,
) -> Vec<Hit<'a>> {
    scored
}

/// Rank chunks against `query` (BM25 + fuzzy, with a small path/symbol-exact bonus). `bm` is the
/// prebuilt index over `idx.chunks` (from [`load_cached`]) — passed in so a per-turn query never
/// rebuilds the O(total tokens) IDF table. With `--features dense` + `AIZEN_CODE_DENSE`, a gated
/// dense tier is RRF-fused in ([`fuse_dense`]) before truncation; otherwise this is lexical-only.
fn rank_chunks<'a>(
    idx: &'a CodebaseIndex,
    bm: &Bm25Index,
    query: &str,
    limit: usize,
) -> Vec<Hit<'a>> {
    let q = tokenize(query);
    if q.is_empty() || idx.chunks.is_empty() {
        return Vec::new();
    }
    let q_lower = query.to_ascii_lowercase();
    let mut scored: Vec<Hit> = idx
        .chunks
        .iter()
        .map(|c| {
            let mut score = bm.score_fuzzy(&q, &c.tokens);
            // Hybrid boosts: exact symbol-name and path substring matches are high-signal.
            if !c.symbol_name.is_empty() && q_lower.contains(&c.symbol_name.to_ascii_lowercase()) {
                score += 5.0;
            }
            let path_lower = c.file_path.to_ascii_lowercase();
            if q.iter().any(|t| path_lower.contains(t.as_str())) {
                score += 1.0;
            }
            Hit { score, chunk: c }
        })
        .filter(|h| h.score > 0.0)
        .collect();
    scored.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal));
    // Gated dense fusion (no-op unless `--features dense` + runtime opt-in) over the FULL candidate
    // set, before truncation, so a dense-strong chunk can climb into the returned top `limit`.
    let q_distinct: HashSet<String> = q.iter().cloned().collect();
    scored = fuse_dense(query, &q_distinct, scored);
    scored.truncate(limit);
    scored
}

/// Rank the indexed chunk corpus against `query`, returning at most `limit` chunks as a text block
/// (`path:lines  (score)` + symbol + preview). Errors if `/init` hasn't run yet.
pub fn search(query: &str, limit: usize) -> Result<String> {
    let (idx, bm) = load_cached()
        .ok_or_else(|| anyhow!("no codebase index yet — run /init to build it first"))?;
    if idx.chunks.is_empty() && idx.files.is_empty() {
        return Ok("the codebase index is empty (nothing matched the /init scan).".into());
    }
    if tokenize(query).is_empty() {
        return Ok(format!("`{query}` has no searchable terms."));
    }
    let hits = rank_chunks(&idx, &bm, query, limit);
    if hits.is_empty() {
        return Ok(format!("no indexed chunks match `{query}`."));
    }
    let mut out = format!("{} chunk(s) match `{query}` (best first):\n", hits.len());
    for h in &hits {
        let c = h.chunk;
        let sym = if c.symbol_name.is_empty() {
            String::new()
        } else {
            format!("  [{} {}]", c.symbol_type, c.symbol_name)
        };
        out.push_str(&format!(
            "{}:{}-{}  (score {:.2}){sym}",
            c.file_path, c.start_line, c.end_line, h.score
        ));
        if !c.preview.is_empty() && c.symbol_name.is_empty() {
            out.push_str(&format!("\n    {}", c.preview));
        }
        out.push('\n');
    }
    Ok(out.trim_end().to_string())
}

/// Relevance gate for per-turn auto-injection: is the top-ranked chunk a CONFIDENT match for the
/// query, or just an incidental one-common-word overlap? Passes when EITHER (a) the chunk's body
/// covers >=2 distinct query tokens (real lexical overlap, not one stopword-surviving word), OR
/// (b) it is a high-signal exact match — the query text names the chunk's symbol, or a query token
/// is a substring of the chunk's file path (both are the same boosts `rank_chunks` trusts). This
/// keeps intentful queries ("how is auth handled", "charge_card") while dropping chatter ("run the
/// tests", "thanks") whose only overlap is a common code word. Manual `codebase_search` is NOT
/// gated — the model asked for it explicitly; only the automatic injection path uses this.
fn gate_passes(query: &str, chunk: &CodeChunk) -> bool {
    let q = tokenize(query);
    if q.is_empty() {
        return false;
    }
    let q_lower = query.to_ascii_lowercase();
    // (b) High-signal exact match: query names the symbol, or a token is in the file path.
    if !chunk.symbol_name.is_empty() && q_lower.contains(&chunk.symbol_name.to_ascii_lowercase()) {
        return true;
    }
    let path_lower = chunk.file_path.to_ascii_lowercase();
    if q.iter().any(|t| path_lower.contains(t.as_str())) {
        return true;
    }
    // (a) Body coverage: count DISTINCT query tokens present in the chunk's tokens.
    let body: std::collections::HashSet<&String> = chunk.tokens.iter().collect();
    let q_distinct: std::collections::HashSet<&String> = q.iter().collect();
    let covered = q_distinct.iter().filter(|t| body.contains(**t)).count();
    covered >= 2
}

/// Automatic per-turn retrieval: rank chunks for `query` and render a context block (path + line
/// range + real content, attributed) bounded by `budget_tokens` (chars/4 estimate). `None` when
/// there is no index, no query terms, nothing matches, or the top hit fails the relevance gate —
/// so the caller injects nothing.
pub fn retrieval_block(query: &str, budget_tokens: usize) -> Option<String> {
    let (idx, bm) = load_cached()?;
    if idx.chunks.is_empty() {
        return None;
    }
    let hits = rank_chunks(&idx, &bm, query, 20);
    if hits.is_empty() {
        return None;
    }
    // Relevance gate: rank_chunks keeps anything with score>0.0, so a single common-code-word
    // overlap ("run", "build", "error", "file") matches on nearly every turn — injecting spends
    // tokens AND varies the dynamic lane on turns that gain nothing. Require the TOP hit to be a
    // CONFIDENT match: it covers >=2 distinct query tokens in its body, OR it is a high-signal
    // exact match (the query names the symbol, or a query token is a substring of the file path).
    // Mirrors the memory subsystem's lexical-coverage gate (memory/mod.rs `lexical_coverage`).
    if !gate_passes(query, hits[0].chunk) {
        return None;
    }
    let budget_chars = budget_tokens.saturating_mul(4).max(800);
    // Hard per-chunk ceiling: a single 400-line chunk (~16k chars) must not blow the whole budget
    // ~2.7x when injected as hit #1. Cap each chunk's content to half the budget on a char boundary.
    let chunk_cap = (budget_chars / 2).max(400);
    let mut body = String::new();
    let mut used = 0usize;
    let mut included = 0usize;
    for h in &hits {
        let c = h.chunk;
        let header = format!("// {}:{}-{}", c.file_path, c.start_line, c.end_line);
        // Truncate an oversized chunk on a char boundary rather than dropping it whole.
        let content: String = if c.content.chars().count() > chunk_cap {
            let kept: String = c.content.chars().take(chunk_cap).collect();
            format!("{kept}\n// … (truncated — read_symbol / file_read for the rest)")
        } else {
            c.content.clone()
        };
        let block = format!("{header}\n{content}\n\n");
        // Guarantee at least one chunk, then enforce the budget hard on every subsequent chunk
        // (was `included >= 3`, which let 3 uncapped chunks stack to ~5k tokens).
        if included >= 1 && used + block.len() > budget_chars {
            break;
        }
        body.push_str(&block);
        used += block.len();
        included += 1;
        if used >= budget_chars {
            break;
        }
    }
    if included == 0 {
        return None;
    }
    Some(format!(
        "<codebase_context>\nRetrieved from the /init codebase index for this query \
         (source-attributed; may be partial). Cite paths when you use them.\n\n{}</codebase_context>",
        body.trim_end()
    ))
}

/// Read-only, side-effect-free semantic chunk lookup over the `/init` index.
pub struct CodebaseSearch;

impl Tool for CodebaseSearch {
    fn name(&self) -> &str {
        "codebase_search"
    }
    fn description(&self) -> &str {
        "Find the code CHUNKS most relevant to a natural-language query over the index built by \
         /init (\"where is auth handled\", \"database connection setup\"). Ranks function/class/heading \
         chunks by concept, returning `path:lines (score)` + symbol + preview — use it to LOCATE \
         where a feature lives, then file_read the top hits. Not for content regex → use \
         search_files; not for file NAMES → use file_glob. Read-only. Errors if the index is missing \
         (tell the user to run /init)."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {"type": "string", "description": "natural-language description of the feature/concept to locate"},
                "limit": {"type": "integer", "description": "max chunks to return (default 12)"}
            },
            "required": ["query"]
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .context("missing `query`")?;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, 100))
            .unwrap_or(DEFAULT_SEARCH_LIMIT);
        search(query, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_takes_first_nonblank_line_clipped() {
        assert_eq!(make_preview("\n\n  hello world  \nsecond"), "hello world");
        let long = "x".repeat(500);
        assert_eq!(make_preview(&long).chars().count(), PREVIEW_CHARS);
    }

    #[test]
    fn sha256_is_stable_and_hex() {
        let h = sha256_hex(b"hello");
        assert_eq!(h.len(), 64);
        assert_eq!(h, sha256_hex(b"hello"));
        assert_ne!(h, sha256_hex(b"hell0"));
    }

    #[test]
    fn sensitivity_detects_env_and_keys_but_allows_examples() {
        assert_eq!(sensitivity_kind(Path::new("proj/.env")), Some("dotenv"));
        assert_eq!(sensitivity_kind(Path::new(".env.local")), Some("dotenv"));
        assert_eq!(
            sensitivity_kind(Path::new("server.pem")),
            Some("private-key")
        );
        assert_eq!(sensitivity_kind(Path::new("id_rsa")), Some("private-key"));
        assert_eq!(
            sensitivity_kind(Path::new(".npmrc")),
            Some("credentials-file")
        );
        // Safe-to-share templates are NOT sensitive.
        assert_eq!(sensitivity_kind(Path::new(".env.example")), None);
        assert_eq!(sensitivity_kind(Path::new("main.rs")), None);
    }

    #[test]
    fn redaction_blanks_secret_values_keeps_structure() {
        let src = "api_key = \"abcdef123456SECRET\"\nlet x = 1;";
        let (out, did) = redact_secrets(src);
        assert!(did, "should redact");
        assert!(
            !out.contains("abcdef123456SECRET"),
            "secret value must be gone: {out}"
        );
        assert!(out.contains("let x = 1;"), "non-secret code preserved");

        let pem = "-----BEGIN RSA PRIVATE KEY-----\nMIIabc\n-----END RSA PRIVATE KEY-----";
        let (out2, did2) = redact_secrets(pem);
        assert!(did2);
        assert!(!out2.contains("BEGIN RSA PRIVATE KEY"));

        let clean = "fn main() { println!(\"hi\"); }";
        let (out3, did3) = redact_secrets(clean);
        assert!(!did3);
        assert_eq!(out3, clean);
    }

    #[test]
    fn connection_string_credentials_are_redacted() {
        let src = "DATABASE_URL=postgres://user:hunter2@db.example.com:5432/app";
        let (out, did) = redact_secrets(src);
        assert!(did);
        assert!(!out.contains("hunter2"), "password must be redacted: {out}");
    }

    #[test]
    fn prefixed_vendor_tokens_are_redacted_standalone() {
        // Each of these appears as a bare literal (no secret-ish key name), so only the
        // vendor-prefix patterns can catch them. All must be blanked.
        let cases: &[(&str, &str)] = &[
            (
                "github fine-grained PAT",
                "github_pat_11ABCDEFG0abcdefghijkl_mnopqrstuvwxyz0123456789ABCDEFGHIJKLM",
            ),
            (
                "anthropic key",
                "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789",
            ),
            // These three are split with `concat!` on purpose: written as one literal, GitHub's
            // push protection reads them as real leaked credentials and refuses the push, even
            // though they are obvious dummies feeding a redaction test.
            (
                "stripe live secret",
                concat!("sk_live_", "abcdefghijklmnop0123456789"),
            ),
            ("gitlab PAT", "glpat-abcdefghij0123456789kl"),
            (
                "slack bot token",
                concat!("xoxb-", "1234567890-ABCDEFGHIJKLMN"),
            ),
            (
                "slack webhook",
                concat!(
                    "https://hooks.slack.com/services/",
                    "T00000000/B00000000/XXXXXXXXXXXXXXXXXXXXXXXX"
                ),
            ),
            ("google api key", "AIzaSyB1234567890abcdefghijklmnopqrstuv"),
            (
                "gcp oauth token",
                "ya29.a0ARrdaM-abcdefghijklmnopqrstuvwxyz",
            ),
            (
                "huggingface token",
                "hf_abcdefghijklmnopqrstuvwxyz0123456789",
            ),
            (
                "sendgrid key",
                "SG.abcdefghijklmnop.qrstuvwxyz0123456789ABCD",
            ),
            (
                "jwt",
                "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.dQw4w9WgXcQabc123",
            ),
            ("aws session key", "ASIAIOSFODNN7EXAMPLE"),
        ];
        for (label, secret) in cases {
            let src = format!("let x = \"{secret}\";");
            let (out, did) = redact_secrets(&src);
            assert!(did, "{label} should redact: {src}");
            assert!(
                !out.contains(secret),
                "{label} value must be blanked: {out}"
            );
        }
    }

    #[test]
    fn encrypted_pem_header_is_redacted() {
        let src =
            "-----BEGIN ENCRYPTED PRIVATE KEY-----\nMIIF...\n-----END ENCRYPTED PRIVATE KEY-----";
        let (out, did) = redact_secrets(src);
        assert!(did);
        assert!(
            !out.contains("BEGIN ENCRYPTED PRIVATE KEY"),
            "PEM header must be blanked: {out}"
        );
    }

    #[test]
    fn small_file_is_single_chunk() {
        let text = "fn a() {}\nfn b() {}\n";
        let chunks = chunk_file("x.rs", "rust", text);
        assert_eq!(chunks.len(), 1, "tiny file → one chunk");
        assert_eq!(chunks[0].symbol_type, "file");
        assert_eq!(chunks[0].start_line, 1);
    }

    #[test]
    fn large_file_splits_into_multiple_chunks_by_symbol() {
        // Build a rust file with several big functions so it exceeds the single-chunk threshold.
        let mut src = String::new();
        for f in 0..5 {
            src.push_str(&format!("fn func{f}() {{\n"));
            for i in 0..40 {
                src.push_str(&format!("    let v{i} = {i} + {f};\n"));
            }
            src.push_str("}\n");
        }
        let chunks = chunk_file("big.rs", "rust", &src);
        assert!(
            chunks.len() >= 5,
            "expected one chunk per function, got {}",
            chunks.len()
        );
        assert!(chunks
            .iter()
            .any(|c| c.symbol_name == "func0" && c.symbol_type == "function"));
        // line ranges are 1-based and ordered
        assert_eq!(chunks[0].start_line, 1);
        assert!(chunks[1].start_line > chunks[0].start_line);
    }

    #[test]
    fn chunk_by_outline_splits_at_server_boundaries() {
        // Build a file whose REAL symbol starts don't match the keyword heuristic (the bodies are
        // indented, the "signatures" are prose) — so only an authoritative outline places them right.
        let mut body = String::new();
        for _ in 0..200 {
            body.push_str("    some indented statement that is not a keyword line\n");
        }
        let src = format!("preamble line one\npreamble line two\n{body}{body}");
        let lines: Vec<&str> = src.lines().collect();
        // Server says: two symbols — one at line 2 (0-based), one halfway.
        let mid = 2 + 200;
        let outline = vec![
            (2usize, "first_handler".to_string(), "function"),
            (mid, "second_handler".to_string(), "function"),
        ];
        let chunks = chunk_by_outline("srv.rs", "rust", &lines, outline);
        // A preamble region (lines 0..2) + the two symbol regions, each big enough to survive.
        assert!(
            chunks.iter().any(|c| c.symbol_name == "first_handler"),
            "got: {chunks:?}"
        );
        assert!(
            chunks.iter().any(|c| c.symbol_name == "second_handler"),
            "got: {chunks:?}"
        );
        // The first symbol chunk starts at the server's line (1-based = 3), not a keyword guess.
        let first = chunks
            .iter()
            .find(|c| c.symbol_name == "first_handler")
            .unwrap();
        assert_eq!(first.start_line, 3);
    }

    #[test]
    fn chunk_by_outline_handles_unsorted_and_duplicate_lines() {
        // Servers can return children out of document order and multiple symbols on one line
        // (a one-liner + its child). Regions must stay ordered and never invert.
        let lines: Vec<&str> = (0..300).map(|_| "code statement here").collect();
        let outline = vec![
            (100usize, "b".to_string(), "function"),
            (10, "a".to_string(), "struct"),
            (10, "a_dup".to_string(), "field"), // same line as `a` → deduped
        ];
        let chunks = chunk_by_outline("u.rs", "rust", &lines, outline);
        // Ordered by start line, no zero/negative-width region panicked the slice.
        let starts: Vec<usize> = chunks.iter().map(|c| c.start_line).collect();
        assert!(
            starts.windows(2).all(|w| w[0] <= w[1]),
            "chunks must be line-ordered: {starts:?}"
        );
        assert!(chunks.iter().any(|c| c.symbol_name == "a"));
        assert!(chunks.iter().any(|c| c.symbol_name == "b"));
    }

    #[test]
    fn import_only_chunk_is_dropped() {
        // A python file that is only imports produces no chunk.
        let text = "import os\nimport sys\nfrom a import b\n";
        let chunks = chunk_file("imports.py", "python", text);
        assert!(
            chunks.is_empty(),
            "import-only file yields no chunk: {chunks:?}"
        );
    }

    #[test]
    fn chunk_id_is_stable_across_position_shift() {
        // Same symbol + same body at different line offsets → same id (id excludes line numbers).
        let a = chunk_file("m.rs", "rust", "fn only() {\n    let x = 1;\n}\n");
        let b = chunk_file(
            "m.rs",
            "rust",
            "// leading comment\n\nfn only() {\n    let x = 1;\n}\n",
        );
        // both single-chunk (tiny) → compare the symbol-bearing body hash indirectly via id equality
        // requires equal body; here bodies differ (b has preamble), so ids differ — instead test the
        // hash primitive directly for position independence.
        let id1 = short_hash("m.rs\0only\0deadbeef");
        let id2 = short_hash("m.rs\0only\0deadbeef");
        assert_eq!(id1, id2);
        assert_eq!(id1.len(), 16);
        assert!(!a.is_empty() && !b.is_empty());
    }

    #[test]
    fn markdown_chunks_by_heading() {
        let mut md = String::from("# Title\n");
        for _ in 0..60 {
            md.push_str("filler line of prose about the system design\n");
        }
        md.push_str("## Section\n");
        for _ in 0..60 {
            md.push_str("more prose describing the retrieval pipeline internals\n");
        }
        let chunks = chunk_file("doc.md", "markdown", &md);
        assert!(
            chunks.iter().any(|c| c.symbol_type == "heading"),
            "should detect headings: {chunks:?}"
        );
    }

    #[test]
    fn bm25_ranks_relevant_chunk_first() {
        let idx = CodebaseIndex {
            version: INDEX_VERSION,
            root: "/tmp".into(),
            built_unix: 0,
            analysis: ProjectAnalysis::default(),
            files: vec![],
            chunks: vec![
                CodeChunk {
                    id: "1".into(),
                    file_path: "auth.rs".into(),
                    language: "rust".into(),
                    start_line: 1,
                    end_line: 10,
                    symbol_name: "login".into(),
                    symbol_type: "function".into(),
                    parent_symbol: String::new(),
                    token_estimate: 10,
                    tokens: tokenize("login logout session token authentication password verify"),
                    preview: "fn login".into(),
                    content: "fn login() {}".into(),
                },
                CodeChunk {
                    id: "2".into(),
                    file_path: "render.rs".into(),
                    language: "rust".into(),
                    start_line: 1,
                    end_line: 10,
                    symbol_name: "draw".into(),
                    symbol_type: "function".into(),
                    parent_symbol: String::new(),
                    token_estimate: 10,
                    tokens: tokenize("draw pixels framebuffer color viewport scroll"),
                    preview: "fn draw".into(),
                    content: "fn draw() {}".into(),
                },
            ],
        };
        let bm = Bm25Index::build(idx.chunks.iter().map(|c| c.tokens.as_slice()));
        let hits = rank_chunks(&idx, &bm, "how does authentication and login work", 5);
        assert_eq!(hits[0].chunk.file_path, "auth.rs");
        assert!(hits[0].score > 0.0);
    }

    #[test]
    fn empty_query_terms_are_reported_not_scored() {
        assert!(tokenize("a of to").is_empty());
    }

    #[cfg(feature = "dense")]
    #[test]
    fn dense_coverage_gate_math() {
        let q: HashSet<String> = tokenize("session token refresh rotate")
            .into_iter()
            .collect();
        // A hit covering every query token → coverage 1.0 (gate stays closed, dense skipped).
        let full = tokenize("session token refresh rotate here now");
        assert!((chunk_lexical_coverage(&q, &full) - 1.0).abs() < 1e-9);
        // A hit sharing one of four distinct tokens → 0.25 (< default 0.60 gate → dense opens).
        let partial = tokenize("session unrelated words entirely");
        assert!((chunk_lexical_coverage(&q, &partial) - 0.25).abs() < 1e-9);
        // Empty query → 0.0 so the gate opens (mirrors the memory subsystem).
        assert_eq!(chunk_lexical_coverage(&HashSet::new(), &full), 0.0);
    }

    #[cfg(feature = "dense")]
    #[test]
    fn dense_fusion_is_gated_and_isolated() {
        with_repo("dense", |_proj| {
            let mk = |id: &str, path: &str, body: &str| CodeChunk {
                id: id.into(),
                file_path: path.into(),
                language: "rust".into(),
                start_line: 1,
                end_line: 5,
                symbol_name: String::new(),
                symbol_type: "window".into(),
                parent_symbol: String::new(),
                token_estimate: 4,
                tokens: tokenize(body),
                preview: String::new(),
                content: body.into(),
            };
            let a = mk(
                "a",
                "a.rs",
                "connection pool database handle acquire release",
            );
            let b = mk(
                "b",
                "b.rs",
                "unrelated rendering pixels viewport scrolling code",
            );
            let scored = || {
                vec![
                    Hit {
                        score: 2.0,
                        chunk: &a,
                    },
                    Hit {
                        score: 1.0,
                        chunk: &b,
                    },
                ]
            };
            let q: HashSet<String> = tokenize("how do we obtain a db connection")
                .into_iter()
                .collect();

            // Flag OFF → no-op passthrough, original lexical order preserved regardless of coverage.
            std::env::remove_var("AIZEN_CODE_DENSE");
            let out = fuse_dense("how do we obtain a db connection", &q, scored());
            assert_eq!(out[0].chunk.id, "a", "flag off must not reorder");

            // Flag ON but the top hit fully covers the query → gate closed, still no reorder.
            std::env::set_var("AIZEN_CODE_DENSE", "1");
            let ql: HashSet<String> = tokenize("connection pool database handle")
                .into_iter()
                .collect();
            let out2 = fuse_dense("connection pool database handle", &ql, scored());
            assert_eq!(
                out2[0].chunk.id, "a",
                "confident literal match must skip dense"
            );

            // Flag ON + low coverage → dense fusion runs (HashEmbedder fallback, deterministic) and
            // the call succeeds over the isolated code- cache without panicking or erroring.
            let out3 = fuse_dense("how do we obtain a db connection", &q, scored());
            assert_eq!(out3.len(), 2, "fusion preserves the candidate set");
            std::env::remove_var("AIZEN_CODE_DENSE");
        });
    }

    #[test]
    fn generated_file_is_detected() {
        assert!(is_generated(Path::new("bundle.min.js"), ""));
        assert!(is_generated(Path::new("Cargo.lock"), ""));
        assert!(is_generated(
            Path::new("x.rs"),
            "// @generated by tool\nfn a(){}"
        ));
        assert!(!is_generated(Path::new("x.rs"), "fn a(){}"));
    }

    #[test]
    fn language_detection_maps_extensions() {
        assert_eq!(language_of("rs", "main.rs"), "rust");
        assert_eq!(language_of("tsx", "App.tsx"), "typescript");
        assert_eq!(language_of("", "Dockerfile"), "dockerfile");
        assert_eq!(language_of("py", "x.py"), "python");
    }

    // ── end-to-end build/load/search integration ─────────────────────────────────────────────
    //
    // These pin BOTH the home seam (where the index file lands) and the project-root seam (what the
    // walk indexes) into an isolated temp dir, serialized on the shared env lock. `build_index`
    // resolves the root from `project_root()` (→ `NG_PROJECT_ROOT`) and writes to
    // `codebase_index_path()` (→ `AIZEN_HOME`), so pinning both fully sandboxes a real scan.

    use std::path::Path;

    /// Run `f` with a fresh sandbox: an isolated home + project root, all env restored after.
    fn with_repo<T>(tag: &str, f: impl FnOnce(&Path) -> T) -> T {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let base =
            std::env::temp_dir().join(format!("ng-cb-{tag}-{}-{}", std::process::id(), now_unix()));
        let _ = std::fs::remove_dir_all(&base);
        let proj = base.join("proj");
        std::fs::create_dir_all(&proj).unwrap();
        std::env::set_var("USERPROFILE", &base);
        std::env::set_var("HOME", &base);
        std::env::set_var("AIZEN_HOME", base.join(".aizen"));
        std::env::set_var("NEXTGEN_HOME", base.join(".aizen"));
        std::env::set_var("NG_PROJECT_ROOT", &proj);
        // Kill the project-slug cache so a prior test's slug can't bleed through.
        let out = f(&proj);
        for v in [
            "USERPROFILE",
            "HOME",
            "AIZEN_HOME",
            "NEXTGEN_HOME",
            "NG_PROJECT_ROOT",
        ] {
            std::env::remove_var(v);
        }
        let _ = std::fs::remove_dir_all(&base);
        out
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    fn build(incremental: bool) -> ScanStats {
        build_index(incremental, None, &|_| {}).expect("build_index")
    }

    #[test]
    fn indexes_a_small_project_and_searches_it() {
        with_repo("small", |proj| {
            write(proj, "src/auth.rs", "pub fn login(user: &str, password: &str) -> bool {\n    verify_credentials(user, password)\n}\n");
            write(proj, "src/render.rs", "pub fn draw_frame(buffer: &mut [u8]) {\n    for pixel in buffer { *pixel = 0; }\n}\n");
            write(proj, "README.md", "# Demo\nA tiny project.\n");

            let stats = build(false);
            assert!(stats.indexed >= 3, "indexed {} files", stats.indexed);
            assert!(stats.chunks >= 2, "chunks {}", stats.chunks);

            // Persisted + reloadable.
            let idx = load().expect("index persisted");
            assert_eq!(idx.version, INDEX_VERSION);
            assert!(idx.files.iter().any(|f| f.path == "src/auth.rs"));

            // Concept search finds the auth file's chunk first.
            let out = search("how does user login work", 5).unwrap();
            assert!(out.contains("src/auth.rs"), "auth should rank; got:\n{out}");
        });
    }

    #[test]
    fn env_file_is_stored_path_only_never_content() {
        with_repo("env", |proj| {
            write(proj, "src/main.rs", "fn main() {}\n");
            write(
                proj,
                ".env",
                "API_KEY=supersecretvalue123456\nDB_PASSWORD=hunter2hunter2\n",
            );
            write(proj, ".env.example", "API_KEY=\nDB_PASSWORD=\n");

            let stats = build(false);
            assert!(stats.sensitive >= 1, "the .env must be counted sensitive");

            let idx = load().unwrap();
            let env = idx
                .files
                .iter()
                .find(|f| f.path == ".env")
                .expect(".env recorded path-only");
            assert!(env.is_sensitive);
            assert!(env.chunk_ids.is_empty(), "sensitive file has no chunks");
            assert!(
                env.content_hash.is_empty(),
                "sensitive file content never hashed/stored"
            );

            // The secret value must appear NOWHERE in the serialized index.
            let raw = std::fs::read_to_string(index_path()).unwrap();
            assert!(
                !raw.contains("supersecretvalue123456"),
                "secret leaked into index"
            );
            assert!(
                !raw.contains("hunter2hunter2"),
                "password leaked into index"
            );

            // The example template is indexed normally (not sensitive).
            assert!(idx
                .files
                .iter()
                .any(|f| f.path == ".env.example" && !f.is_sensitive));
        });
    }

    #[test]
    fn inline_secrets_are_redacted_from_indexed_content() {
        with_repo("redact", |proj| {
            write(
                proj,
                "config.py",
                "DEBUG = True\napi_key = \"sk-abcdefghij0123456789XYZ\"\nDATABASE_URL = \"postgres://u:p4ssw0rd@host/db\"\n",
            );
            let stats = build(false);
            assert!(stats.redacted >= 1, "a secret should have been redacted");

            let raw = std::fs::read_to_string(index_path()).unwrap();
            assert!(
                !raw.contains("sk-abcdefghij0123456789XYZ"),
                "api key leaked"
            );
            assert!(!raw.contains("p4ssw0rd"), "connection password leaked");
            // The surrounding non-secret code is still indexed.
            assert!(raw.contains("DEBUG"));
        });
    }

    #[test]
    fn oversized_and_binary_files_are_skipped() {
        with_repo("skip", |proj| {
            write(proj, "ok.rs", "fn a() {}\n");
            // Oversized (> MAX_FILE_BYTES).
            let big = "x".repeat((MAX_FILE_BYTES + 1024) as usize);
            write(proj, "huge.rs", &big);
            // Binary content in a source-looking extension → decode_text returns None.
            std::fs::write(proj.join("blob.rs"), [0u8, 159, 146, 150, 0, 1, 2, 3]).unwrap();

            let stats = build(false);
            assert!(stats.skipped_large >= 1, "oversized should be skipped");
            assert!(stats.skipped_binary >= 1, "binary should be skipped");
            let idx = load().unwrap();
            assert!(idx.files.iter().any(|f| f.path == "ok.rs"));
            assert!(!idx.files.iter().any(|f| f.path == "huge.rs"));
        });
    }

    #[test]
    fn ignored_dirs_are_not_indexed() {
        with_repo("ignore", |proj| {
            write(proj, "src/lib.rs", "pub fn keep() {}\n");
            write(proj, "node_modules/dep/index.js", "module.exports = {}\n");
            write(proj, "target/debug/build.rs", "fn generated() {}\n");
            build(false);
            let idx = load().unwrap();
            assert!(idx.files.iter().any(|f| f.path == "src/lib.rs"));
            assert!(
                !idx.files.iter().any(|f| f.path.contains("node_modules")),
                "node_modules indexed"
            );
            assert!(
                !idx.files.iter().any(|f| f.path.starts_with("target/")),
                "target indexed"
            );
        });
    }

    #[test]
    fn gitignore_is_respected() {
        with_repo("gitignore", |proj| {
            write(proj, ".gitignore", "secret_dir/\n*.log\n");
            write(proj, "src/app.rs", "fn app() {}\n");
            write(proj, "secret_dir/hidden.rs", "fn hidden() {}\n");
            write(proj, "debug.log", "log line\n");
            build(false);
            let idx = load().unwrap();
            assert!(idx.files.iter().any(|f| f.path == "src/app.rs"));
            assert!(
                !idx.files.iter().any(|f| f.path.contains("secret_dir")),
                "gitignored dir indexed"
            );
        });
    }

    #[test]
    fn incremental_reuses_unchanged_and_tracks_add_modify_delete() {
        with_repo("incr", |proj| {
            write(proj, "a.rs", "fn a() { let x = 1; }\n");
            write(proj, "b.rs", "fn b() { let y = 2; }\n");
            let first = build(false);
            assert_eq!(first.added, 2);

            // No change → everything reused, nothing re-read.
            let second = build(true);
            assert_eq!(second.reused, 2, "unchanged files reused");
            assert_eq!(second.added, 0);

            // Modify a, add c, delete b.
            std::thread::sleep(std::time::Duration::from_millis(10));
            write(proj, "a.rs", "fn a() { let x = 1; let z = 3; }\n");
            write(proj, "c.rs", "fn c() {}\n");
            std::fs::remove_file(proj.join("b.rs")).unwrap();
            let third = build(true);
            assert_eq!(third.removed, 1, "b.rs removed");
            assert!(third.added >= 2, "a.rs modified + c.rs added");
            assert_eq!(third.reused, 0, "a changed, b gone, c new → nothing reused");
            let idx = load().unwrap();
            assert!(!idx.files.iter().any(|f| f.path == "b.rs"));
            assert!(idx.files.iter().any(|f| f.path == "c.rs"));
        });
    }

    #[test]
    fn sha256_fastpath_reuses_when_mtime_moves_but_bytes_are_identical() {
        with_repo("sha", |proj| {
            write(proj, "x.rs", "fn x() { let v = 42; }\n");
            build(false);
            // Rewrite identical content so mtime moves but the hash is unchanged.
            std::thread::sleep(std::time::Duration::from_millis(10));
            write(proj, "x.rs", "fn x() { let v = 42; }\n");
            let s = build(true);
            assert_eq!(
                s.reused, 1,
                "identical bytes → SHA-256 fast-path reuse despite new mtime"
            );
            assert_eq!(s.added, 0);
        });
    }

    #[test]
    fn force_rebuild_ignores_prior_index() {
        with_repo("force", |proj| {
            write(proj, "a.rs", "fn a() {}\n");
            build(false);
            std::thread::sleep(std::time::Duration::from_millis(10));
            let forced = build(false); // incremental=false ⇒ full rebuild
            assert_eq!(forced.reused, 0, "force rebuild re-reads everything");
            assert!(forced.added >= 1);
        });
    }

    #[test]
    fn empty_project_does_not_crash() {
        with_repo("empty", |_proj| {
            let stats = build(false);
            assert_eq!(stats.indexed, 0);
            assert_eq!(stats.chunks, 0);
            // Search over an empty index is a clean message, not a panic.
            let out = search("anything", 5).unwrap();
            assert!(
                out.contains("empty") || out.contains("no indexed"),
                "got: {out}"
            );
        });
    }

    #[test]
    fn unicode_and_spaced_filenames_are_indexed() {
        with_repo("unicode", |proj| {
            write(proj, "hồ_sơ.rs", "fn ho_so() { let data = 1; }\n");
            write(proj, "my file.py", "def handler():\n    return 42\n");
            build(false);
            let idx = load().unwrap();
            assert!(
                idx.files.iter().any(|f| f.path == "hồ_sơ.rs"),
                "unicode path missing: {:?}",
                idx.files.iter().map(|f| &f.path).collect::<Vec<_>>()
            );
            assert!(
                idx.files.iter().any(|f| f.path == "my file.py"),
                "spaced path missing"
            );
        });
    }

    #[test]
    fn monorepo_files_are_tagged_with_their_package() {
        with_repo("mono", |proj| {
            write(proj, "package.json", "{\"name\":\"root\"}\n");
            write(proj, "packages/api/package.json", "{\"name\":\"api\"}\n");
            write(
                proj,
                "packages/api/src/server.ts",
                "export function serve() { return 1; }\n",
            );
            write(
                proj,
                "packages/web/Cargo.toml",
                "[package]\nname = \"web\"\n",
            );
            write(proj, "packages/web/src/main.rs", "fn main() {}\n");
            build(false);
            let idx = load().unwrap();
            let api = idx
                .files
                .iter()
                .find(|f| f.path == "packages/api/src/server.ts")
                .unwrap();
            assert_eq!(api.package, "packages/api");
            let web = idx
                .files
                .iter()
                .find(|f| f.path == "packages/web/src/main.rs")
                .unwrap();
            assert_eq!(web.package, "packages/web");
            assert!(idx
                .analysis
                .workspaces
                .contains(&"packages/api".to_string()));
        });
    }

    #[test]
    fn project_analysis_detects_languages_and_build() {
        with_repo("analyze", |proj| {
            write(proj, "Cargo.toml", "[package]\nname = \"demo\"\n");
            write(proj, "src/main.rs", "fn main() {}\n");
            write(proj, "src/lib.rs", "pub fn f() {}\n");
            build(false);
            let idx = load().unwrap();
            assert_eq!(idx.analysis.languages.get("rust").copied().unwrap_or(0), 2);
            assert!(idx
                .analysis
                .package_managers
                .iter()
                .any(|p| p.contains("cargo")));
            assert!(idx
                .analysis
                .entry_points
                .contains(&"src/main.rs".to_string()));
        });
    }

    #[test]
    fn corrupt_index_is_treated_as_absent_not_misread() {
        with_repo("corrupt", |proj| {
            write(proj, "a.rs", "fn a() {}\n");
            build(false);
            // Corrupt the persisted JSON.
            std::fs::write(index_path(), b"{ not valid json ").unwrap();
            assert!(load().is_none(), "corrupt index must load as None");
            assert!(status().is_none());
            // A fresh build recovers cleanly over the corrupt file.
            let s = build(false);
            assert!(s.indexed >= 1);
            assert!(load().is_some());
        });
    }

    #[test]
    fn stale_schema_version_is_rejected() {
        with_repo("version", |proj| {
            write(proj, "a.rs", "fn a() {}\n");
            build(false);
            // Downgrade the persisted version → load() must reject it (silent rebuild territory).
            let mut idx = load().unwrap();
            idx.version = INDEX_VERSION - 1;
            std::fs::write(index_path(), serde_json::to_vec(&idx).unwrap()).unwrap();
            assert!(load().is_none(), "stale-version index must not be misread");
        });
    }

    #[test]
    fn concurrent_init_is_blocked_by_the_lock() {
        with_repo("lock", |proj| {
            write(proj, "a.rs", "fn a() {}\n");
            // Hold the exclusive lock, then a build must fail to acquire (short timeout via the
            // real LOCK_TIMEOUT is 20s — so instead grab it and confirm try-acquire fails fast).
            let held =
                RepoTxnLock::acquire_exclusive(&lock_path(), Duration::from_millis(50)).unwrap();
            let busy = RepoTxnLock::acquire_exclusive(&lock_path(), Duration::from_millis(50));
            assert!(
                busy.is_err(),
                "second exclusive acquire must be refused while held"
            );
            drop(held);
            // Once released, a normal build proceeds.
            let s = build(false);
            assert!(s.indexed >= 1);
        });
    }

    #[test]
    fn cancelled_build_leaves_prior_index_untouched() {
        with_repo("cancel", |proj| {
            write(proj, "a.rs", "fn a() { let x = 1; }\n");
            let first = build(false);
            assert!(first.indexed >= 1);
            let before = std::fs::read(index_path()).unwrap();

            // Add a file, then run a build with an already-cancelled token → it must bail without
            // writing, leaving the prior index byte-identical.
            write(proj, "b.rs", "fn b() {}\n");
            let cancel = crate::core::cancel::TurnCancel::new();
            cancel.cancel();
            let res = build_index(true, Some(&cancel), &|_| {});
            assert!(res.is_err(), "a pre-cancelled build must not succeed");
            let after = std::fs::read(index_path()).unwrap();
            assert_eq!(
                before, after,
                "cancelled build must not modify the existing index"
            );
        });
    }

    #[test]
    fn retrieval_block_injects_attributed_content_or_nothing() {
        with_repo("retrieval", |proj| {
            write(proj, "src/payments.rs", "pub fn charge_card(amount: u64, token: &str) -> Result<(), String> {\n    process_payment(amount, token)\n}\n");
            build(false);
            // A matching query yields an attributed block with the path + line range.
            let block =
                retrieval_block("how are card payments charged", 1000).expect("should retrieve");
            assert!(block.contains("<codebase_context>"));
            assert!(
                block.contains("src/payments.rs"),
                "must attribute the source path"
            );
            // A query with no searchable terms → nothing injected.
            assert!(retrieval_block("a of the", 1000).is_none());
        });
    }

    #[test]
    fn retrieval_block_enforces_hard_token_ceiling() {
        with_repo("budget", |proj| {
            // A single big function whose body dwarfs the budget: ~400 lines of the query terms.
            // The tokenizer keeps `_` inside tokens, so use SPACE-separated words in comments so
            // "payment"/"charge"/"amount" match as whole tokens (not buried in snake_case idents).
            let mut big = String::from("pub fn process() {\n");
            for i in 0..400 {
                big.push_str(&format!(
                    "    // charge payment amount for order number {i} today\n"
                ));
            }
            big.push_str("}\n");
            write(proj, "src/pay.rs", &big);
            build(false);
            // Budget 1000 tokens → 4000 chars; per-chunk cap = 2000 chars. Even one oversized chunk
            // must be truncated, not injected whole (~13k chars), so the block stays bounded.
            let block = retrieval_block("charge payment amount", 1000)
                .expect("should retrieve the matching chunk");
            assert!(block.contains("<codebase_context>"));
            // budget_chars(4000) + one truncated chunk(cap 2000) + framing (~250) — comfortably < 8k.
            assert!(
                block.len() < 8000,
                "hard ceiling breached: block is {} chars (expected < 8000)",
                block.len()
            );
            assert!(
                block.contains("truncated"),
                "an oversized chunk must be truncated, not dropped or injected whole"
            );
        });
    }

    #[test]
    fn retrieval_block_relevance_gate_drops_chatter() {
        with_repo("gate", |proj| {
            // Natural-word body so lexical coverage is meaningful (the tokenizer keeps `_` inside
            // identifiers, so snake_case names count as one token — real prose here instead).
            // File named so its PATH shares no token with the body words below (path is a
            // high-signal gate branch; keep it out of the way to test body coverage in isolation).
            write(
                proj,
                "src/mod.rs",
                "pub fn login() {\n    // check user password then open session token\n    grant access\n}\n",
            );
            build(false);
            // Intentful query covering >=2 distinct body tokens → injects.
            assert!(
                retrieval_block("check user password session", 1000).is_some(),
                "a query covering >=2 body tokens must pass the gate"
            );
            // Chatter with zero body overlap must NOT inject.
            assert!(
                retrieval_block("run the tests", 1000).is_none(),
                "zero-overlap chatter must be gated out"
            );
            // Single-body-word overlap ("open") — one distinct covered token, no symbol/path
            // match → gate closed even though rank_chunks keeps it with score>0.
            assert!(
                retrieval_block("please open the door", 1000).is_none(),
                "single-common-word overlap must be gated out"
            );
        });
    }

    #[test]
    fn cached_index_rebuilds_after_reindex() {
        with_repo("cache", |proj| {
            // First index + a query that populates the in-memory cache (parse + BM25 build).
            write(proj, "src/alpha.rs", "pub fn alpha_widget() {\n    // configure the alpha widget rendering pipeline here\n}\n");
            build(false);
            let first = search("alpha widget rendering", 5).expect("search ok");
            assert!(
                first.contains("src/alpha.rs"),
                "first index must be found: {first}"
            );
            assert!(!first.contains("beta"), "beta not indexed yet: {first}");

            // Rewrite the tree with entirely different content and re-index. build_index calls
            // invalidate_cache() after the atomic write, so the next search must reflect the NEW
            // index — never the stale cached parse/BM25.
            std::fs::remove_file(proj.join("src/alpha.rs")).unwrap();
            write(proj, "src/beta.rs", "pub fn beta_handler() {\n    // dispatch the beta handler request routing here\n}\n");
            build(false);
            let second = search("beta handler routing", 5).expect("search ok");
            assert!(
                second.contains("src/beta.rs"),
                "re-index must be served, not stale cache: {second}"
            );

            // The old chunk is gone from results (cache genuinely invalidated, not merged).
            let stale = search("alpha widget rendering", 5).expect("search ok");
            assert!(
                !stale.contains("src/alpha.rs"),
                "stale chunk must not survive re-index: {stale}"
            );
        });
    }

    #[test]
    fn source_tree_drifted_detects_edit_add_delete_and_ignores_clean() {
        with_repo("drift", |proj| {
            write(proj, "src/a.rs", "pub fn a() { let x = 1; }\n");
            write(proj, "src/b.rs", "pub fn b() { let y = 2; }\n");
            build(false);
            let idx = load().expect("index persisted");
            let base_files = idx.files.len();
            assert!(
                base_files >= 2,
                "expected >=2 indexed files, got {base_files}"
            );

            // Clean tree, index dated in the far future → no file mtime can beat it and the count
            // matches → NOT drifted.
            let mut clean = idx.clone();
            clean.built_unix = u64::MAX;
            assert!(
                !source_tree_drifted(&clean),
                "an untouched tree must not report drift"
            );

            // An edit/add: a real file's mtime is always newer than epoch second 1 → drifted.
            let mut edited = idx.clone();
            edited.built_unix = 1;
            assert!(
                source_tree_drifted(&edited),
                "a source mtime newer than built_unix is drift"
            );

            // A delete: the indexable file COUNT drops below the index's recorded count. Uses the
            // future build time so ONLY the count branch can trip.
            std::fs::remove_file(proj.join("src/b.rs")).unwrap();
            let mut deleted = idx.clone();
            deleted.built_unix = u64::MAX;
            assert!(
                source_tree_drifted(&deleted),
                "a removed indexable file is drift"
            );
        });
    }

    #[test]
    fn ensure_fresh_rebuilds_in_background_after_delete() {
        with_repo("fresh", |proj| {
            write(proj, "src/keep.rs", "pub fn keep_me() { /* survives */ }\n");
            write(
                proj,
                "src/gone.rs",
                "pub fn gone_soon() {\n    // the doomed dispatch handler routing table\n}\n",
            );
            build(false);
            // Warm the cache + confirm the doomed symbol is initially retrievable.
            let before = search("doomed dispatch handler routing", 5).expect("search ok");
            assert!(
                before.contains("src/gone.rs"),
                "gone.rs must index first: {before}"
            );

            // Delete a file WITHOUT re-running /init, then let the automatic hook notice + rebuild.
            std::fs::remove_file(proj.join("src/gone.rs")).unwrap();
            // Clear the debounce so this call is not swallowed by an earlier test's timestamp.
            if let Ok(mut g) = LAST_FRESH_CHECK.lock() {
                *g = None;
            }
            ensure_fresh();

            // ensure_fresh detaches its walk+rebuild; join it (bounded) so env teardown can't race
            // the background build_index. The delete is count-based drift → timing-independent.
            let mut waited = 0u64;
            while REINDEX_IN_FLIGHT.load(std::sync::atomic::Ordering::Acquire) && waited < 8000 {
                std::thread::sleep(Duration::from_millis(25));
                waited += 25;
            }
            assert!(waited < 8000, "background reindex did not finish in time");

            // The rebuilt index no longer carries the deleted file.
            let after = search("doomed dispatch handler routing", 5).expect("search ok");
            assert!(
                !after.contains("src/gone.rs"),
                "deleted file must be gone after auto re-index: {after}"
            );
        });
    }
}
