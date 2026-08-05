//! Path + settings resolution for the standalone CLI memory brain.
//!
//! Memory is the CLI's OWN, under `~/.aizen/cli-memory/` — no byte-for-byte
//! interop requirement with the VS Code extension (owner decision 2026-06-20).
//! The home root is `~/.aizen` (renamed from the pre-rebrand `~/.aizen`, which is
//! auto-migrated on first run so an upgrading user keeps all their data).

use std::path::{Path, PathBuf};

/// One process-wide lock for tests that mutate the global home env var. Tests run in parallel
/// across modules, so a per-module mutex isn't enough — they'd race on the same env var. Every
/// home-mutating test must hold THIS lock.
#[cfg(test)]
pub(crate) static TEST_HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Set env vars for a test and RESTORE their prior values on drop.
///
/// The obvious teardown — `remove_var` for each key — is wrong for `USERPROFILE`/`HOME`, and the bug
/// it caused was subtle enough to hunt twice. Several helpers point those at a sandbox and then
/// delete them, so after the first such test the process has NO home at all. Code that treats "no
/// home" as "no home boundary" then loses its guard: `lsp::discovery::is_forbidden_root` returns
/// false for every directory, so a walk that should stop below the user profile climbs past it and
/// claims whatever manifest sits there. That produced a failure in an unrelated test, in a different
/// module, only in a full parallel run — and pointed the blame at a stray `~/package.json` rather
/// than at the teardown.
///
/// Restoring the prior value (including "was absent") keeps a test's mutation from outliving it.
#[cfg(test)]
pub(crate) struct EnvGuard {
    saved: Vec<(String, Option<std::ffi::OsString>)>,
}

#[cfg(test)]
impl EnvGuard {
    /// Snapshot `keys`, then apply `set`. Keys in `set` need not appear in `keys`.
    pub(crate) fn set<I, K, V>(vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<std::ffi::OsStr>,
        V: AsRef<std::ffi::OsStr>,
    {
        let mut saved = Vec::new();
        for (k, v) in vars {
            let key = k.as_ref().to_string_lossy().to_string();
            if !saved.iter().any(|(s, _): &(String, _)| *s == key) {
                saved.push((key, std::env::var_os(k.as_ref())));
            }
            std::env::set_var(k.as_ref(), v.as_ref());
        }
        Self { saved }
    }

    /// Snapshot `keys` and remove them, restoring on drop. For tests that need a var ABSENT.
    pub(crate) fn unset<I, K>(keys: I) -> Self
    where
        I: IntoIterator<Item = K>,
        K: AsRef<std::ffi::OsStr>,
    {
        let mut saved = Vec::new();
        for k in keys {
            saved.push((
                k.as_ref().to_string_lossy().to_string(),
                std::env::var_os(k.as_ref()),
            ));
            std::env::remove_var(k.as_ref());
        }
        Self { saved }
    }
}

#[cfg(test)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        for (k, v) in self.saved.drain(..) {
            match v {
                Some(val) => std::env::set_var(&k, val),
                None => std::env::remove_var(&k),
            }
        }
    }
}

/// Resolve the home root: `AIZEN_HOME` (legacy `AIZEN_HOME`) else `USERPROFILE`/`HOME`/cwd +
/// `/.aizen`. When no env override is set, a pre-rebrand `~/.aizen` is migrated to `~/.aizen`
/// on first use (atomic same-parent rename) so memory/personas/soul/config carry over. The fn
/// name stays `aizen_home` (internal; called everywhere) — only the path + brand changed.
pub fn aizen_home() -> PathBuf {
    for var in ["AIZEN_HOME", "AIZEN_HOME"] {
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

/// The home data root: always `<base>/.aizen`.
///
/// There is deliberately NO legacy-name fallback here. The pre-rebrand `.aizen` root predates
/// this repository: the earliest tag (v0.4.0) already defaults to `.aizen`, so no released build
/// ever created a `.aizen` home, and a migration branch for a directory nothing ever wrote is
/// dead weight that keeps the old brand alive in every path decision.
fn resolve_default_home(base: &Path) -> PathBuf {
    base.join(".aizen")
}

/// The project root for PROJECT-LOCAL customization (`./.aizen/`): the git repo top-level if we're
/// in one, else the current dir. Repo-root-aware so launching `aizen` from a SUBDIR still finds the
/// repo's `.aizen/` (R4 — fixes the cwd-relative footgun). `AIZEN_PROJECT_ROOT` overrides (tests + an
/// escape hatch). Shell-out to git keeps the pure-static posture (no git2/gix).
pub fn project_root() -> PathBuf {
    if let Ok(v) = std::env::var("AIZEN_PROJECT_ROOT") {
        let v = v.trim();
        if !v.is_empty() {
            return PathBuf::from(v);
        }
    }
    if let Ok(mut cmd) = crate::core::gitx::command() {
        if let Ok(out) = cmd.args(["rev-parse", "--show-toplevel"]).output() {
            if out.status.success() {
                let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !s.is_empty() {
                    return PathBuf::from(s);
                }
            }
        }
    }
    // No usable git (absent, or it refused — e.g. dubious-ownership): walk UP for a project
    // marker, pure-Rust. One checkout must keep ONE identity even on a gitless machine — the old
    // straight-to-cwd fallback minted a fresh zone per LAUNCH DIR, which is half of the slug-fork
    // disease. Looking ONLY for `.git` left that fork wide open for trees that aren't repos at all:
    // `proj/` and `proj/src/` became two projects, so each subdir grew its own zone and disowned
    // the other's sessions. A VCS marker outranks a manifest (it bounds the whole checkout), and
    // within each class the OUTERMOST match wins — same semantics as `git rev-parse --show-toplevel`
    // for a workspace member. cwd is the true last resort (a bare dir with no marker at all).
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    const VCS: [&str; 4] = [".git", ".hg", ".svn", ".jj"];
    const MANIFEST: [&str; 4] = ["Cargo.toml", "package.json", "pyproject.toml", "go.mod"];
    // The walk stops BELOW the user's home dir. Without that bound a stray manifest in `~` would
    // make every non-repo tree under it share one root — collapsing unrelated projects into a
    // single zone, which is a worse identity bug than the per-subdir fork this fixes. (`.aizen`
    // deliberately is NOT a marker: `~/.aizen` is the global home, so honoring it would resolve
    // every project under the home dir to the home dir itself.)
    let home_bound = std::env::var("USERPROFILE")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("HOME").ok().filter(|s| !s.trim().is_empty()))
        .map(PathBuf::from);
    let outermost = |markers: &[&str]| -> Option<PathBuf> {
        let mut found = None;
        let mut cur = Some(cwd.as_path());
        while let Some(d) = cur {
            // `h.starts_with(d)` is true exactly when `d` IS the home dir or an ancestor of it.
            if home_bound.as_deref().is_some_and(|h| h.starts_with(d)) {
                break;
            }
            if markers.iter().any(|m| d.join(m).exists()) {
                found = Some(d.to_path_buf());
            }
            cur = d.parent();
        }
        found
    };
    outermost(&VCS)
        .or_else(|| outermost(&MANIFEST))
        .unwrap_or_else(|| cwd.clone())
}

/// Workspace-scoping kill-switch: `AIZEN_NO_SCOPE=1` collapses memory back to one global pool
/// (every scope filter passes everything). Escape hatch, not a config field — scoping is the
/// intended default and this exists only to debug/compare.
pub fn scope_disabled() -> bool {
    matches!(
        std::env::var("AIZEN_NO_SCOPE")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// True when an env var is set to a truthy token (`1`/`true`/`yes`/`on`). Shared by the
/// memory kill-switches so they parse identically.
fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref().map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Hebbian co-retrieval graph kill-switch. `AIZEN_NO_GRAPH=1` turns OFF both edge *recording*
/// (retrieval writes nothing to `graph.tsv`) and edge *reading* (neighbor expansion is skipped),
/// collapsing retrieval to the pre-P5 lexical/salience path. Escape hatch for debugging/benching.
pub fn graph_disabled() -> bool {
    env_flag("AIZEN_NO_GRAPH")
}

/// Opt-IN switch for graph neighbor-expansion in PRODUCTION retrieval. `AIZEN_GRAPH_EXPAND=1` lets a
/// strong lexical hit pull in its most-associated neighbors (spreading activation). Default OFF —
/// same posture as the dense/fuzzy moat tiers: the edge graph is always *recorded* (cheap, builds
/// the corpus), but *reading* it into results waits until the bench proves net recall value.
pub fn graph_expand_enabled() -> bool {
    !graph_disabled() && env_flag("AIZEN_GRAPH_EXPAND")
}

/// Opt-IN switch for ranking the always-on `<skills>` index by graph affinity to the facts recalled
/// this turn — i.e. "which procedure historically fired alongside what we're talking about now",
/// instead of "which procedure is used most overall". Default OFF for the same reason as
/// [`graph_expand_enabled`]: the cross-kind edges are always recorded, but letting them reorder what
/// the model sees waits until a bench shows it beats the plain usage ordering.
pub fn skill_graph_rank_enabled() -> bool {
    !graph_disabled() && env_flag("AIZEN_SKILL_GRAPH_RANK")
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

/// The stable identity of the current workspace: `dirname-hex8`. The hash key is the workspace
/// root's canonicalized path in ONE normalized spelling ([`workspace_key`]) — identity is WHERE
/// the checkout lives, full stop. The old key preferred `remote.origin.url` with a raw
/// `canonicalize` fallback, which let *whether git happened to spawn* (PATH luck) pick the key:
/// the same repo silently forked into twin memory/skills/index zones. A moved or re-cloned
/// checkout is now an explicit `aizen zone migrate`, never an implicit re-key. Cached per
/// (`AIZEN_PROJECT_ROOT` env, cwd) so tests that repoint `AIZEN_PROJECT_ROOT` are never served a
/// stale slug.
pub fn project_slug() -> String {
    identity().2
}

/// The normalized identity key of the current project root — the exact string [`project_slug`]
/// hashes. Session provenance stamps store it so "same project?" checks agree byte-for-byte with
/// zone identity instead of re-deriving their own path comparison.
pub fn project_key() -> String {
    identity().1
}

/// `(root, key, slug)` for the current workspace, computed ONCE per (`AIZEN_PROJECT_ROOT`, cwd).
///
/// The three are derived from the same `project_root()` call by construction, so a caller can never
/// see a key and a slug that disagree. Sharing the cache also matters for cost: `project_root()` may
/// spawn `git rev-parse`, and stamping session provenance wants all three every autosave — three
/// separate lookups would have meant three git spawns per turn.
fn identity() -> (PathBuf, String, String) {
    static CACHE: std::sync::Mutex<Option<(String, (PathBuf, String, String))>> =
        std::sync::Mutex::new(None);
    let cache_key = format!(
        "{}|{}",
        std::env::var("AIZEN_PROJECT_ROOT").unwrap_or_default(),
        std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    if let Ok(guard) = CACHE.lock() {
        if let Some((k, ident)) = guard.as_ref() {
            if *k == cache_key {
                return ident.clone();
            }
        }
    }
    let root = project_root();
    let name = root
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("project");
    let key = workspace_key(&root);
    let slug = slug_for_key(name, &key);
    let ident = (root, key, slug);
    if let Ok(mut guard) = CACHE.lock() {
        *guard = Some((cache_key, ident.clone()));
    }
    ident
}

/// The `dirname-hex8` slug for an explicit (name, hash key) pair — the one formatter both the
/// live keying and the legacy-candidate computation go through (pub(crate) so the zones tests
/// can fabricate byte-exact legacy slugs).
pub(crate) fn slug_for_key(name: &str, key: &str) -> String {
    format!("{}-{:08x}", slug_fragment(name), fnv1a64(key) as u32)
}

/// Hash key for a workspace root: its canonicalized path in ONE normalized spelling — verbatim
/// `\\?\` prefix stripped, `\` → `/`, drive letter lowercased, no trailing slash — mirroring
/// `workspace_txn::normalized_path` so the two identity systems agree on "same directory".
/// Canonicalize-failure falls back to the raw path through the same normalizer (still
/// deterministic for a given spelling), so a vanished dir can't panic the slug.
fn workspace_key(root: &Path) -> String {
    let canon = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    normalize_path_key(&canon)
}

/// See [`workspace_key`]. Split out so tests can prove every spelling of one directory
/// (verbatim vs plain, trailing slash, mixed separators) hashes identically.
pub fn normalize_path_key(p: &Path) -> String {
    let mut value = p.to_string_lossy().replace('\\', "/");
    if let Some(rest) = value.strip_prefix("//?/UNC/") {
        // Verbatim UNC (`\\?\UNC\server\share`) and the plain `\\server\share` spelling of the
        // same share must collapse to one key, or network checkouts fork on canonicalize luck.
        value = format!("//{rest}");
    } else if let Some(rest) = value.strip_prefix("//?/") {
        value = rest.to_string();
    }
    if value.as_bytes().get(1) == Some(&b':') {
        value.replace_range(0..1, &value[..1].to_ascii_lowercase());
    }
    value.trim_end_matches('/').to_string()
}

/// Slugs the pre-2026-07 keying could have produced for the CURRENT project. Two eras to cover:
/// the remote-URL key (git worked), and the path keys (git didn't) — where the OLD gitless
/// fallback rooted at the LAUNCH DIR, so a habitual subdir launch keyed a zone under the
/// SUBDIR's name and path. Candidates therefore span the root itself, today's cwd, and every
/// directory between them, each in its canonicalize-verbatim and raw spellings. The current
/// slug itself is excluded; `aizen zone migrate` merges artifacts found under the rest.
pub fn legacy_slug_candidates() -> Vec<String> {
    let root = project_root();
    let cwd = std::env::current_dir().unwrap_or_else(|_| root.clone());
    legacy_slug_candidates_for(&root, &cwd, git_remote_origin(&root), &project_slug())
}

/// The injectable core of [`legacy_slug_candidates`] (cwd is process-global — tests must not
/// `set_current_dir`, so they pass one in).
fn legacy_slug_candidates_for(
    root: &Path,
    cwd: &Path,
    remote_url: Option<String>,
    current: &str,
) -> Vec<String> {
    let mut pairs: Vec<(String, String)> = Vec::new(); // (dirname, hash key)
    let dirname = |p: &Path| {
        p.file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("project")
            .to_string()
    };
    if let Some(url) = remote_url {
        pairs.push((dirname(root), url));
    }
    // The launch dirs the old keying could have rooted at: root, cwd, and everything between.
    let mut dirs: Vec<PathBuf> = vec![root.to_path_buf()];
    let mut cur = Some(cwd);
    while let Some(p) = cur {
        if p == root {
            break;
        }
        if p.starts_with(root) {
            dirs.push(p.to_path_buf());
        }
        cur = p.parent();
    }
    for d in dirs {
        let n = dirname(&d);
        if let Ok(c) = std::fs::canonicalize(&d) {
            pairs.push((n.clone(), c.display().to_string())); // Windows: `\\?\C:\…` verbatim
        }
        pairs.push((n, d.display().to_string()));
    }
    let mut out: Vec<String> = Vec::new();
    for (n, k) in pairs {
        let s = slug_for_key(&n, &k);
        if s != current && !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// `git config --get remote.origin.url` in `root` — `None` when not a repo / no remote / no git.
/// No longer part of the identity key; kept for `zone migrate` (legacy-URL slugs) and `/where`.
pub fn git_remote_origin(root: &Path) -> Option<String> {
    let out = crate::core::gitx::command()
        .ok()?
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
/// commands), merged OVER the home dir by each resolver: always `<root>/.aizen`. Like
/// [`resolve_default_home`], this carries no legacy-name fallback — one name, everywhere.
pub fn project_aizen_dir() -> PathBuf {
    project_root().join(".aizen")
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

/// Normalize an arbitrary path to the canonical anchor form used by the tier/place system:
/// canonicalize (fallback raw) → normalize_path_key → to_ascii_lowercase (Windows segment-safe).
pub fn anchor_of(path: &Path) -> String {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let mut key = normalize_path_key(&canon);
    // ascii-lowercase: sigma-final-form / İ must not break prefix matching
    key = key.to_ascii_lowercase();
    key
}

/// The current working directory as a normalized anchor string (the anchor a `place` fact
/// would get if the user didn't specify one).
pub fn current_anchor() -> String {
    match std::env::current_dir() {
        Ok(p) => anchor_of(&p),
        Err(_) => String::new(),
    }
}

/// A human-friendly label for the current project (the git remote origin when available,
/// else the directory name). Used in prompts and `/where`.
pub fn project_label() -> Option<String> {
    // Prefer the git remote origin
    let root = project_root();
    if let Some(url) = git_remote_origin(&root) {
        // Extract a readable name from the URL
        let label = url
            .trim_end_matches(".git")
            .rsplit('/')
            .next()
            .or_else(|| url.rsplit(':').next())
            .unwrap_or(&url)
            .to_string();
        if !label.is_empty() {
            return Some(label);
        }
    }
    // Fallback: the directory name
    root.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .filter(|s| !s.is_empty())
}

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
    aizen_home().join("cli-memory")
}

/// Long-tail entry store (one fact per `*.md`).
pub fn entries_dir() -> PathBuf {
    cli_memory_dir().join("entries")
}

/// The always-on user-style profile rendered into the frozen core.
pub fn style_path() -> PathBuf {
    cli_memory_dir().join("STYLE.md")
}

/// Per-repo codebase index (`/init`). Layout: `cli-memory/codebase/<slug>.json`.
/// The slug is sanitized to a single safe path segment so a hostile repo name can't
/// escape the index dir.
pub fn codebase_index_path(slug: &str) -> PathBuf {
    cli_memory_dir()
        .join("codebase")
        .join(format!("{}.json", safe_core_slug(slug)))
}

/// Sanitize a project slug for use as a single path segment (no `/` `\` `..`).
fn safe_core_slug(slug: &str) -> String {
    let s: String = slug
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "project".into()
    } else {
        s.chars().take(64).collect()
    }
}

/// Per-repo always-on frozen core (active). Layout: `cli-memory/core/active/<slug>.md`.
/// Isolates prefix cache so repo A's core never injects into repo B.
pub fn core_active_path(slug: &str) -> PathBuf {
    cli_memory_dir()
        .join("core")
        .join("active")
        .join(format!("{}.md", safe_core_slug(slug)))
}

/// Per-repo staged core for next session. Layout: `cli-memory/core/next/<slug>.md`.
pub fn core_next_path(slug: &str) -> PathBuf {
    cli_memory_dir()
        .join("core")
        .join("next")
        .join(format!("{}.md", safe_core_slug(slug)))
}

/// Legacy single-file active core (pre per-repo layout). Read-once fallback only.
pub fn legacy_core_active_path() -> PathBuf {
    cli_memory_dir().join("core.active.md")
}

/// Legacy single-file staged core (pre per-repo layout).
pub fn legacy_core_next_path() -> PathBuf {
    cli_memory_dir().join("core.next.md")
}

/// Mid-confidence learned candidates land here for `aizen memory review` (P3).
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

/// The Hebbian co-retrieval graph (P5 — neuron-link associations between facts). A single
/// plain-text edge file next to the entry store; the ONE piece of memory state that is not
/// derivable from fact content (which facts get recalled together over time).
pub fn graph_path() -> PathBuf {
    cli_memory_dir().join("graph.tsv")
}

/// Local model store (P5 dense backend). Shared `~/.aizen/models/` (NOT under cli-memory)
/// so other Aizen tooling can reuse a downloaded model. Consumed by the `dense` feature build.
#[allow(dead_code)] // used by `--features dense` (model2vec loader); inert in the default build
pub fn models_dir() -> PathBuf {
    aizen_home().join("models")
}

/// The dense embedding model: a subdir name of `models_dir()`, or an absolute path to a model dir.
///
/// Precedence `AIZEN_EMBED_MODEL` → the saved `embed_model` in cli-config → the benched default.
/// The env var stays on top so a one-off `AIZEN_EMBED_MODEL=… aizen …` still wins over the file, and
/// the file exists so the `/config` Memory section can make a durable choice instead of asking the
/// user to keep an env var set forever.
///
/// No recursion risk in reading cli-config from here: `cli_config::load` only reaches back into
/// `config_path()` → `aizen_home()`, neither of which consults this function.
#[allow(dead_code)] // used by `--features dense` (model2vec loader); inert in the default build
pub fn embed_model_name() -> String {
    std::env::var("AIZEN_EMBED_MODEL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            crate::core::cli_config::load()
                .embed_model
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
        })
        // potion-base-8M (English static, ~30MB) — the P6 CI bench (dense-bench.yml) measured it
        // LIFTING paraphrase recall@5 more than the ~18x-larger multilingual model (+0.231 vs
        // +0.154) on the bilingual En-Vi fixtures, so it's the default. `AIZEN_EMBED_MODEL` overrides.
        .unwrap_or_else(|| "potion-base-8M".to_string())
}

/// Verified-good defaults adopted from the retrieval/anti-bloat research.
/// These are *starting points* the bench can retune — not a binding contract.
#[derive(Debug, Clone)]
pub struct MemorySettings {
    /// Frozen-core hard cap (tokens, chars/4 estimate). Prefix-stability budget.
    /// Always-on is STYLE + global user prefs only — keep this tight to save tokens.
    pub frozen_core_max_tokens: usize,
    /// Session working-memory inject cap (tokens, chars/4). 0 disables inject.
    pub session_mem_max_tokens: usize,
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
    /// through `search_hybrid_gated_in` with a persistent per-fact embedding cache.
    pub enable_dense: bool,
    /// Dense query-level GATE (P6): fuse the dense tier only when the best lexical hit covers FEWER
    /// than this fraction of the query's tokens — i.e. BM25 is ambiguous (paraphrase / cross-lingual).
    /// A confident, high-coverage literal match skips dense entirely, preserving its precision (the
    /// bench showed always-on fusion lifts paraphrase recall but wrecks literal precision/noise).
    /// `0.0` ⇒ never gate open (dense off in practice); `≥1.0` ⇒ always fuse (the always-on ceiling).
    pub dense_gate_coverage: f64,
}

impl Default for MemorySettings {
    fn default() -> Self {
        Self {
            frozen_core_max_tokens: 800,
            session_mem_max_tokens: 300,
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
            // Gate open when the best lexical hit covers <60% of the query tokens. Tuned so a
            // clean literal match (full coverage) skips dense while a paraphrase/cross-lingual
            // query (low lexical overlap) opens it. The bench (`--split tune --hybrid`) is what
            // re-tunes this: it must keep the paraphrase recall lift without the literal-slice
            // precision/noise hit that always-on fusion showed.
            dense_gate_coverage: 0.60,
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
    fn normalize_path_key_is_spelling_invariant() {
        // Pure string transform — host-independent, so windows-style literals are safe here.
        // Every spelling of one directory must hash identically: the OLD key forked the zone on
        // exactly this (verbatim `\\?\` canonicalize vs the plain-path fallback).
        let want = "c:/Users/Admin/proj";
        assert_eq!(
            normalize_path_key(Path::new(r"\\?\C:\Users\Admin\proj")),
            want
        );
        assert_eq!(normalize_path_key(Path::new(r"C:\Users\Admin\proj")), want);
        assert_eq!(normalize_path_key(Path::new("C:/Users/Admin/proj/")), want);
        assert_eq!(
            normalize_path_key(Path::new("/home/u/proj/")),
            "/home/u/proj"
        );
        // Verbatim UNC and the plain spelling of the same share collapse to one key.
        assert_eq!(
            normalize_path_key(Path::new(r"\\?\UNC\srv\share\proj")),
            "//srv/share/proj"
        );
        assert_eq!(
            normalize_path_key(Path::new(r"\\srv\share\proj")),
            "//srv/share/proj"
        );
    }

    #[test]
    fn workspace_key_never_carries_the_verbatim_spelling() {
        let dir = std::env::temp_dir().join(format!("aizen-wskey-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let canon = std::fs::canonicalize(&dir).expect("canonicalize temp dir");
        let key = workspace_key(&dir);
        // The real regression pin (Windows): canonicalize returns the `\\?\` verbatim form, and
        // exactly that spelling reaching the hash is what forked zones. The key must equal the
        // normalizer applied to the verbatim DISPLAY STRING — the old fork input — not just to
        // whatever canonicalize returns (that comparison would be a tautology).
        assert!(
            !key.starts_with("//?/"),
            "verbatim prefix must never reach the key: {key}"
        );
        assert_eq!(
            key,
            normalize_path_key(Path::new(&canon.display().to_string()))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_candidates_cover_url_root_and_subdir_launch_eras() {
        let base = std::env::temp_dir().join(format!("aizen-legacy-{}", std::process::id()));
        let root = base.join("repo");
        let cwd = root.join("src").join("agent");
        let _ = std::fs::create_dir_all(&cwd);
        let current = slug_for_key("repo", &workspace_key(&root));
        let cands = legacy_slug_candidates_for(
            &root,
            &cwd,
            Some("https://example.com/o/repo.git".to_string()),
            &current,
        );
        assert!(
            !cands.contains(&current),
            "current slug is never a candidate"
        );
        // URL era: hashed remote under the ROOT's name.
        let url_slug = slug_for_key("repo", "https://example.com/o/repo.git");
        assert!(
            cands.contains(&url_slug),
            "URL-keyed era must be covered: {cands:?}"
        );
        // Gitless era, habitual SUBDIR launch: the old fallback rooted at cwd, so the zone lived
        // under the SUBDIR's name and raw path. That population must be findable.
        let subdir_raw = slug_for_key("agent", &cwd.display().to_string());
        assert!(
            cands.contains(&subdir_raw),
            "cwd-keyed gitless era must be covered: {cands:?}"
        );
        // …and the intermediate dir between cwd and root too.
        let mid_raw = slug_for_key("src", &root.join("src").display().to_string());
        assert!(
            cands.contains(&mid_raw),
            "intermediate launch dirs must be covered: {cands:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn project_slug_is_stable_and_follows_ng_project_root() {
        // AIZEN_PROJECT_ROOT is process-global env → serialize with every other home-mutating test.
        let _g = TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-slug-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::env::set_var("AIZEN_PROJECT_ROOT", &dir);
        let a = project_slug();
        let b = project_slug();
        assert_eq!(a, b, "same workspace → same slug (cached)");
        let hex = a.rsplit('-').next().unwrap();
        assert_eq!(hex.len(), 8, "hex8 suffix: {a}");
        assert!(hex.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        // repointing the root env gives a DIFFERENT zone (the cache can't serve stale)
        let dir2 = std::env::temp_dir().join(format!("aizen-slug2-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir2);
        std::env::set_var("AIZEN_PROJECT_ROOT", &dir2);
        assert_ne!(project_slug(), a, "different root → different slug");
        std::env::remove_var("AIZEN_PROJECT_ROOT");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }
}
