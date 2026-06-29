//! Project-convention loading. Walk from the working directory up to the repo root, collect any
//! `AGENTS.md` / `CLAUDE.md` files, and merge them into the top-level system prompt so the agent
//! inherits the codebase's build/test commands, layout, and house rules without the user repeating
//! them every turn. `AGENTS.md` is the 2025 cross-tool standard; `CLAUDE.md` is read for ecosystem
//! compatibility. Read-only and fail-soft: an unreadable file is skipped, and `None` (no block at
//! all) is returned when nothing is found — preserving the byte-stable prompt prefix for projects
//! that ship no conventions file.

use std::path::{Path, PathBuf};

/// Max chars of merged project context injected into the prompt — generous but bounded so a giant
/// committed doc can't blow the context budget (and keeps the cached prefix a sane size).
const MAX_CONTEXT_CHARS: usize = 12_000;

/// Hard cap on directories climbed, as insurance against a pathological tree / missing repo root.
const MAX_CLIMB: usize = 40;

/// Convention filenames, in priority order WITHIN a directory (first found per dir wins, so an
/// `AGENTS.md` beside a `CLAUDE.md` isn't double-counted).
const CONVENTION_FILES: &[&str] = &["AGENTS.md", "CLAUDE.md"];

/// Load merged project conventions for `cwd`, or `None` if none exist on the path to the repo root.
///
/// Walks from `cwd` up to (and including) the first ancestor containing `.git` (or the filesystem
/// root), then emits sections farthest→nearest so the NEAREST (most specific) file's guidance lands
/// LAST and therefore wins. Each section is headed by its path relative to the repo root. Never
/// errors. The total is capped at [`MAX_CONTEXT_CHARS`].
pub fn load_project_context(cwd: &Path) -> Option<String> {
    let start = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());

    // Collect dirs nearest→farthest, stopping at the repo root (a dir containing `.git`).
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut cur: &Path = &start;
    loop {
        dirs.push(cur.to_path_buf());
        if dirs.len() >= MAX_CLIMB || cur.join(".git").exists() {
            break; // repo root (or the safety cap) — stop climbing
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => break, // filesystem root
        }
    }
    // Emit farthest→nearest so the nearest file appears last (highest priority). The repo root is
    // also the label base, so sections read `# AGENTS.md`, `# crate/sub/AGENTS.md`, …
    dirs.reverse();
    let label_base = dirs.first().cloned().unwrap_or_else(|| start.clone());

    let mut sections: Vec<String> = Vec::new();
    for dir in &dirs {
        for name in CONVENTION_FILES {
            let path = dir.join(name);
            match std::fs::read_to_string(&path) {
                Ok(body) => {
                    let body = body.trim();
                    if !body.is_empty() {
                        sections.push(format!("# {}\n{}", display_label(&path, &label_base), body));
                    }
                    break; // one convention file per directory
                }
                Err(_) => continue,
            }
        }
    }
    if sections.is_empty() {
        return None;
    }

    let merged = sections.join("\n\n");
    if merged.chars().count() > MAX_CONTEXT_CHARS {
        let kept: String = merged.chars().take(MAX_CONTEXT_CHARS).collect();
        return Some(format!("{kept}\n…[project context truncated at {MAX_CONTEXT_CHARS} chars]…"));
    }
    Some(merged)
}

/// A readable label for a convention file: its path relative to the repo root, else the file name.
fn display_label(path: &Path, base: &Path) -> String {
    path.strip_prefix(base)
        .ok()
        .map(|p| p.display().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| path.display().to_string())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A hermetic temp tree with a `.git` marker at its root so the walk stops there (never climbs
    /// into the real filesystem).
    fn sandbox(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("ng-projctx-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(".git")).unwrap();
        root
    }

    fn write(dir: &Path, rel: &str, content: &str) {
        let p = dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, content).unwrap();
    }

    #[test]
    fn none_when_absent() {
        let root = sandbox("absent");
        assert!(load_project_context(&root).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn loads_root_agents_md() {
        let root = sandbox("root");
        write(&root, "AGENTS.md", "Build with cargo. UNIQUE_ROOT_FACT.");
        let ctx = load_project_context(&root).expect("should load");
        assert!(ctx.contains("UNIQUE_ROOT_FACT"));
        assert!(ctx.contains("# AGENTS.md"), "section headed by path: {ctx}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn nearest_wins_appears_last() {
        let root = sandbox("nearest");
        write(&root, "AGENTS.md", "ROOT_RULES");
        write(&root, "crate/sub/AGENTS.md", "SUB_RULES");
        let cwd = root.join("crate/sub");
        let ctx = load_project_context(&cwd).expect("should load");
        let root_at = ctx.find("ROOT_RULES").unwrap();
        let sub_at = ctx.find("SUB_RULES").unwrap();
        assert!(root_at < sub_at, "nearest (sub) must come last so it wins: {ctx}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn claude_md_read_compat_and_one_per_dir() {
        let root = sandbox("compat");
        // AGENTS.md takes priority over CLAUDE.md within the same dir (only one counted).
        write(&root, "AGENTS.md", "FROM_AGENTS");
        write(&root, "CLAUDE.md", "FROM_CLAUDE");
        let ctx = load_project_context(&root).expect("should load");
        assert!(ctx.contains("FROM_AGENTS"));
        assert!(!ctx.contains("FROM_CLAUDE"), "AGENTS.md wins within a dir: {ctx}");
        let _ = std::fs::remove_dir_all(&root);

        let root2 = sandbox("compat2");
        write(&root2, "CLAUDE.md", "ONLY_CLAUDE");
        let ctx2 = load_project_context(&root2).expect("should load");
        assert!(ctx2.contains("ONLY_CLAUDE"), "CLAUDE.md alone is read: {ctx2}");
        let _ = std::fs::remove_dir_all(&root2);
    }

    #[test]
    fn caps_oversized_context() {
        let root = sandbox("cap");
        write(&root, "AGENTS.md", &"x".repeat(MAX_CONTEXT_CHARS + 5_000));
        let ctx = load_project_context(&root).expect("should load");
        assert!(ctx.chars().count() <= MAX_CONTEXT_CHARS + 80, "capped near MAX: {}", ctx.chars().count());
        assert!(ctx.contains("truncated"), "marks truncation");
        let _ = std::fs::remove_dir_all(&root);
    }
}
