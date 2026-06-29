//! A streaming, dependency-free Markdown renderer for the assistant's reply — the single biggest
//! lever for a "clean" TUI. The model streams raw Markdown (`**bold**`, `## heads`, ` ```code``` `,
//! `- bullets`, links); without this it prints the literal markers. We render it **line-buffered**:
//! input is fed in via [`MarkdownStream::push`], complete lines are styled and returned (the caller
//! flushes them through `tui::emit`), and the trailing partial line is held until its newline. The
//! pinned box's "⚡ working…" indicator covers the brief gap while a line completes.
//!
//! Design notes:
//! - **History stays raw.** This only styles the DISPLAY; the caller keeps the raw Markdown for the
//!   conversation history, so the model never sees our decoration.
//! - **Passthrough off-TTY.** Constructed with `decorate=false` for pipes/CI → `push` returns the
//!   text verbatim (no gutter, no borders, no ANSI), so captured output is byte-identical to before.
//! - **Gutter.** Every assistant line carries a moonlight `▌ ` gutter — a continuous bar that marks
//!   the whole turn as the assistant's voice and separates it from `❯` user echoes and `⚙` tool traces.
//! - **Best-effort syntax highlight.** Code-fence bodies get a light, language-aware pass (strings /
//!   comments / numbers / keywords). It never mangles code: anything unrecognised stays default.

use crate::ui::theme;
use console::{measure_text_width, style};

/// A left-edge bar marking assistant output. Moonlight silver — the design's `border-left:2px #c3ccd8`.
fn gutter() -> String {
    format!("{} ", theme::accent("▌"))
}

/// Streaming Markdown → styled-terminal renderer. One per assistant turn.
pub struct MarkdownStream {
    decorate: bool,
    cols: usize,
    pending: String,
    in_fence: bool,
    fence_lang: String,
}

impl MarkdownStream {
    /// `decorate=false` (non-TTY) makes every method a verbatim passthrough.
    pub fn new(decorate: bool, cols: usize) -> Self {
        Self { decorate, cols: cols.max(24), pending: String::new(), in_fence: false, fence_lang: String::new() }
    }

    /// Feed a streamed delta; returns the text ready to print (complete styled lines). Empty when
    /// the delta only extended the in-progress line.
    pub fn push(&mut self, text: &str) -> String {
        if !self.decorate {
            return text.to_string();
        }
        self.pending.push_str(text);
        let mut out = String::new();
        while let Some(nl) = self.pending.find('\n') {
            let line: String = self.pending[..nl].to_string();
            self.pending.drain(..=nl);
            self.render_line(&line, &mut out);
        }
        out
    }

    /// Flush the final partial line (and close an unterminated code fence) at end of turn.
    pub fn finish(&mut self) -> String {
        if !self.decorate {
            return String::new();
        }
        let mut out = String::new();
        if !self.pending.is_empty() {
            let line = std::mem::take(&mut self.pending);
            self.render_line(&line, &mut out);
        }
        if self.in_fence {
            out.push_str(&self.fence_bottom());
            self.in_fence = false;
        }
        out
    }

    // ── line rendering ───────────────────────────────────────────────────────────
    fn render_line(&mut self, line: &str, out: &mut String) {
        let ls = line.trim_start();

        if self.in_fence {
            if ls.starts_with("```") {
                out.push_str(&self.fence_bottom());
                self.in_fence = false;
            } else {
                self.emit_code(out, line);
            }
            return;
        }

        // code fence open: ```lang
        if ls.starts_with("```") {
            self.in_fence = true;
            self.fence_lang = ls.trim_start_matches('`').trim().to_string();
            out.push_str(&self.fence_top());
            return;
        }

        // horizontal rule
        if is_hr(ls) {
            out.push_str(&gutter());
            out.push_str(&theme::faint("─".repeat(self.cols.min(48).saturating_sub(2))).to_string());
            out.push('\n');
            return;
        }

        // heading: #..###### text
        if let Some((level, text)) = heading(ls) {
            let g = gutter();
            self.emit_block(out, &g, &g, text, &move |row| {
                if level <= 2 {
                    theme::accent(inline(row)).bold().underlined().to_string()
                } else {
                    theme::accent(inline(row)).bold().to_string()
                }
            });
            return;
        }

        // blockquote: > text
        if let Some(rest) = ls.strip_prefix('>') {
            let first = format!("{}{} ", gutter(), style("▏").color256(theme::FAINT));
            let cont = self.cont_prefix(measure_text_width(&first));
            self.emit_block(out, &first, &cont, rest.trim_start(), &|row| {
                theme::muted(inline(row)).italic().to_string()
            });
            return;
        }

        // bullet: - / * / + then space
        if let Some(rest) = bullet_rest(ls) {
            let first = format!("{}  {} ", gutter(), theme::accent("•"));
            let cont = self.cont_prefix(measure_text_width(&first));
            self.emit_block(out, &first, &cont, rest, &|row| inline(row));
            return;
        }

        // numbered: 1. text
        if let Some((num, rest)) = number_rest(ls) {
            let first = format!("{}  {} ", gutter(), theme::accent(format!("{num}.")));
            let cont = self.cont_prefix(measure_text_width(&first));
            self.emit_block(out, &first, &cont, rest, &|row| inline(row));
            return;
        }

        // blank line → continuous gutter (rhythm)
        if ls.is_empty() {
            out.push_str(theme::accent("▌").to_string().as_str());
            out.push('\n');
            return;
        }

        // normal prose
        let g = gutter();
        self.emit_block(out, &g, &g, line, &|row| inline(row));
    }

    /// Emit `text` as one or more rows, word-wrapped to the terminal width (gutter/indent kept on
    /// every row). `first_prefix` leads the first row, `cont_prefix` the wrapped continuations; both
    /// already carry the gold gutter bar so the left edge stays continuous. `style_each` styles the
    /// per-row visible text (inline markdown, heading bold, …). This is what stops a long single-line
    /// paragraph from running off the right edge of the window.
    fn emit_block(&self, out: &mut String, first_prefix: &str, cont_prefix: &str, text: &str, style_each: &dyn Fn(&str) -> String) {
        let prefix_w = measure_text_width(first_prefix);
        let budget = self.cols.saturating_sub(prefix_w).max(8);
        let rows = wrap_plain(text, budget);
        if rows.is_empty() {
            out.push_str(first_prefix);
            out.push('\n');
            return;
        }
        for (i, row) in rows.iter().enumerate() {
            out.push_str(if i == 0 { first_prefix } else { cont_prefix });
            out.push_str(&style_each(row));
            out.push('\n');
        }
    }

    /// A code-fence body line: `▌ │ <highlighted>`, char-wrapped (never word-wrapped — that would
    /// corrupt code) to the width, the gutter + rule repeated on each wrapped row.
    fn emit_code(&self, out: &mut String, line: &str) {
        let prefix = format!("{}{}", gutter(), style("│ ").color256(theme::CODE_RULE));
        let budget = self.cols.saturating_sub(measure_text_width(&prefix)).max(8);
        for row in char_chunks(line, budget) {
            out.push_str(&prefix);
            out.push_str(&highlight(&row, &self.fence_lang));
            out.push('\n');
        }
    }

    /// A continuation prefix that re-draws the gutter bar then pads to `prefix_w` so wrapped rows
    /// line up under the first row's text.
    fn cont_prefix(&self, prefix_w: usize) -> String {
        format!("{}{}", gutter(), " ".repeat(prefix_w.saturating_sub(2)))
    }

    /// `▌ ╭─ lang ───────╮`
    fn fence_top(&self) -> String {
        let label = if self.fence_lang.is_empty() { "code" } else { self.fence_lang.as_str() };
        let w = self.cols.min(60).saturating_sub(4).max(14);
        let head = format!("╭─ {label} ");
        let dashes = w.saturating_sub(head.chars().count()).saturating_sub(1);
        format!(
            "{}{}{}{}\n",
            gutter(),
            theme::accent_dim("╭─ "),
            theme::accent(label),
            theme::accent_dim(format!(" {}╮", "─".repeat(dashes))),
        )
    }

    /// `▌ ╰──────────────╯`
    fn fence_bottom(&self) -> String {
        let w = self.cols.min(60).saturating_sub(4).max(14);
        format!("{}{}\n", gutter(), theme::accent_dim(format!("╰{}╯", "─".repeat(w.saturating_sub(2)))))
    }
}

// ── wrapping ───────────────────────────────────────────────────────────────────────
/// Greedy word-wrap to a visible-width `budget`. Words longer than the budget are hard-split. Width
/// is measured (not byte length) so Vietnamese diacritics / wide glyphs count correctly. Operates on
/// PLAIN text (styling is applied per-row afterwards), so it never splits an ANSI escape.
fn wrap_plain(text: &str, budget: usize) -> Vec<String> {
    let budget = budget.max(8);
    let mut rows: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for word in text.split_whitespace() {
        let ww = measure_text_width(word);
        if ww > budget {
            // a single over-long token (URL, long path) → flush, then hard-split it
            if !cur.is_empty() {
                rows.push(std::mem::take(&mut cur));
            }
            let mut chunks = char_chunks(word, budget);
            cur = chunks.pop().unwrap_or_default();
            rows.extend(chunks);
            cur_w = measure_text_width(&cur);
            continue;
        }
        if cur.is_empty() {
            cur = word.to_string();
            cur_w = ww;
        } else if cur_w + 1 + ww <= budget {
            cur.push(' ');
            cur.push_str(word);
            cur_w += 1 + ww;
        } else {
            rows.push(std::mem::take(&mut cur));
            cur = word.to_string();
            cur_w = ww;
        }
    }
    if !cur.is_empty() {
        rows.push(cur);
    }
    rows
}

/// Split into pieces of at most `budget` chars (a coarse width proxy; code is ~all single-width).
fn char_chunks(s: &str, budget: usize) -> Vec<String> {
    let budget = budget.max(4);
    let chars: Vec<char> = s.chars().collect();
    if chars.is_empty() {
        return vec![String::new()];
    }
    chars.chunks(budget).map(|c| c.iter().collect()).collect()
}

// ── block classifiers ────────────────────────────────────────────────────────────
fn heading(ls: &str) -> Option<(u8, &str)> {
    let hashes = ls.chars().take_while(|&c| c == '#').count();
    if (1..=6).contains(&hashes) && ls[hashes..].starts_with(' ') {
        Some((hashes as u8, ls[hashes..].trim_start()))
    } else {
        None
    }
}

fn is_hr(ls: &str) -> bool {
    let t = ls.trim();
    (t.len() >= 3) && (t.chars().all(|c| c == '-') || t.chars().all(|c| c == '*') || t.chars().all(|c| c == '_'))
}

fn bullet_rest(ls: &str) -> Option<&str> {
    for m in ["- ", "* ", "+ "] {
        if let Some(rest) = ls.strip_prefix(m) {
            return Some(rest);
        }
    }
    None
}

fn number_rest(ls: &str) -> Option<(String, &str)> {
    let digits: String = ls.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() || digits.len() > 3 {
        return None;
    }
    let after = &ls[digits.len()..];
    if let Some(rest) = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
        Some((digits, rest))
    } else {
        None
    }
}

// ── inline rendering (bold / italic / code / links) ───────────────────────────────
fn find_char(chars: &[char], from: usize, target: char) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i] == target)
}

/// Render inline Markdown within a single (already block-classified) line. Unclosed markers are left
/// literal so partial/odd input never produces garbled styling.
pub fn inline(s: &str) -> String {
    if !s.contains(['*', '_', '`', '[']) {
        return s.to_string(); // fast path: nothing to style
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // inline code `code`
        if c == '`' {
            if let Some(close) = find_char(&chars, i + 1, '`') {
                if close > i + 1 {
                    let code: String = chars[i + 1..close].iter().collect();
                    out.push_str(&style(code).color256(theme::LINK).to_string());
                    i = close + 1;
                    continue;
                }
            }
        }
        // bold **text**
        if c == '*' && chars.get(i + 1) == Some(&'*') {
            if let Some(close) = find_seq2(&chars, i + 2, '*') {
                let inner: String = chars[i + 2..close].iter().collect();
                out.push_str(&style(inline(&inner)).bold().to_string());
                i = close + 2;
                continue;
            }
        }
        // italic *text* or _text_
        if c == '*' || c == '_' {
            if let Some(close) = find_char(&chars, i + 1, c) {
                let inner: String = chars[i + 1..close].iter().collect();
                if !inner.is_empty() && !inner.contains(c) && !inner.starts_with(' ') {
                    out.push_str(&style(inner).italic().to_string());
                    i = close + 1;
                    continue;
                }
            }
        }
        // link [text](url)
        if c == '[' {
            if let Some(rb) = find_char(&chars, i + 1, ']') {
                if chars.get(rb + 1) == Some(&'(') {
                    if let Some(rp) = find_char(&chars, rb + 2, ')') {
                        let text: String = chars[i + 1..rb].iter().collect();
                        let url: String = chars[rb + 2..rp].iter().collect();
                        out.push_str(&theme::link(text).underlined().to_string());
                        out.push_str(&theme::faint(format!(" ({url})")).to_string());
                        i = rp + 1;
                        continue;
                    }
                }
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Find the next position `j>=from` where `chars[j]==delim && chars[j+1]==delim` (a `**` close).
fn find_seq2(chars: &[char], from: usize, delim: char) -> Option<usize> {
    let mut j = from;
    while j + 1 < chars.len() {
        if chars[j] == delim && chars[j + 1] == delim {
            return Some(j);
        }
        j += 1;
    }
    None
}

// ── light syntax highlighting (code-fence bodies) ──────────────────────────────────
fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}
fn is_ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Comment lead-ins by language family. `#` is only a comment in hash-comment langs (py/sh/yaml/…),
/// never in C-likes, so a `#include` / Rust attribute isn't greyed out wrongly.
fn comment_tokens(lang: &str) -> &'static [&'static str] {
    let l = lang.to_ascii_lowercase();
    match l.as_str() {
        "py" | "python" | "sh" | "bash" | "zsh" | "yaml" | "yml" | "toml" | "ruby" | "rb" | "r" => &["#"],
        "sql" | "lua" | "haskell" | "hs" => &["--"],
        "lisp" | "clojure" | "scheme" => &[";"],
        "" => &["//", "#"], // unknown fence → accept both common forms
        _ => &["//"],       // rust / js / ts / go / c / c++ / java / c# / json5 / …
    }
}

fn starts_with_at(chars: &[char], i: usize, tok: &str) -> bool {
    let t: Vec<char> = tok.chars().collect();
    i + t.len() <= chars.len() && (0..t.len()).all(|k| chars[i + k] == t[k])
}

const KEYWORDS: &[&str] = &[
    "fn", "let", "const", "var", "function", "def", "class", "struct", "enum", "impl", "trait", "pub",
    "return", "if", "else", "elif", "for", "while", "loop", "match", "case", "switch", "break",
    "continue", "import", "from", "use", "mod", "package", "async", "await", "yield", "type",
    "interface", "public", "private", "protected", "static", "final", "void", "new", "self", "this",
    "super", "true", "false", "null", "none", "nil", "and", "or", "not", "in", "is", "as", "with",
    "try", "catch", "except", "finally", "throw", "raise", "defer", "go", "func", "where", "extends",
];

fn is_keyword(w: &str) -> bool {
    KEYWORDS.contains(&w)
}

/// Best-effort, per-line syntax highlight. Conservative: only strings, comments, numbers and a union
/// keyword set are coloured; everything else is left as-is (so it never corrupts unfamiliar code).
fn highlight(line: &str, lang: &str) -> String {
    if line.trim().is_empty() {
        return line.to_string();
    }
    let comments = comment_tokens(lang);
    let chars: Vec<char> = line.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        // comment → to end of line
        if comments.iter().any(|t| starts_with_at(&chars, i, t)) {
            let rest: String = chars[i..].iter().collect();
            out.push_str(&style(rest).color256(theme::CODE_COMMENT).to_string());
            break;
        }
        // string literal (" or ')
        if c == '"' || c == '\'' {
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j] == '\\' {
                    j += 2;
                    continue;
                }
                if chars[j] == c {
                    j += 1;
                    break;
                }
                j += 1;
            }
            let s: String = chars[i..j.min(chars.len())].iter().collect();
            out.push_str(&style(s).color256(theme::CODE_STRING).to_string());
            i = j;
            continue;
        }
        // number
        if c.is_ascii_digit() && (i == 0 || !is_ident_char(chars[i - 1])) {
            let mut j = i;
            while j < chars.len() && (chars[j].is_ascii_alphanumeric() || chars[j] == '.' || chars[j] == '_') {
                j += 1;
            }
            let s: String = chars[i..j].iter().collect();
            out.push_str(&style(s).color256(theme::CODE_NUMBER).to_string());
            i = j;
            continue;
        }
        // identifier / keyword
        if is_ident_start(c) {
            let mut j = i;
            while j < chars.len() && is_ident_char(chars[j]) {
                j += 1;
            }
            let word: String = chars[i..j].iter().collect();
            if is_keyword(&word) {
                out.push_str(&style(&word).color256(theme::CODE_KEYWORD).to_string());
            } else {
                out.push_str(&word);
            }
            i = j;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use console::strip_ansi_codes;

    fn render_all(md: &str) -> String {
        let mut s = MarkdownStream::new(true, 80);
        let mut out = s.push(md);
        out.push_str(&s.finish());
        out
    }

    #[test]
    fn passthrough_when_not_decorating() {
        let mut s = MarkdownStream::new(false, 80);
        let raw = "**bold** and `code`\n# not a heading here";
        assert_eq!(s.push(raw), raw, "non-TTY must be byte-identical passthrough");
        assert_eq!(s.finish(), "");
    }

    #[test]
    fn strips_inline_markers() {
        let out = strip_ansi_codes(&render_all("a **bold** and *it* and `c` word\n")).to_string();
        assert!(!out.contains("**"), "bold markers consumed: {out:?}");
        assert!(out.contains("bold") && out.contains("it") && out.contains('c'));
        // the lone * / ` delimiters are gone
        assert!(!out.contains('`'), "inline-code backticks consumed: {out:?}");
    }

    #[test]
    fn heading_drops_hashes_and_gutters() {
        let out = strip_ansi_codes(&render_all("## Title here\n")).to_string();
        assert!(out.contains("Title here"));
        assert!(!out.contains('#'), "heading hashes removed: {out:?}");
        assert!(out.contains('▌'), "assistant gutter present");
    }

    #[test]
    fn bullets_and_numbers_render() {
        let out = strip_ansi_codes(&render_all("- one\n- two\n1. first\n")).to_string();
        assert!(out.contains("• one") && out.contains("• two"), "bullets: {out:?}");
        assert!(out.contains("1. first"), "numbered: {out:?}");
    }

    #[test]
    fn code_fence_gets_box_and_holds_lines() {
        let mut s = MarkdownStream::new(true, 80);
        // opening fence line alone: should NOT yet print the code, but should print a top border
        let top = strip_ansi_codes(&s.push("```rust\n")).to_string();
        assert!(top.contains("╭") && top.contains("rust"), "fence top with lang: {top:?}");
        let body = strip_ansi_codes(&s.push("let x = 1;\n")).to_string();
        assert!(body.contains("let x = 1;"), "code body preserved verbatim: {body:?}");
        assert!(body.contains('│'), "code body has the left rule");
        let bottom = strip_ansi_codes(&s.push("```\n")).to_string();
        assert!(bottom.contains("╰"), "fence bottom border: {bottom:?}");
    }

    #[test]
    fn partial_line_held_until_newline() {
        let mut s = MarkdownStream::new(true, 80);
        assert_eq!(s.push("no newline yet"), "", "a line with no \\n is buffered");
        let out = s.push(" done\n");
        assert!(strip_ansi_codes(&out).contains("no newline yet done"), "completed line flushes whole: {out:?}");
    }

    #[test]
    fn unterminated_fence_closed_on_finish() {
        let mut s = MarkdownStream::new(true, 80);
        s.push("```\ncode without close\n");
        let tail = strip_ansi_codes(&s.finish()).to_string();
        assert!(tail.contains("╰"), "finish() closes a dangling fence: {tail:?}");
    }

    #[test]
    fn highlight_keeps_code_text() {
        // highlighting must never drop/alter the underlying characters.
        let h = strip_ansi_codes(&highlight("let s = \"hi\"; // note", "rust")).to_string();
        assert_eq!(h, "let s = \"hi\"; // note");
    }

    #[test]
    fn long_line_wraps_to_width() {
        // The bug this guards: a long single-line paragraph (no internal \n) used to run off the
        // right edge of the window. Every rendered row must now fit the width AND keep the gutter.
        let cols = 40;
        let mut s = MarkdownStream::new(true, cols);
        let long = "word ".repeat(30); // ~150 chars, one logical line
        let out = s.push(&format!("{long}\n"));
        let plain = strip_ansi_codes(&out).to_string();
        let lines: Vec<&str> = plain.lines().collect();
        assert!(lines.len() > 1, "a 150-char line must wrap into several rows");
        for l in &lines {
            assert!(measure_text_width(l) <= cols, "row wider than the window: {l:?} = {}", measure_text_width(l));
            assert!(l.contains('▌'), "every wrapped row keeps the gutter: {l:?}");
        }
    }

    #[test]
    fn over_long_word_hard_splits() {
        // A token with no spaces (e.g. a URL) longer than the width must still be broken up.
        let url = "x".repeat(120);
        let rows = wrap_plain(&url, 30);
        assert!(rows.len() >= 4, "long unbroken token splits: {} rows", rows.len());
        assert!(rows.iter().all(|r| measure_text_width(r) <= 30));
    }

    #[test]
    fn link_keeps_text_and_url() {
        let out = strip_ansi_codes(&render_all("see [docs](https://x.io)\n")).to_string();
        assert!(out.contains("docs") && out.contains("https://x.io"), "link text+url kept: {out:?}");
        assert!(!out.contains('[') && !out.contains(']'), "link brackets consumed: {out:?}");
    }
}
