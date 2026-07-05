//! Path + settings resolution for the standalone CLI memory brain.
//!
//! Memory is the CLI's OWN, under `~/.aizen/cli-memory/` — no byte-for-byte
//! interop requirement with the VS Code extension (owner decision 2026-06-20).
//! The home root is `~/.aizen` (renamed from the pre-rebrand `~/.nextgen`, which is
//! auto-migrated on first run so an upgrading user keeps all their data).

use std::path::{Path, PathBuf};

/// One process-wide lock for tests that mutate the global home env var. Tests run in parallel
/// across modules, so a per-module mutex isn't enough — they'd race on the same env var. Every
/// home-mutating test must hold THIS lock.
#[cfg(test)]
pub(crate) static TEST_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Resolve the home root: `AIZEN_HOME` (legacy `NEXTGEN_HOME`) else `USERPROFILE`/`HOME`/cwd +
/// `/.aizen`. When no env override is set, a pre-rebrand `~/.nextgen` is migrated to `~/.aizen`
/// on first use (atomic same-parent rename) so memory/personas/soul/config carry over. The fn
/// name stays `nextgen_home` (internal; called everywhere) — only the path + brand changed.
pub fn nextgen_home() -> PathBuf {
    for var in ["AIZEN_HOME", "NEXTGEN_HOME"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() {
                return PathBuf::from(v); // explicit override (also how tests pin a temp home) — verbatim, no migration
            }
        }
    }
    let home = std::env::var("USERPROFILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("HOME").ok().filter(|s| !s.trim().is_empty()))
        .unwrap_or_else(|| ".".to_string());
    resolve_default_home(Path::new(&home))
}

/// Prefer `~/.aizen`; if it doesn't exist yet but a legacy `~/.nextgen` does, migrate it (rename).
/// If the rename is blocked (e.g. a locked file), keep using the legacy dir so data is never lost.
fn resolve_default_home(base: &Path) -> PathBuf {
    let aizen = base.join(".aizen");
    if aizen.exists() {
        return aizen;
    }
    let legacy = base.join(".nextgen");
    if legacy.is_dir() {
        if std::fs::rename(&legacy, &aizen).is_ok() {
            return aizen;
        }
        return legacy; // couldn't migrate → keep the user's existing data dir
    }
    aizen
}

/// The project root for PROJECT-LOCAL customization (`./.nextgen/`): the git repo top-level if we're
/// in one, else the current dir. Repo-root-aware so launching `ng` from a SUBDIR still finds the
/// repo's `.nextgen/` (R4 — fixes the cwd-relative footgun). `NG_PROJECT_ROOT` overrides (tests + an
/// escape hatch). Shell-out to git keeps the pure-static posture (no git2/gix).
pub fn project_root() -> PathBuf {
    if let Ok(v) = std::env::var("NG_PROJECT_ROOT") {
        let v = v.trim();
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    if let Ok(out) = std::process::Command::new("git").args(["rev-parse", "--show-toplevel"]).output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return PathBuf::from(s);
            }
        }
    }
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

/// Workspace-scoping kill-switch: `NG_NO_SCOPE=1` collapses memory back to one global pool
/// (every scope filter passes everything). Escape hatch, not a config field — scoping is the
/// intended default and this exists only to debug/compare.
pub fn scope_disabled() -> bool {
    matches!(
        std::env::var("NG_NO_SCOPE").ok().as_deref().map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// FNV-1a 64-bit — tiny local hash for the project-slug stable key (core must not depend on
/// `memory::embed`; 8 lines beats a crate).
fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// File/frontmatter-safe slug fragment: lowercase alnum, runs of the rest collapse to one `-`.
fn slug_fragment(name: &str) -> String {
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
        "project".to_string()
    } else {
        s.chars().take(24).collect()
    }
}

/// The stable identity of the current workspace: `dirname-hex8`. The hash key prefers the git
/// `remote.origin.url` (a re-clone / worktree / moved checkout keeps the SAME memory zone), falling
/// back to the canonical root path (non-repo dirs still get a stable zone). Cached per
/// (`NG_PROJECT_ROOT` env, cwd) so production pays the git shell-outs once and tests that repoint
/// `NG_PROJECT_ROOT` are never served a stale slug.
pub fn project_slug() -> String {
    static CACHE: std::sync::Mutex<Option<(String, String)>> = std::sync::Mutex::new(None);
    let cache_key = format!(
        "{}|{}",
        std::env::var("NG_PROJECT_ROOT").unwrap_or_default(),
        std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default()
    );
    if let Ok(guard) = CACHE.lock() {
        if let Some((k, slug)) = guard.as_ref() {
            if *k == cache_key {
                return slug.clone();
            }
        }
    }
    let root = project_root();
    let stable_key = git_remote_origin(&root).unwrap_or_else(|| {
        std::fs::canonicalize(&root)
            .unwrap_or_else(|_| root.clone())
            .display()
            .to_string()
    });
    let name = root.file_name().and_then(|s| s.to_str()).unwrap_or("project");
    let slug = format!("{}-{:08x}", slug_fragment(name), fnv1a64(&stable_key) as u32);
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((cache_key, slug.clone()));
    }
    slug
}

/// `git config --get remote.origin.url` in `root` — `None` when not a repo / no remote / no git.
fn git_remote_origin(root: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["config", "--get", "remote.origin.url"])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Where inside the project the user is working right now: cwd relative to the project root,
/// `/`-normalized. `None` at the root (or outside it) — only a real subdir is a "region".
pub fn current_subpath() -> Option<String> {
    let cwd = std::env::current_dir().ok()?;
    let rel = cwd.strip_prefix(project_root()).ok()?;
    let s = rel.to_string_lossy().replace('\\', "/");
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// The project-local customization dir a cloned repo ships (skills / mcp.json / personas /
/// commands), merged OVER the home dir by each resolver. Prefers `<root>/.aizen`; falls back to a
/// legacy `<root>/.nextgen` when only that exists (so repos predating the rebrand still work).
pub fn project_nextgen_dir() -> PathBuf {
    let root = project_root();
    let aizen = root.join(".aizen");
    if aizen.exists() {
        return aizen;
    }
    let legacy = root.join(".nextgen");
    if legacy.exists() {
        return legacy;
    }
    aizen // default to the new name
}

/// Tighten a freshly-written secret-bearing file to owner-only (0600) on Unix; a no-op on Windows
/// (where the user-profile ACL governs). Best-effort — never fails the caller. Mirrors the 0600
/// discipline the OAuth/MCP token caches already use, applied to every long-lived secret store.
#[cfg(unix)]
pub fn harden_file(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}
#[cfg(not(unix))]
pub fn harden_file(_path: &Path) {}

/// Tighten a directory holding secrets to owner-only (0700) on Unix; a no-op on Windows.
#[cfg(unix)]
pub fn harden_dir(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
}
#[cfg(not(unix))]
pub fn harden_dir(_path: &Path) {}

/// The CLI-owned memory directory. The markdown files here are the source of truth.
pub fn cli_memory_dir() -> PathBuf {
    nextgen_home().join("cli-memory")
}

/// Long-tail entry store (one fact per `*.md`).
pub fn entries_dir() -> PathBuf {
    cli_memory_dir().join("entries")
}

/// The always-on user-style profile rendered into the frozen core.
pub fn style_path() -> PathBuf {
    cli_memory_dir().join("STYLE.md")
}

/// Mid-confidence learned candidates land here for `ng memory review` (P3).
pub fn review_dir() -> PathBuf {
    cli_memory_dir().join("review")
}

/// Recoverable archive of evicted / superseded rows (P4).
pub fn archive_dir() -> PathBuf {
    cli_memory_dir().join("archive")
}

/// Cached embeddings (P5), keyed by content hash.
pub fn embed_cache_dir() -> PathBuf {
    cli_memory_dir().join("embed-cache")
}

/// Local model store (P5 dense backend). Shared `~/.aizen/models/` (NOT under cli-memory)
/// so other Aizen tooling can reuse a downloaded model. Consumed by the `dense` feature build.
#[allow(dead_code)] // used by `--features dense` (model2vec loader); inert in the default build
pub fn models_dir() -> PathBuf {
    nextgen_home().join("models")
}

/// The dense embedding model name (a subdir of `models_dir()`), overridable via env.
#[allow(dead_code)] // used by `--features dense` (model2vec loader); inert in the default build
pub fn embed_model_name() -> String {
    std::env::var("NG_EMBED_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "potion-multilingual-128M".to_string())
}

/// Verified-good defaults adopted from the retrieval/anti-bloat research.
/// These are *starting points* the bench can retune — not a binding contract.
#[derive(Debug, Clone)]
pub struct MemorySettings {
    /// Frozen-core hard cap (tokens, chars/4 estimate). Prefix-stability budget.
    pub frozen_core_max_tokens: usize,
    // Reserved tuning knobs: callers currently pass `k` per-call and `lexical_score_tokens` uses the
    // research-fixed 0.7/0.3 blend, so these defaults aren't read yet (kept for the bench grid-search).
    /// Lazy `search_memory` injection.
    #[allow(dead_code)]
    pub search_top_k: usize,
    /// Hard output budget of one `memory_search` tool call (chars/4 estimate).
    pub search_max_tokens: usize,
    /// Lexical blend weights.
    #[allow(dead_code)]
    pub w_jaccard: f64,
    #[allow(dead_code)]
    pub w_tf: f64,
    /// Recency half-life (days) for the learned-store decay (exp(-age/30)).
    pub recency_half_life_days: f64,
    // ── learning pipeline (P3) ──
    /// Below this confidence a candidate is dropped entirely.
    pub learn_min_confidence: f64,
    /// `[learn_min_confidence, learn_store_confidence)` → review queue (human gate).
    pub learn_store_confidence: f64,
    /// Style candidates at/above this confidence are eligible for core promotion (gated by confirm).
    pub learn_core_confidence: f64,
    /// Lexical near-duplicate threshold for consolidation (UPDATE-reinforce instead of ADD).
    /// Reworded restatements of one fact land ~0.78–1.0; different-topic facts ≤~0.55,
    /// so this cleanly absorbs dups without false-merging distinct facts. Paraphrase-level
    /// and contradiction handling are P5 (cosine) / P4 (supersession).
    pub learn_dedup_threshold: f64,
    /// MinHash near-duplicate threshold (char-5-gram Jaccard) for the write-path dedup guard.
    pub minhash_dup_threshold: f64,
    // ── anti-bloat (P4) ──
    /// Max INFERRED active facts before the LRU cap archives the oldest. Curated facts exempt.
    pub learn_inferred_cap: usize,
    // ── dense tier (P5) ──
    /// Reciprocal Rank Fusion constant (bench-tuned 10, not the literature default 60).
    pub rrf_k: f64,
    /// Embedding dimensionality for the fallback hashing embedder (HashEmbedder picks its own
    /// default; this knob is reserved for tuning the dense backend dim).
    #[allow(dead_code)]
    pub embed_dim: usize,
    /// Enable the Jaro-Winkler fuzzy bridge in PRODUCTION retrieval (typo/morphology recall the
    /// exact BM25 floor misses). Default OFF — the lexical floor ships alone; flip on once a
    /// typo-heavy corpus proves net value. When on, `search_filtered` scores via `score_fuzzy`.
    pub enable_fuzzy: bool,
    /// Enable the dense (embedding) tier fused with lexical via RRF in PRODUCTION retrieval.
    /// Default OFF; the real semantic backend needs `--features dense` (else the pure-Rust hashing
    /// embedder, which exercises the path but isn't semantic). When on, `search_filtered` routes
    /// through `search_hybrid_in` with a persistent per-fact embedding cache.
    pub enable_dense: bool,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            frozen_core_max_tokens: 1500,
            search_top_k: 5,
            search_max_tokens: 1200,
            w_jaccard: 0.7,
            w_tf: 0.3,
            recency_half_life_days: 30.0,
            learn_min_confidence: 0.5,
            learn_store_confidence: 0.7,
            learn_core_confidence: 0.85,
            learn_dedup_threshold: 0.78,
            minhash_dup_threshold: 0.8,
            learn_inferred_cap: 500,
            rrf_k: 10.0,
            embed_dim: 256,
            enable_fuzzy: false,
            enable_dense: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_fragment_normalizes_and_bounds() {
        assert_eq!(slug_fragment("My Project!"), "my-project");
        assert_eq!(slug_fragment("***"), "project");
        assert!(slug_fragment(&"a".repeat(100)).chars().count() <= 24);
    }

    #[test]
    fn project_slug_is_stable_and_follows_ng_project_root() {
        // NG_PROJECT_ROOT is process-global env → serialize with every other home-mutating test.
        let _g = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-slug-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("NG_PROJECT_ROOT", &dir);
        let a = project_slug();
        let b = project_slug();
        assert_eq!(a, b, "same workspace → same slug (cached)");
        let hex = a.rsplit('-').next().unwrap();
        assert_eq!(hex.len(), 8, "hex8 suffix: {a}");
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        // repointing the root env gives a DIFFERENT zone (the cache can't serve stale)
        let dir2 = std::env::temp_dir().join(format!("ng-slug2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir2);
        std::env::set_var("NG_PROJECT_ROOT", &dir2);
        assert_ne!(project_slug(), a, "different root → different slug");
        std::env::remove_var("NG_PROJECT_ROOT");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }
}
