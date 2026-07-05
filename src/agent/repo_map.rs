//! `repo_map` — an on-demand, ranked skeleton of the codebase (files + key symbols) so the model
//! orients in ONE call instead of N exploratory reads.
//!
//! The classic implementation (Aider) builds this with tree-sitter — a C library this machine
//! cannot compile. aizen already runs LANGUAGE SERVERS, which know the symbols better than
//! tree-sitter does: the map is file tree (ignore-aware walk) → ranking by GIT CHURN (recently /
//! frequently touched files are what the model most likely needs; plain `git log --name-only`
//! shell-out — no libgit2) with mtime as the no-repo fallback → `documentSymbol` skeletons for the
//! top files. Registered only while LSP is ON, and it is a TOOL (just-in-time, cache-friendly) —
//! never an always-on prompt block.

use crate::agent::lsp::discovery;
use crate::agent::tools::Tool;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Files scanned at most (the walk is ignore-aware, so this is generous).
const MAX_SCAN: usize = 2_000;
/// Files that get an LSP symbol skeleton (the rest are listed name-only).
const MAX_SYMBOL_FILES: usize = 12;
/// Symbols shown per file (depth ≤ 1).
const MAX_SYMS_PER_FILE: usize = 15;
/// Soft wall-clock deadline for the symbol phase — files past it are listed without symbols.
const SYMBOL_DEADLINE_SECS: u64 = 10;
/// Hard cap on the rendered map.
const MAX_MAP_CHARS: usize = 8_000;
/// Commits sampled for the churn ranking.
const CHURN_COMMITS: &str = "300";

pub struct RepoMap {
    root: PathBuf,
}

impl RepoMap {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for RepoMap {
    fn name(&self) -> &str {
        "repo_map"
    }
    fn description(&self) -> &str {
        "A ranked skeleton of the codebase: the most-changed source files (git churn) with their \
         key symbols from the language server. Use ONCE to orient in an unfamiliar repo/subtree \
         before targeted reads — not a substitute for search_files (find text) or file_read. \
         Requires LSP on. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "scope": {"type": "string", "description": "optional path prefix to map (e.g. src/agent); default: the whole project"},
                "max_files": {"type": "integer", "description": "files to include (default 25, cap 50)"}
            },
            "additionalProperties": false
        })
    }
    /// Shared LSP manager state — same serial posture as the lsp_* tools.
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let max_files = args
            .get("max_files")
            .and_then(|v| v.as_u64())
            .map(|n| (n as usize).clamp(1, 50))
            .unwrap_or(25);
        let scope_root = match args.get("scope").and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()) {
            Some(scope) => {
                let p = self.root.join(scope);
                if !p.is_dir() {
                    return Ok(format!("error: scope '{scope}' is not a directory under the project root"));
                }
                p
            }
            None => self.root.clone(),
        };

        // 1) Ignore-aware walk for SOURCE files (the union of the LSP server table's extensions).
        let files = source_files(&scope_root);
        if files.is_empty() {
            return Ok("no source files found in scope".to_string());
        }
        let total = files.len();

        // 2) Rank: git churn primary, mtime recency as tiebreak/fallback.
        let churn = git_churn(&self.root);
        let mut ranked: Vec<(PathBuf, u32, u64)> = files
            .into_iter()
            .map(|p| {
                let rel = p.strip_prefix(&self.root).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                let c = churn.as_ref().and_then(|m| m.get(rel.as_str())).copied().unwrap_or(0);
                let mtime = p
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                (p, c, mtime)
            })
            .collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.cmp(&a.2)));
        ranked.truncate(max_files);

        // 3) Symbol skeletons for the top files (bounded; fail-soft per file).
        let by = if churn.is_some() { "git churn" } else { "recency" };
        let mut out = format!("repo map (top {} of {total} source files by {by}; symbols from LSP)\n", ranked.len());
        let deadline = Instant::now() + std::time::Duration::from_secs(SYMBOL_DEADLINE_SECS);
        for (i, (path, churn_n, _)) in ranked.iter().enumerate() {
            let rel = path.strip_prefix(&self.root).unwrap_or(path).to_string_lossy().replace('\\', "/");
            let churn_tag = if *churn_n > 0 { format!("  churn:{churn_n}") } else { String::new() };
            out.push_str(&format!("{rel}{churn_tag}\n"));
            if i < MAX_SYMBOL_FILES && Instant::now() < deadline {
                if let Ok(syms) = crate::agent::lsp::LSP.document_symbols_items(path) {
                    let line = render_symbol_line(&syms);
                    if !line.is_empty() {
                        out.push_str(&format!("  {line}\n"));
                    }
                }
            }
            if out.chars().count() > MAX_MAP_CHARS {
                out.push_str("…[map truncated]\n");
                break;
            }
        }
        Ok(out.trim_end().to_string())
    }
}

/// Ignore-aware walk collecting files whose extension any LSP server handles.
fn source_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for entry in ignore::WalkBuilder::new(root).build().flatten() {
        if out.len() >= MAX_SCAN {
            break;
        }
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
        if discovery::SERVERS.iter().any(|s| s.extensions.contains(&ext.as_str())) {
            out.push(p.to_path_buf());
        }
    }
    out
}

/// Per-file change counts over the last [`CHURN_COMMITS`] commits (`git log --name-only`) — the
/// same shell-out pattern as timemachine. `None` when not a repo / git missing.
fn git_churn(root: &Path) -> Option<HashMap<String, u32>> {
    let out = std::process::Command::new("git")
        .args(["log", "--name-only", "--pretty=format:", "-n", CHURN_COMMITS])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(parse_churn(&String::from_utf8_lossy(&out.stdout)))
}

/// Count path occurrences in `git log --name-only` output (pure — unit-tested on a fixture).
fn parse_churn(stdout: &str) -> HashMap<String, u32> {
    let mut m = HashMap::new();
    for line in stdout.lines() {
        let l = line.trim();
        if !l.is_empty() {
            *m.entry(l.to_string()).or_insert(0) += 1;
        }
    }
    m
}

/// One compact line of a file's top-level symbols: `kind name:line`, depth ≤ 1, capped — vertical
/// outlines blow the map cap fast; `fn` (the dominant kind) drops its prefix for density.
fn render_symbol_line(syms: &[crate::agent::lsp::server::DocSym]) -> String {
    let mut parts = Vec::new();
    for s in syms.iter().filter(|s| s.depth <= 1).take(MAX_SYMS_PER_FILE) {
        if s.kind == "function" || s.kind == "method" {
            parts.push(format!("{}:{}", s.name, s.line));
        } else {
            parts.push(format!("{} {}:{}", s.kind, s.name, s.line));
        }
    }
    parts.join("  ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn churn_parse_counts_paths() {
        let fixture = "\
src/agent/mod.rs
src/main.rs

src/agent/mod.rs
src/llm/client.rs

src/agent/mod.rs
";
        let m = parse_churn(fixture);
        assert_eq!(m.get("src/agent/mod.rs"), Some(&3));
        assert_eq!(m.get("src/main.rs"), Some(&1));
        assert_eq!(m.get("src/llm/client.rs"), Some(&1));
        assert_eq!(m.len(), 3, "blank separator lines don't count");
    }

    #[test]
    fn symbol_line_is_compact_and_capped() {
        use crate::agent::lsp::server::DocSym;
        let syms: Vec<DocSym> = (0..30)
            .map(|i| DocSym { name: format!("f{i}"), kind: "function", line: i + 1, depth: 0 })
            .collect();
        let line = render_symbol_line(&syms);
        assert!(line.starts_with("f0:1  f1:2"), "{line}");
        assert!(!line.contains("f20"), "capped at {MAX_SYMS_PER_FILE}");
        // Non-fn kinds keep their kind prefix.
        let one = vec![DocSym { name: "Config".into(), kind: "struct", line: 7, depth: 0 }];
        assert_eq!(render_symbol_line(&one), "struct Config:7");
        // Deep symbols are skipped.
        let deep = vec![DocSym { name: "inner".into(), kind: "function", line: 9, depth: 2 }];
        assert_eq!(render_symbol_line(&deep), "");
    }

    #[test]
    fn source_files_filters_by_lsp_extensions() {
        let d = std::env::temp_dir().join(format!("ng-repomap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("a.rs"), "fn main() {}").unwrap();
        std::fs::write(d.join("b.py"), "x = 1").unwrap();
        std::fs::write(d.join("c.txt"), "not source").unwrap();
        std::fs::write(d.join("d.exe"), [0u8; 4]).unwrap();
        let files = source_files(&d);
        let names: Vec<String> =
            files.iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).collect();
        assert!(names.contains(&"a.rs".to_string()) && names.contains(&"b.py".to_string()), "{names:?}");
        assert!(!names.iter().any(|n| n == "c.txt" || n == "d.exe"), "{names:?}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
