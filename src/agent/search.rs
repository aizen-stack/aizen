//! First-party content search (`search_files`). Before this, the tool surface told the model to
//! "use shell_run + ripgrep" for content search — a silent footgun: on a machine without `rg` on
//! PATH it just fails. This drives ripgrep's OWN walker (`ignore`, which respects `.gitignore` and
//! skips hidden/binary) + the in-tree `regex` crate for line matching. Pure-Rust, static-safe,
//! read-only — and it's the canonical content-search tool (`file_glob` matches NAMES, this matches
//! CONTENT).

use crate::agent::builtin::confine;
use crate::agent::tools::Tool;
use anyhow::{Context, Result};
use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde_json::Value;
use std::path::PathBuf;

/// Cap on emitted matches (the loop also bounds history growth at the agent layer).
const DEFAULT_MAX_RESULTS: usize = 200;
/// Skip files larger than this (generated bundles / data dumps aren't useful search targets).
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Bytes sniffed for a NUL to classify a file as binary (then skipped).
const BINARY_SNIFF: usize = 8192;
/// Clip a very long matched line so one minified line can't blow the result.
const MAX_LINE_CHARS: usize = 240;

pub struct SearchFiles {
    root: PathBuf,
}
impl SearchFiles {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for SearchFiles {
    fn name(&self) -> &str {
        "search_files"
    }
    fn description(&self) -> &str {
        "Search file CONTENT by regular expression across the working directory (respects \
         .gitignore, skips hidden + binary files). Returns `path:line: matched text`. This is the \
         canonical content search — do NOT shell out to grep/ripgrep, and do NOT use file_glob \
         (that matches file NAMES, not content). Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "pattern": {"type": "string", "description": "a regular expression to match against each line"},
                "path": {"type": "string", "description": "optional subdir of the working dir to limit the search"},
                "glob": {"type": "string", "description": "optional file glob to restrict which files are searched, e.g. *.rs or src/**/*.ts"},
                "ignore_case": {"type": "boolean", "description": "case-insensitive match (default false)"},
                "max_results": {"type": "integer", "description": "cap on matches returned (default 200)"}
            },
            "required": ["pattern"]
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let pattern = args.get("pattern").and_then(|v| v.as_str()).context("missing `pattern`")?;
        let ignore_case = args.get("ignore_case").and_then(|v| v.as_bool()).unwrap_or(false);
        let max_results =
            args.get("max_results").and_then(|v| v.as_u64()).unwrap_or(DEFAULT_MAX_RESULTS as u64) as usize;
        let re = RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .build()
            .with_context(|| format!("invalid regex `{pattern}`"))?;

        let search_root = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => confine(&self.root, p, true)?,
            None => self.root.clone(),
        };

        let mut wb = WalkBuilder::new(&search_root);
        wb.git_global(false); // honor the repo's .gitignore, not the dev's global one (reproducible)
        if let Some(glob) = args.get("glob").and_then(|v| v.as_str()) {
            let mut ob = OverrideBuilder::new(&search_root);
            ob.add(glob).with_context(|| format!("invalid glob `{glob}`"))?;
            wb.overrides(ob.build().context("building glob filter")?);
        }

        let mut results: Vec<String> = Vec::new();
        let mut files_hit = 0usize;
        let mut truncated = false;
        'walk: for dent in wb.build() {
            let dent = match dent {
                Ok(d) => d,
                Err(_) => continue,
            };
            if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            if dent.metadata().map(|m| m.len()).unwrap_or(0) > MAX_FILE_BYTES {
                continue;
            }
            let bytes = match std::fs::read(dent.path()) {
                Ok(b) => b,
                Err(_) => continue,
            };
            if bytes.iter().take(BINARY_SNIFF).any(|&b| b == 0) {
                continue; // binary
            }
            let text = String::from_utf8_lossy(&bytes);
            let rel = dent.path().strip_prefix(&self.root).unwrap_or(dent.path());
            let mut this_file = false;
            for (i, line) in text.lines().enumerate() {
                if re.is_match(line) {
                    this_file = true;
                    let trimmed = line.trim_end();
                    let shown: String = if trimmed.chars().count() > MAX_LINE_CHARS {
                        trimmed.chars().take(MAX_LINE_CHARS).collect::<String>() + "…"
                    } else {
                        trimmed.to_string()
                    };
                    results.push(format!("{}:{}: {}", rel.display(), i + 1, shown));
                    if results.len() >= max_results {
                        truncated = true;
                        if this_file {
                            files_hit += 1;
                        }
                        break 'walk;
                    }
                }
            }
            if this_file {
                files_hit += 1;
            }
        }

        if results.is_empty() {
            return Ok(format!("no matches for /{pattern}/"));
        }
        let header = format!("{} match(es) in {files_hit} file(s):", results.len());
        let mut out = header;
        out.push('\n');
        out.push_str(&results.join("\n"));
        if truncated {
            out.push_str(&format!("\n…[capped at {max_results} matches — narrow the pattern or set path/glob]"));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        std::env::current_dir().unwrap().canonicalize().unwrap()
    }

    #[test]
    fn finds_a_known_string_in_the_repo() {
        // Search the crate for a string we KNOW exists (this tool's own name in its description).
        let t = SearchFiles::new(root());
        let out = t
            .execute(&serde_json::json!({"pattern": "canonical content search", "glob": "*.rs"}))
            .unwrap();
        assert!(out.contains("search.rs:"), "should locate this file; got:\n{out}");
        assert!(out.contains("match(es)"));
    }

    #[test]
    fn no_match_reports_cleanly() {
        let t = SearchFiles::new(root());
        // Build the needle at runtime so its literal can't appear contiguously in this source file
        // (a fixed literal would match its own test line — the walker really does find everything).
        let needle = format!("{}{}{}", "zNoMatch", "Tok", "42xQ");
        let out = t.execute(&serde_json::json!({ "pattern": needle })).unwrap();
        assert!(out.starts_with("no matches"), "got: {out}");
    }

    #[test]
    fn bad_regex_errors() {
        let t = SearchFiles::new(root());
        assert!(t.execute(&serde_json::json!({"pattern": "("})).is_err());
        assert!(t.execute(&serde_json::json!({})).is_err()); // missing pattern
    }
}
