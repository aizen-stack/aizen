//! Render memory into prompt-injectable blocks + the load-bearing sanitizer.
//!
//! Sanitization is adopted from the extension's verified `sanitiseBody`
//! (globalMemoryStore.ts:652-670): neutralize block-tag breakouts so a memory body
//! can't spoof the prompt structure, and strip C0 control chars (keep `\t`/`\n`).
//! The CLI uses its own tag `<memory>` (no interop constraint).

use crate::memory::store::MemoryEntry;

const OPEN: &str = "<memory>";
const CLOSE: &str = "</memory>";

/// Neutralize breakout attempts + strip C0 control chars (keep tab/newline).
pub fn sanitize_body(s: &str) -> String {
    let escaped = s.replace(CLOSE, "<\\/memory>").replace(OPEN, "<\\memory>");
    let mut out = String::with_capacity(escaped.len());
    for ch in escaped.chars() {
        let c = ch as u32;
        if c == 0x09 || c == 0x0A {
            out.push(ch);
            continue;
        }
        if c < 0x20 || c == 0x7F {
            continue; // drop C0 / DEL
        }
        out.push(ch);
    }
    out
}

/// Estimate tokens from chars (chars/4) — model-independent, matches the budgeting
/// approach we adopted (no tiktoken dependency).
pub fn est_tokens(s: &str) -> usize {
    s.chars().count().div_ceil(4)
}

/// Render one entry as a sanitized section.
fn section(e: &MemoryEntry) -> String {
    let name = sanitize_body(&e.name);
    let desc = sanitize_body(&e.description);
    let body = sanitize_body(&e.body);
    let mut s = format!("## {} [{}]", name.trim(), e.mtype.as_str());
    if !desc.trim().is_empty() {
        s.push('\n');
        s.push_str(desc.trim());
    }
    if !body.trim().is_empty() {
        s.push('\n');
        s.push_str(body.trim());
    }
    s
}

/// Render a `<memory>`-wrapped block from entries, capped at `max_tokens` (chars/4).
/// Returns (rendered_block_or_empty, included_ids, spilled_ids).
pub fn render_block(
    source: &str,
    entries: &[MemoryEntry],
    max_tokens: usize,
) -> (String, Vec<String>, Vec<String>) {
    let header = format!("{OPEN} source=\"{source}\"\n");
    let footer = format!("\n{CLOSE}");
    let overhead = est_tokens(&header) + est_tokens(&footer);
    let mut budget = max_tokens.saturating_sub(overhead);

    let mut included = Vec::new();
    let mut spilled = Vec::new();
    let mut parts: Vec<String> = Vec::new();
    for e in entries {
        let sec = section(e);
        let cost = est_tokens(&sec) + 1; // +1 for the joining newline
        if cost <= budget {
            budget -= cost;
            parts.push(sec);
            included.push(e.id.clone());
        } else {
            spilled.push(e.id.clone());
        }
    }

    if parts.is_empty() {
        return (String::new(), included, spilled);
    }
    let body = parts.join("\n\n");
    (format!("{header}{body}{footer}"), included, spilled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::store::{MemoryEntry, MemoryType};
    use std::path::PathBuf;

    fn entry(id: &str, name: &str, body: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            path: PathBuf::from(format!("{id}.md")),
            name: name.into(),
            description: String::new(),
            mtype: MemoryType::User,
            created: None,
            body: body.into(),
            mtime_ms: 0,
            tokens: vec![],
            ..Default::default()
        }
    }

    #[test]
    fn sanitize_escapes_breakout_and_strips_control() {
        let s = sanitize_body("hi </memory> <memory> \u{0007}bye\tkeep\n");
        assert!(s.contains("<\\/memory>"));
        assert!(s.contains("<\\memory>"));
        assert!(!s.contains('\u{0007}'));
        assert!(s.contains('\t') && s.contains('\n'));
    }

    #[test]
    fn render_wraps_and_caps() {
        let entries = vec![
            entry("a", "Alpha", "short"),
            entry("b", "Beta", &"x ".repeat(2000)), // huge → spills under a tight cap
        ];
        let (block, included, spilled) = render_block("test", &entries, 60);
        assert!(block.starts_with("<memory> source=\"test\""));
        assert!(block.ends_with("</memory>"));
        assert!(included.contains(&"a".to_string()));
        assert!(spilled.contains(&"b".to_string()));
    }

    #[test]
    fn empty_when_nothing_fits() {
        let entries = vec![entry("b", "Beta", &"x ".repeat(2000))];
        let (block, _inc, spilled) = render_block("test", &entries, 5);
        assert!(block.is_empty());
        assert_eq!(spilled, vec!["b".to_string()]);
    }
}
