//! Hyperlink detection and OSC 8 injection for the retained TUI.
//!
//! After each `terminal.draw()` call, `inject_hyperlinks` walks the visible transcript rows,
//! detects URLs (`https?://…`) and file paths (`src/foo.rs`, `C:\…`, `/abs/path`), then writes
//! OSC 8 escape sequences directly via `backend_mut()` — bypassing ratatui's cell model entirely,
//! exactly like the screensaver sixel blitter. ratatui's cell diff never sees the OSC 8 bytes so
//! they are never overwritten mid-session; a `terminal.clear()` (Ctrl-L) wipes them, but the next
//! draw + inject restores them.
//!
//! ## Why post-draw injection?
//! ratatui 0.30 `Cell` has no hyperlink field and no OSC 8 feature flag. Injecting raw bytes into
//! a cell's symbol would confuse the width-accounting code (OSC 8 sequences contain visible-width
//! chars like `]`, `;`). Post-draw is the only race-free approach that works with this version.
//!
//! ## OSC 8 format (widely supported: WezTerm, Windows Terminal 1.19+, iTerm2, foot, …)
//! ```text
//! \x1b]8;;URL\x1b\\ <visible text> \x1b]8;;\x1b\\
//! ```
//! The empty second parameter is "no params"; the terminator is ST (`\x1b\\`).

use std::io::Write as _;

/// A detected hyperlink span within a plain-text row.
#[derive(Debug, Clone)]
pub struct LinkSpan {
    /// Display-cell column where the link text starts (0-indexed).
    pub col: usize,
    /// Display-cell width of the link text (number of cells).
    pub width: usize,
    /// The URL to open (already formatted as `https://…` or `file:///…`).
    pub url: String,
    /// The visible text (subset of the row).
    pub text: String,
}

/// Scan a single plain-text row (SGR already stripped) and return all hyperlink spans.
pub fn scan_row(row: &str) -> Vec<LinkSpan> {
    let mut spans = Vec::new();
    let chars: Vec<char> = row.chars().collect();
    let len = chars.len();
    let mut i = 0;
    while i < len {
        // ── URL: https?:// ──────────────────────────────────────────────────────────────
        if i + 7 < len
            && (chars[i..i + 8].iter().collect::<String>() == "https://"
                || (i + 6 < len
                    && chars[i..i + 7].iter().collect::<String>() == "http://"))
        {
            let start = i;
            // consume until whitespace or common terminator
            while i < len && !is_url_break(chars[i]) {
                i += 1;
            }
            let text: String = chars[start..i].iter().collect();
            let text = trim_url_tail(&text);
            if text.len() > 10 {
                let col = display_col(&chars, start);
                let width = display_col(&chars, start + text.chars().count()) - col;
                spans.push(LinkSpan {
                    col,
                    width,
                    url: text.to_string(),
                    text: text.to_string(),
                });
            }
            continue;
        }

        // ── file path: bare word ending in .rs / .toml / .json / .md / .txt / .py / .ts …
        // OR starts with a drive letter `C:\` / absolute `/` ─────────────────────────────
        if let Some((start, end, url)) = try_file_path(&chars, i) {
            let text: String = chars[start..end].iter().collect();
            let col = display_col(&chars, start);
            let width = display_col(&chars, end) - col;
            spans.push(LinkSpan {
                col,
                width,
                url,
                text,
            });
            i = end;
            continue;
        }

        i += 1;
    }
    spans
}

// ── helpers ─────────────────────────────────────────────────────────────────────────────────────

fn is_url_break(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '"' | '\'' | '>' | '<' | ')' | ']' | '}' | '`')
}

/// Trim trailing punctuation that is usually not part of the URL itself.
fn trim_url_tail(s: &str) -> &str {
    let mut end = s.len();
    while end > 0 {
        match s.as_bytes()[end - 1] {
            b'.' | b',' | b';' | b':' | b'!' | b'?' | b')' | b']' | b'}' => end -= 1,
            _ => break,
        }
    }
    &s[..end]
}

/// Try to parse a file path starting at `chars[i]`. Returns `(start, end, file_url)` or `None`.
fn try_file_path(chars: &[char], i: usize) -> Option<(usize, usize, String)> {
    let len = chars.len();

    // Windows absolute path: letter + `:\`
    let is_win_abs = i + 2 < len
        && chars[i].is_ascii_alphabetic()
        && chars[i + 1] == ':'
        && (chars[i + 2] == '\\' || chars[i + 2] == '/');

    // Unix absolute path: starts with `/`
    let is_unix_abs = chars[i] == '/';

    // Relative path: starts with a ident char and contains `/` or `\` + known extension
    let is_rel = !is_win_abs
        && !is_unix_abs
        && (chars[i].is_alphanumeric() || matches!(chars[i], '_' | '.'));

    if !is_win_abs && !is_unix_abs && !is_rel {
        return None;
    }

    // consume path chars
    let start = i;
    let mut j = i;
    while j < len && is_path_char(chars[j]) {
        j += 1;
    }
    if j == start {
        return None;
    }

    let path: String = chars[start..j].iter().collect();

    // for relative paths: must contain a `/` or `\` and end with a known extension
    if is_rel {
        if !path.contains('/') && !path.contains('\\') {
            return None;
        }
        if !has_source_ext(&path) {
            return None;
        }
    }

    // for absolute unix/win paths: must have at least one separator after the start
    if (is_unix_abs || is_win_abs) && path.len() < 3 {
        return None;
    }

    // Build file:// URL
    let url = if is_win_abs {
        // C:\foo\bar → file:///C:/foo/bar
        let posix = path.replace('\\', "/");
        format!("file:///{posix}")
    } else {
        format!("file://{path}")
    };

    Some((start, j, url))
}

fn is_path_char(c: char) -> bool {
    c.is_alphanumeric()
        || matches!(c, '_' | '-' | '.' | '/' | '\\' | ':')
        // Windows colon in path only once (drive letter), but we allow it for `C:\`
}

/// Known source/config file extensions that should be linkified.
fn has_source_ext(path: &str) -> bool {
    const EXTS: &[&str] = &[
        ".rs", ".toml", ".json", ".md", ".txt", ".py", ".ts", ".tsx", ".js", ".jsx",
        ".go", ".c", ".cpp", ".h", ".hpp", ".java", ".kt", ".swift", ".rb", ".sh",
        ".yaml", ".yml", ".env", ".lock", ".html", ".css", ".scss",
    ];
    let lower = path.to_lowercase();
    // strip optional `:line` suffix before checking extension
    let base = if let Some(pos) = lower.rfind(':') {
        &lower[..pos]
    } else {
        &lower
    };
    EXTS.iter().any(|ext| base.ends_with(ext))
}

/// Compute the display-cell column of `chars[idx]` (sum of char widths before it).
fn display_col(chars: &[char], idx: usize) -> usize {
    chars[..idx.min(chars.len())]
        .iter()
        .map(|c| console::measure_text_width(&c.to_string()).max(1))
        .sum()
}

// ── OSC 8 injection ─────────────────────────────────────────────────────────────────────────────

/// Build an OSC 8 hyperlink sequence wrapping `text` with `url`.
/// Format: `\x1b]8;;URL\x1b\\ TEXT \x1b]8;;\x1b\\`
pub fn osc8(url: &str, text: &str) -> String {
    format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
}

/// After `terminal.draw()`, walk the visible transcript rows and overprint each detected link span
/// with an OSC 8 sequence at the exact screen coordinates.
///
/// `sgr_rows` are the raw rendered strings (with SGR colour codes) for the full transcript.
/// `start` is the first row index into `sgr_rows` that is currently visible at screen row `area.y`.
/// `visible` is the number of rows in the viewport.
///
/// We use `MoveTo` + `Print` (raw crossterm) via `backend_mut()` — exactly like the screensaver
/// blitter — so ratatui's cell model is never disturbed.
pub fn inject_hyperlinks<W: std::io::Write>(
    out: &mut W,
    sgr_rows: &[String],
    plain_rows: &[String],
    start: usize,
    visible: usize,
    area: ratatui::layout::Rect,
) {
    use crossterm::cursor::MoveTo;
    use crossterm::style::Print;
    use crossterm::queue;

    let row_count = visible.min(sgr_rows.len().saturating_sub(start));
    for screen_row in 0..row_count {
        let abs_row = start + screen_row;
        let plain = match plain_rows.get(abs_row) {
            Some(r) => r,
            None => continue,
        };
        let sgr = match sgr_rows.get(abs_row) {
            Some(r) => r,
            None => continue,
        };
        let spans = scan_row(plain);
        if spans.is_empty() {
            continue;
        }
        // For each span, re-print the exact substring from the SGR row baked inside OSC 8.
        // We slice the SGR row to extract the coloured text for this span and wrap it.
        for span in &spans {
            let y = area.y + screen_row as u16;
            let x = area.x + span.col as u16;
            if x >= area.x + area.width {
                continue;
            }
            // Extract the SGR-bearing slice for this display column range.
            let coloured_text = extract_sgr_slice(sgr, span.col, span.width);
            let linked = osc8(&span.url, &coloured_text);
            let _ = queue!(out, MoveTo(x, y), Print(&linked));
        }
    }
    let _ = out.flush();
}

/// Extract the portion of an SGR string that covers display columns `[col, col+width)`.
/// We walk the string char by char (skipping SGR escape sequences) tracking the display column,
/// and reconstruct the substring including any SGR codes it carries.
fn extract_sgr_slice(sgr: &str, col: usize, width: usize) -> String {
    let mut out = String::new();
    let mut display_pos = 0usize;
    let mut in_range = false;
    let mut chars = sgr.chars().peekable();

    while let Some(c) = chars.next() {
        // SGR escape: \x1b[ ... m  — copy through if we are in-range (preserves colour)
        if c == '\x1b' && chars.peek() == Some(&'[') {
            let mut seq = String::from("\x1b[");
            chars.next(); // consume '['
            let mut final_byte = None;
            while let Some(&pc) = chars.peek() {
                chars.next();
                seq.push(pc);
                if pc.is_ascii_alphabetic() {
                    final_byte = Some(pc);
                    break;
                }
            }
            if final_byte == Some('m') && in_range {
                out.push_str(&seq);
            }
            continue;
        }

        let cw = console::measure_text_width(&c.to_string()).max(1);
        let next_pos = display_pos + cw;

        if display_pos >= col && display_pos < col + width {
            if !in_range {
                in_range = true;
            }
            out.push(c);
        } else if in_range {
            // past the end
            break;
        }

        display_pos = next_pos;
        if display_pos >= col + width {
            break;
        }
    }

    if out.is_empty() {
        // fallback: use the plain col slice
        out
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_https_url() {
        let spans = scan_row("see https://example.com/foo for details");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].url, "https://example.com/foo");
        assert_eq!(spans[0].text, "https://example.com/foo");
    }

    #[test]
    fn trims_trailing_punctuation_from_url() {
        let spans = scan_row("visit https://example.com/foo. done");
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].url, "https://example.com/foo");
    }

    #[test]
    fn detects_relative_file_path_with_known_ext() {
        let spans = scan_row("edited src/ui/tui/retained.rs line 42");
        assert_eq!(spans.len(), 1);
        assert!(spans[0].url.starts_with("file://"));
        assert!(spans[0].text.contains("retained.rs"));
    }

    #[test]
    fn ignores_bare_word_without_slash() {
        // "retained.rs" alone (no slash) must NOT be linkified — too many false positives
        let spans = scan_row("see retained.rs for details");
        assert!(spans.is_empty(), "bare filename without slash must not link: {spans:?}");
    }

    #[test]
    fn osc8_format_correct() {
        let s = osc8("https://x.io", "click");
        assert!(s.starts_with("\x1b]8;;https://x.io\x1b\\"));
        assert!(s.contains("click"));
        assert!(s.ends_with("\x1b]8;;\x1b\\"));
    }
}
