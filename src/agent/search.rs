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
use ignore::{WalkBuilder, WalkState};
use regex::RegexBuilder;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Cap on emitted matches (the loop also bounds history growth at the agent layer).
const DEFAULT_MAX_RESULTS: usize = 200;
/// Skip files larger than this (generated bundles / data dumps aren't useful search targets).
const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
/// Bytes sniffed for a NUL to classify a file as binary (then skipped).
const BINARY_SNIFF: usize = 8192;
/// Clip a very long matched line so one minified line can't blow the result.
const MAX_LINE_CHARS: usize = 240;
/// Hard wall-clock cap so a search over a giant tree can't freeze the agent loop (the parallel walk
/// checks this per file and quits every thread once it trips).
const SEARCH_TIMEOUT: Duration = Duration::from_secs(10);
/// Cap on `context` — ±10 lines already shows a whole function around most matches; more is a
/// file read wearing a search costume.
const MAX_CONTEXT: usize = 10;

/// Decode a file's bytes to searchable text, detecting UTF-16 (P2.2). A UTF-16-encoded source file
/// (common on Windows — PowerShell `>` redirection, some editors, .NET logs) is `byte,0,byte,0…`,
/// so the plain NUL sniff would wrongly write it off as binary and the search would silently miss
/// it. We detect UTF-16 by BOM first (unambiguous), then by a strong alternating-NUL heuristic on
/// the head. Returns `None` for genuine binary (so the caller skips it).
pub(crate) fn decode_text(bytes: &[u8]) -> Option<String> {
    // BOM-based detection (unambiguous).
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let u16s: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return Some(String::from_utf16_lossy(&u16s));
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let u16s: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return Some(String::from_utf16_lossy(&u16s));
    }
    let head = &bytes[..bytes.len().min(BINARY_SNIFF)];
    if !head.contains(&0) {
        return Some(String::from_utf8_lossy(bytes).into_owned()); // ordinary UTF-8 / ASCII
    }
    // NULs present without a BOM: could be BOM-less UTF-16, or genuine binary. UTF-16LE ASCII text
    // has NULs on the ODD byte offsets (high byte of each code unit); UTF-16BE on the EVEN offsets.
    // If ~80% of one parity is NUL, treat it as UTF-16 text of that endianness.
    let half = head.len() / 2;
    if half == 0 {
        return None;
    }
    let odd_nul = head.iter().skip(1).step_by(2).filter(|&&b| b == 0).count();
    let even_nul = head.iter().step_by(2).filter(|&&b| b == 0).count();
    if odd_nul * 10 >= half * 8 {
        let u16s: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return Some(String::from_utf16_lossy(&u16s));
    }
    if even_nul * 10 >= half * 8 {
        let u16s: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_be_bytes([c[0], c[1]]))
            .collect();
        return Some(String::from_utf16_lossy(&u16s));
    }
    None // genuine binary
}

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
        "Search file CONTENT by regular expression (`path` may be a ../ or absolute dir elsewhere). \
         Respects .gitignore and skips hidden + binary files unless hidden:true. Returns \
         `path:line: matched text`; context:N adds ±N surrounding lines (grep -C) so a follow-up \
         file_read is rarely needed. The canonical content search — do NOT shell out to \
         grep/ripgrep; file_glob matches NAMES, not content. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "pattern": {"type": "string", "description": "a regular expression to match against each line"},
                "path": {"type": "string", "description": "optional directory to limit the search (a subdir, or a ../ or absolute path elsewhere)"},
                "glob": {"type": "string", "description": "optional file glob to restrict which files are searched, e.g. *.rs or src/**/*.ts"},
                "ignore_case": {"type": "boolean", "description": "case-insensitive match (default false)"},
                "hidden": {"type": "boolean", "description": "also search hidden files and .gitignored paths (default false)"},
                "max_results": {"type": "integer", "description": "cap on matches returned (default 200)"},
                "context": {"type": "integer", "description": "lines of context around each match, grep -C style (default 0, max 10)"}
            },
            "required": ["pattern"]
        })
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let pattern = args
            .get("pattern")
            .and_then(|v| v.as_str())
            .context("missing `pattern`")?;
        let ignore_case = args
            .get("ignore_case")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let max_results = args
            .get("max_results")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_MAX_RESULTS as u64) as usize;
        let ctx = args
            .get("context")
            .and_then(|v| v.as_u64())
            .unwrap_or(0)
            .min(MAX_CONTEXT as u64) as usize;
        let re = RegexBuilder::new(pattern)
            .case_insensitive(ignore_case)
            .build()
            .with_context(|| format!("invalid regex `{pattern}`"))?;

        let search_root = match args.get("path").and_then(|v| v.as_str()) {
            Some(p) => confine(&self.root, p, true)?,
            None => self.root.clone(),
        };

        let show_hidden = args
            .get("hidden")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut wb = WalkBuilder::new(&search_root);
        wb.git_global(false); // honor the repo's .gitignore, not the dev's global one (reproducible)
        if show_hidden {
            // Opt-in "see everything": stop honoring .gitignore / hidden-file rules so the walk
            // reaches dotfiles and ignored paths too (the user asked for nothing to be skipped).
            wb.hidden(false)
                .git_ignore(false)
                .git_exclude(false)
                .ignore(false)
                .parents(false);
        }
        if let Some(glob) = args.get("glob").and_then(|v| v.as_str()) {
            let mut ob = OverrideBuilder::new(&search_root);
            ob.add(glob)
                .with_context(|| format!("invalid glob `{glob}`"))?;
            wb.overrides(ob.build().context("building glob filter")?);
        }

        // A PARALLEL walk (ripgrep's own engine) with a SHARED wall-clock budget: every worker
        // checks the deadline before opening a file and returns `Quit` once it trips, so a search
        // over a huge tree can't freeze the agent loop. Results, the match counter, and the
        // skipped-large-file counter are shared behind a `Mutex`/atomics. Per-file work (read →
        // binary/UTF-16 classify → line-scan) happens on the worker thread.
        // Per file: (path, rows), each row = (1-based line, clipped text, is_match). With
        // context=0 the rows are exactly the match lines (the classic flat shape); with context>0
        // they are the MERGED ±ctx windows around the matches, so an overlapping pair of matches
        // never prints a line twice.
        type FileRows = (PathBuf, Vec<(usize, String, bool)>);
        let results: Mutex<Vec<FileRows>> = Mutex::new(Vec::new());
        let files_hit = AtomicUsize::new(0);
        let match_count = AtomicUsize::new(0);
        let skipped_large = AtomicUsize::new(0);
        let truncated = AtomicBool::new(false);
        let deadline = Instant::now() + SEARCH_TIMEOUT;
        let budget_hit = AtomicBool::new(false);
        let re = &re;
        wb.build_parallel().run(|| {
            Box::new(|dent| {
                // Stop the whole walk once the match cap or the wall-clock budget is reached.
                if match_count.load(Ordering::Relaxed) >= max_results {
                    truncated.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }
                if Instant::now() >= deadline {
                    budget_hit.store(true, Ordering::Relaxed);
                    return WalkState::Quit;
                }
                let dent = match dent {
                    Ok(d) => d,
                    Err(_) => return WalkState::Continue,
                };
                if !dent.file_type().map(|t| t.is_file()).unwrap_or(false) {
                    return WalkState::Continue;
                }
                if dent.metadata().map(|m| m.len()).unwrap_or(0) > MAX_FILE_BYTES {
                    skipped_large.fetch_add(1, Ordering::Relaxed); // P2.3: count, then report
                    return WalkState::Continue;
                }
                let bytes = match std::fs::read(dent.path()) {
                    Ok(b) => b,
                    Err(_) => return WalkState::Continue,
                };
                let text = match decode_text(&bytes) {
                    Some(t) => t,
                    None => return WalkState::Continue, // binary
                };
                let rel = dent
                    .path()
                    .strip_prefix(&self.root)
                    .unwrap_or(dent.path())
                    .to_path_buf();
                // Pass 1: collect this file's match line numbers (1-based) under the global cap.
                let mut hit_lines: Vec<usize> = Vec::new();
                for (i, line) in text.lines().enumerate() {
                    if re.is_match(line) {
                        let n = match_count.fetch_add(1, Ordering::Relaxed);
                        if n >= max_results {
                            truncated.store(true, Ordering::Relaxed);
                            break;
                        }
                        hit_lines.push(i + 1);
                    }
                }
                if hit_lines.is_empty() {
                    return WalkState::Continue;
                }
                files_hit.fetch_add(1, Ordering::Relaxed);
                // Pass 2: materialize rows — the match lines alone, or their merged ±ctx windows.
                let lines: Vec<&str> = text.lines().collect();
                let clip = |line: &str| -> String {
                    let trimmed = line.trim_end();
                    if trimmed.chars().count() > MAX_LINE_CHARS {
                        trimmed.chars().take(MAX_LINE_CHARS).collect::<String>() + "…"
                    } else {
                        trimmed.to_string()
                    }
                };
                let rows: Vec<(usize, String, bool)> = if ctx == 0 {
                    hit_lines
                        .iter()
                        .map(|&n| (n, clip(lines[n - 1]), true))
                        .collect()
                } else {
                    let mut windows: Vec<(usize, usize)> = Vec::new();
                    for &n in &hit_lines {
                        let s = n.saturating_sub(ctx).max(1);
                        let e = (n + ctx).min(lines.len());
                        match windows.last_mut() {
                            // Merge overlapping/adjacent windows so no line prints twice.
                            Some((_, prev_end)) if s <= *prev_end + 1 => {
                                *prev_end = (*prev_end).max(e)
                            }
                            _ => windows.push((s, e)),
                        }
                    }
                    let hits: std::collections::HashSet<usize> =
                        hit_lines.iter().copied().collect();
                    windows
                        .iter()
                        .flat_map(|&(s, e)| {
                            (s..=e).map(|n| (n, clip(lines[n - 1]), hits.contains(&n)))
                        })
                        .collect()
                };
                results.lock().unwrap().push((rel, rows));
                WalkState::Continue
            })
        });

        let mut results = results.into_inner().unwrap();
        // The parallel walk collects in nondeterministic order; sort so output is stable/readable.
        // (Rows within a file are already in line order — built by one worker in one pass.)
        results.sort_by(|a, b| a.0.cmp(&b.0));
        let files_hit = files_hit.load(Ordering::Relaxed);
        let skipped_large = skipped_large.load(Ordering::Relaxed);

        if results.is_empty() {
            let mut msg = format!("no matches for /{pattern}/");
            if skipped_large > 0 {
                msg.push_str(&format!(
                    "\n(skipped {skipped_large} file(s) larger than {} MB — search a specific path if the match is in one)",
                    MAX_FILE_BYTES / (1024 * 1024)
                ));
            }
            if budget_hit.load(Ordering::Relaxed) {
                msg.push_str("\n(search budget reached before the whole tree was scanned — narrow with path/glob)");
            }
            return Ok(msg);
        }
        let shown_matches: usize = results
            .iter()
            .map(|(_, rows)| rows.iter().filter(|r| r.2).count())
            .sum();
        // grep's own convention: `path:line:` marks a MATCH row, `path-line-` a context row, `--`
        // separates non-contiguous groups — a shape every model already knows how to read.
        let mut lines: Vec<String> = Vec::new();
        for (rel, rows) in &results {
            let mut prev: Option<usize> = None;
            for (n, shown, is_match) in rows {
                if !lines.is_empty() && ctx > 0 && prev.is_none_or(|p| *n != p + 1) {
                    lines.push("--".to_string());
                }
                if *is_match {
                    lines.push(format!("{}:{}: {}", rel.display(), n, shown));
                } else {
                    lines.push(format!("{}-{}- {}", rel.display(), n, shown));
                }
                prev = Some(*n);
            }
        }
        let mut out = format!("{shown_matches} match(es) in {files_hit} file(s):");
        out.push('\n');
        out.push_str(&lines.join("\n"));
        if truncated.load(Ordering::Relaxed) {
            out.push_str(&format!(
                "\n…[capped at {max_results} matches — narrow the pattern or set path/glob]"
            ));
        }
        if budget_hit.load(Ordering::Relaxed) {
            out.push_str("\n…[search budget reached before the whole tree was scanned — narrow with path/glob]");
        }
        if skipped_large > 0 {
            out.push_str(&format!(
                "\n…[skipped {skipped_large} file(s) larger than {} MB]",
                MAX_FILE_BYTES / (1024 * 1024)
            ));
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
        assert!(
            out.contains("search.rs:"),
            "should locate this file; got:\n{out}"
        );
        assert!(out.contains("match(es)"));
    }

    #[test]
    fn no_match_reports_cleanly() {
        let t = SearchFiles::new(root());
        // Build the needle at runtime so its literal can't appear contiguously in this source file
        // (a fixed literal would match its own test line — the walker really does find everything).
        let needle = format!("{}{}{}", "zNoMatch", "Tok", "42xQ");
        let out = t
            .execute(&serde_json::json!({ "pattern": needle }))
            .unwrap();
        assert!(out.starts_with("no matches"), "got: {out}");
    }

    #[test]
    fn bad_regex_errors() {
        let t = SearchFiles::new(root());
        assert!(t.execute(&serde_json::json!({"pattern": "("})).is_err());
        assert!(t.execute(&serde_json::json!({})).is_err()); // missing pattern
    }

    fn tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("aizen-search-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.canonicalize().unwrap()
    }

    #[test]
    fn decode_text_handles_utf16_bom_le_and_be() {
        // P2.2: a UTF-16 file (BOM'd) must decode to searchable text, not be dropped as binary.
        let le = {
            let mut b = vec![0xFF, 0xFE];
            for u in "hello".encode_utf16() {
                b.extend_from_slice(&u.to_le_bytes());
            }
            b
        };
        assert_eq!(decode_text(&le).as_deref(), Some("hello"));
        let be = {
            let mut b = vec![0xFE, 0xFF];
            for u in "hello".encode_utf16() {
                b.extend_from_slice(&u.to_be_bytes());
            }
            b
        };
        assert_eq!(decode_text(&be).as_deref(), Some("hello"));
    }

    #[test]
    fn decode_text_handles_bomless_utf16le() {
        // BOM-less UTF-16LE ASCII: NUL on every odd byte. The alternating-NUL heuristic must catch it
        // instead of the plain NUL sniff writing it off as binary.
        let mut b = Vec::new();
        for u in "function main".encode_utf16() {
            b.extend_from_slice(&u.to_le_bytes());
        }
        let decoded = decode_text(&b).expect("bom-less utf16le should decode as text");
        assert!(decoded.contains("function main"), "got: {decoded:?}");
    }

    #[test]
    fn decode_text_rejects_real_binary() {
        // Genuine binary: NULs present but scattered across BOTH parities (neither the odd nor the
        // even offsets are dominated by NUL), so neither the BOM check nor the alternating-NUL
        // UTF-16 heuristic fires — it stays classified as binary and is skipped.
        let b = vec![0x00, 0x01, 0x02, 0x03, 0x04, 0xFF, 0x89, 0x13, 0x37, 0x00];
        assert_eq!(decode_text(&b), None);
    }

    #[test]
    fn utf16_file_is_searched_end_to_end() {
        // A UTF-16LE-BOM file on disk must be found by a content search (regression for the old NUL
        // sniff that skipped it).
        let root = tmp("utf16");
        let mut bytes = vec![0xFF, 0xFE];
        for u in "let secret = 42;".encode_utf16() {
            bytes.extend_from_slice(&u.to_le_bytes());
        }
        std::fs::write(root.join("cfg.ts"), &bytes).unwrap();
        let t = SearchFiles::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern": "secret"}))
            .unwrap();
        assert!(out.contains("cfg.ts"), "utf-16 file searched: {out}");
    }

    #[test]
    fn context_lines_wrap_matches_and_merge_overlapping_windows() {
        let root = tmp("context");
        // Lines 1..=12; matches at 4 and 6 (windows ±2 overlap → ONE merged group 2..=8) and at 12.
        let body = (1..=12)
            .map(|i| {
                if i == 4 || i == 6 || i == 12 {
                    format!("needle line {i}")
                } else {
                    format!("plain line {i}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(root.join("ctx.txt"), body).unwrap();
        let t = SearchFiles::new(root);
        let out = t
            .execute(&serde_json::json!({"pattern": "needle", "context": 2}))
            .unwrap();
        assert!(
            out.starts_with("3 match(es) in 1 file(s):"),
            "header counts MATCHES, not rows: {out}"
        );
        // Match rows use `path:line:`, context rows `path-line-` (grep -C convention).
        assert!(out.contains("ctx.txt:4: needle line 4"), "{out}");
        assert!(out.contains("ctx.txt-3- plain line 3"), "{out}");
        // Overlapping windows merged: line 5 (between the 4 and 6 matches) appears exactly once.
        assert_eq!(out.matches("plain line 5").count(), 1, "{out}");
        // Non-contiguous groups separated by `--`: the 2..=8 block vs the 10..=12 block.
        assert!(out.contains("--"), "{out}");
        assert!(
            !out.contains("plain line 9"),
            "gap line outside both windows: {out}"
        );
        // context:0 (default) keeps the classic flat shape.
        let flat = t2_flat(&t);
        assert!(flat.contains("ctx.txt:4: needle line 4"), "{flat}");
        assert!(!flat.contains("-3-"), "no context rows by default: {flat}");
    }

    fn t2_flat(t: &SearchFiles) -> String {
        t.execute(&serde_json::json!({"pattern": "needle"}))
            .unwrap()
    }

    #[test]
    fn reports_skipped_oversized_files() {
        // P2.3: a file over the size cap is skipped but REPORTED so the model knows to target it.
        let root = tmp("skip-large");
        let big = "x".repeat((MAX_FILE_BYTES + 1024) as usize);
        std::fs::write(root.join("huge.log"), &big).unwrap();
        std::fs::write(root.join("small.txt"), "nothing here").unwrap();
        let t = SearchFiles::new(root);
        let out = t.execute(&serde_json::json!({"pattern": "xxxxx"})).unwrap();
        assert!(out.contains("skipped 1 file"), "skip reported: {out}");
    }
}
