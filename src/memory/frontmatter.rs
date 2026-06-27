//! Minimal, robust frontmatter for the CLI's own markdown memory format.
//!
//! Standalone (no extension-interop divergence risk), so we use a small hand-rolled
//! `key: value` parser instead of pulling a full-YAML crate. Strict-and-skip: never
//! errors; a malformed/absent fence just yields `had_frontmatter=false` + verbatim body.
//!
//! On-disk shape (one fact per file):
//! ```text
//! ---
//! name: auth-strategy
//! description: short one-liner
//! type: reference
//! created: 2026-06-20
//! ---
//! body markdown...
//! ```

use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Frontmatter {
    pub fields: BTreeMap<String, String>,
    pub body: String,
    pub had_frontmatter: bool,
}

impl Frontmatter {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.fields.get(key).map(|s| s.as_str())
    }
}

/// Normalize line endings + strip a leading UTF-8 BOM.
fn normalize(input: &str) -> String {
    let s = input.strip_prefix('\u{FEFF}').unwrap_or(input);
    s.replace("\r\n", "\n").replace('\r', "\n")
}

/// Unquote a scalar value (`"x"` / `'x'` → `x`), trimming surrounding whitespace.
fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2
        && ((v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')))
    {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

/// Parse a memory markdown file into frontmatter fields + body.
pub fn parse(input: &str) -> Frontmatter {
    let text = normalize(input);

    // Frontmatter only if the first non-empty content is a `---` fence on its own line.
    if !text.starts_with("---\n") && text != "---" && !text.starts_with("---\r") {
        // also tolerate a leading blank line before the fence
        let trimmed_start = text.trim_start_matches('\n');
        if !trimmed_start.starts_with("---\n") {
            return Frontmatter {
                fields: BTreeMap::new(),
                body: text,
                had_frontmatter: false,
            };
        }
    }

    let after_open = match text.strip_prefix("---\n") {
        Some(rest) => rest,
        None => {
            // leading blank lines case
            let t = text.trim_start_matches('\n');
            match t.strip_prefix("---\n") {
                Some(rest) => rest,
                None => {
                    return Frontmatter {
                        fields: BTreeMap::new(),
                        body: text,
                        had_frontmatter: false,
                    }
                }
            }
        }
    };

    // Find the closing fence: a line that is exactly `---`.
    let mut fields = BTreeMap::new();
    let mut closed = false;
    let mut body_start = after_open.len();
    let mut offset = 0usize;
    for line in after_open.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n');
        if trimmed == "---" {
            closed = true;
            body_start = offset + line.len();
            break;
        }
        // key: value (skip blanks + `#` comments)
        let t = trimmed.trim();
        if !t.is_empty() && !t.starts_with('#') {
            if let Some(idx) = t.find(':') {
                let key = t[..idx].trim().to_string();
                let val = unquote(&t[idx + 1..]);
                if !key.is_empty() {
                    fields.insert(key, val);
                }
            }
        }
        offset += line.len();
    }

    if !closed {
        // Open-ended fence → treat whole input as body, no frontmatter.
        return Frontmatter {
            fields: BTreeMap::new(),
            body: text,
            had_frontmatter: false,
        };
    }

    let body = after_open[body_start..]
        .trim_start_matches('\n')
        .trim_end()
        .to_string();
    Frontmatter {
        fields,
        body,
        had_frontmatter: true,
    }
}

/// Serialize frontmatter fields + body back to disk form.
/// `key_order` pins a stable field order; any remaining fields follow sorted.
pub fn serialize(fields: &BTreeMap<String, String>, body: &str, key_order: &[&str]) -> String {
    let mut out = String::from("---\n");
    let mut written = std::collections::HashSet::new();
    for k in key_order {
        if let Some(v) = fields.get(*k) {
            out.push_str(&format!("{}: {}\n", k, v));
            written.insert(*k);
        }
    }
    for (k, v) in fields {
        if !written.contains(k.as_str()) {
            out.push_str(&format!("{}: {}\n", k, v));
        }
    }
    out.push_str("---\n\n");
    out.push_str(body.trim_end());
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic() {
        let fm = parse("---\nname: auth\ntype: reference\n---\nbody here\n");
        assert!(fm.had_frontmatter);
        assert_eq!(fm.get("name"), Some("auth"));
        assert_eq!(fm.get("type"), Some("reference"));
        assert_eq!(fm.body, "body here");
    }

    #[test]
    fn strips_bom_and_crlf() {
        let fm = parse("\u{FEFF}---\r\nname: x\r\n---\r\nb\r\n");
        assert!(fm.had_frontmatter);
        assert_eq!(fm.get("name"), Some("x"));
        assert_eq!(fm.body, "b");
    }

    #[test]
    fn no_frontmatter_keeps_body() {
        let fm = parse("just some text\nno fence");
        assert!(!fm.had_frontmatter);
        assert_eq!(fm.body, "just some text\nno fence");
    }

    #[test]
    fn open_fence_is_not_frontmatter() {
        let fm = parse("---\nname: x\nnever closed\n");
        assert!(!fm.had_frontmatter);
    }

    #[test]
    fn unquotes_and_skips_comments() {
        let fm = parse("---\n# a comment\nname: \"quoted val\"\n---\nb");
        assert_eq!(fm.get("name"), Some("quoted val"));
    }

    #[test]
    fn round_trip() {
        let mut f = BTreeMap::new();
        f.insert("name".to_string(), "auth".to_string());
        f.insert("type".to_string(), "reference".to_string());
        let s = serialize(&f, "the body", &["name", "description", "type"]);
        let fm = parse(&s);
        assert_eq!(fm.get("name"), Some("auth"));
        assert_eq!(fm.body, "the body");
    }
}
