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
use console::{measure_text_width, style, truncate_str};

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
    table_candidate: Option<String>,
    table_lines: Vec<String>,
}

impl MarkdownStream {
    /// `decorate=false` (non-TTY) makes every method a verbatim passthrough.
    pub fn new(decorate: bool, cols: usize) -> Self {
        Self {
            decorate,
            cols: cols.max(24),
            pending: String::new(),
            in_fence: false,
            fence_lang: String::new(),
            table_candidate: None,
            table_lines: Vec::new(),
        }
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
            self.accept_line(&line, &mut out);
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
            self.accept_line(&line, &mut out);
        }
        self.flush_table_state(&mut out);
        if self.in_fence {
            out.push_str(&self.fence_bottom());
            self.in_fence = false;
        }
        out
    }

    fn accept_line(&mut self, line: &str, out: &mut String) {
        if self.in_fence {
            self.render_line(line, out);
            return;
        }
        if !self.table_lines.is_empty() {
            if looks_like_table_row(line) {
                self.table_lines.push(line.to_string());
                return;
            }
            self.flush_table_state(out);
            self.accept_line(line, out);
            return;
        }
        if let Some(header) = self.table_candidate.take() {
            if is_table_separator(line, parse_table_row(&header).map_or(0, |r| r.len())) {
                self.table_lines.push(header);
                self.table_lines.push(line.to_string());
                return;
            }
            self.render_line(&header, out);
            self.accept_line(line, out);
            return;
        }
        if looks_like_table_row(line) {
            self.table_candidate = Some(line.to_string());
        } else {
            self.render_line(line, out);
        }
    }

    fn flush_table_state(&mut self, out: &mut String) {
        if let Some(line) = self.table_candidate.take() {
            self.render_line(&line, out);
        }
        if !self.table_lines.is_empty() {
            let lines = std::mem::take(&mut self.table_lines);
            match parse_table(&lines) {
                Some(table) => out.push_str(&render_table_tty(&table, self.cols)),
                None => {
                    for line in lines {
                        self.render_line(&line, out);
                    }
                }
            }
        }
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
            out.push_str(
                &theme::faint("─".repeat(self.cols.min(48).saturating_sub(2))).to_string(),
            );
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
    fn emit_block(
        &self,
        out: &mut String,
        first_prefix: &str,
        cont_prefix: &str,
        text: &str,
        style_each: &dyn Fn(&str) -> String,
    ) {
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

    /// A code-fence body line: `▌ │ <highlighted> │`, a CLOSED box — content char-wrapped (never
    /// word-wrapped — that would corrupt code) to the inner width, padded, then the right rule, so
    /// every row aligns with the top/bottom borders instead of sprawling past a narrow frame.
    fn emit_code(&self, out: &mut String, line: &str) {
        let left = format!("{}{}", gutter(), style("│ ").color256(theme::CODE_RULE));
        let inner = self.fence_inner();
        let right = format!(" {}", style("│").color256(theme::CODE_RULE));
        let mut row = |visible: &str, styled: String| {
            out.push_str(&left);
            out.push_str(&styled);
            let pad = inner.saturating_sub(measure_text_width(visible));
            out.push_str(&" ".repeat(pad));
            out.push_str(&right);
            out.push('\n');
        };
        if is_visual_fence(&self.fence_lang) {
            let shown = truncate_display(line, inner);
            row(&shown, shown.clone());
            return;
        }
        for chunk in char_chunks(line, inner) {
            row(&chunk, highlight(&chunk, &self.fence_lang));
        }
    }

    /// A continuation prefix that re-draws the gutter bar then pads to `prefix_w` so wrapped rows
    /// line up under the first row's text.
    fn cont_prefix(&self, prefix_w: usize) -> String {
        format!("{}{}", gutter(), " ".repeat(prefix_w.saturating_sub(2)))
    }

    /// Total inner width of the code box (excluding the gutter), shared by the top border, every body
    /// row, and the bottom border so the frame is a true rectangle instead of a narrow top over wide
    /// content. Capped so wide terminals don't stretch code across the whole screen.
    fn fence_width(&self) -> usize {
        self.cols
            .saturating_sub(measure_text_width(&gutter()))
            .min(80)
            .max(16)
    }

    /// The writable span between the `│ ` left rule and the ` │` right rule.
    fn fence_inner(&self) -> usize {
        self.fence_width().saturating_sub(4).max(8)
    }

    /// `▌ ╭─ lang ───────╮`
    fn fence_top(&self) -> String {
        let label = if self.fence_lang.is_empty() {
            "code"
        } else {
            self.fence_lang.as_str()
        };
        let w = self.fence_width();
        let label_w = measure_text_width(label).min(w.saturating_sub(6));
        let label = truncate_display(label, label_w);
        // border = ╭─ {label} {dashes}╮ ; ╭─ =2, spaces=2, ╮=1 → dashes fill the rest to `w`.
        let dashes = w.saturating_sub(measure_text_width(&label) + 5);
        format!(
            "{}{}{}{}\n",
            gutter(),
            theme::accent_dim("╭─ "),
            theme::accent(&label),
            theme::accent_dim(format!(" {}╮", "─".repeat(dashes))),
        )
    }

    /// `▌ ╰──────────────╯`
    fn fence_bottom(&self) -> String {
        let w = self.fence_width();
        format!(
            "{}{}\n",
            gutter(),
            theme::accent_dim(format!("╰{}╯", "─".repeat(w.saturating_sub(2))))
        )
    }
}

#[derive(Clone, Copy)]
enum TableAlign {
    Left,
    Center,
    Right,
}

struct MarkdownTable {
    headers: Vec<String>,
    aligns: Vec<TableAlign>,
    rows: Vec<Vec<String>>,
}

fn is_visual_fence(lang: &str) -> bool {
    matches!(
        lang.trim().to_ascii_lowercase().as_str(),
        "diagram" | "ascii" | "flow"
    )
}

fn truncate_display(s: &str, budget: usize) -> String {
    if measure_text_width(s) <= budget {
        s.to_string()
    } else {
        truncate_str(s, budget, "…").into_owned()
    }
}

fn parse_table_row(line: &str) -> Option<Vec<String>> {
    let mut t = line.trim();
    if t.starts_with('|') {
        t = &t[1..];
    }
    if t.ends_with('|') && !t.ends_with("\\|") {
        t = &t[..t.len() - 1];
    }
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut escaped = false;
    let mut saw_pipe = false;
    for ch in t.chars() {
        if escaped {
            if ch == '|' {
                cur.push('|');
            } else {
                cur.push('\\');
                cur.push(ch);
            }
            escaped = false;
        } else if ch == '\\' {
            escaped = true;
        } else if ch == '|' {
            saw_pipe = true;
            cells.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(ch);
        }
    }
    if escaped {
        cur.push('\\');
    }
    cells.push(cur.trim().to_string());
    (saw_pipe || line.trim().starts_with('|') || line.trim().ends_with('|')).then_some(cells)
}

fn looks_like_table_row(line: &str) -> bool {
    line.trim().contains('|') && parse_table_row(line).is_some_and(|r| r.len() >= 2)
}

fn separator_cell(cell: &str) -> Option<TableAlign> {
    let t = cell.trim();
    let left = t.starts_with(':');
    let right = t.ends_with(':');
    let core = t.trim_matches(':');
    // GFM only requires at least one hyphen per delimiter cell (`-`, `--`, `:-`, `-:`, `:-:`, …).
    // The old 3-dash minimum rejected the short separators models routinely emit (e.g. `|--|`),
    // which failed table detection and dumped the raw pipes to the screen.
    if core.is_empty() || !core.chars().all(|c| c == '-') {
        return None;
    }
    Some(match (left, right) {
        (true, true) => TableAlign::Center,
        (false, true) => TableAlign::Right,
        _ => TableAlign::Left,
    })
}

fn is_table_separator(line: &str, expected: usize) -> bool {
    // A header candidate (>= 2 cells) followed by a row whose every cell is a delimiter (`---`,
    // `:-:`, …) IS a table, even when the separator's column count doesn't match the header's.
    // Models routinely emit a short separator under a wider header (`|---:|---|` beneath a
    // 3-column head); requiring exact equality rejected those and dumped the raw pipes to screen.
    // `parse_table` reconciles the counts (pad Left / truncate) once detection passes.
    expected >= 2
        && parse_table_row(line)
            .is_some_and(|r| r.len() >= 2 && r.iter().all(|c| separator_cell(c).is_some()))
}

fn parse_table(lines: &[String]) -> Option<MarkdownTable> {
    if lines.len() < 2 {
        return None;
    }
    let headers = parse_table_row(&lines[0])?;
    if headers.len() < 2 {
        return None;
    }
    let sep = parse_table_row(&lines[1])?;
    if sep.len() < 2 {
        return None;
    }
    // Every separator cell must be a delimiter, but the count need not equal the header's — a
    // model may emit fewer (or more) delimiter cells than columns. Reconcile to `headers.len()`:
    // extra separators are dropped, missing ones default to Left. Detection already confirmed the
    // shape, so a count mismatch reshapes the alignment vector instead of discarding the table.
    let mut aligns = sep
        .iter()
        .map(|c| separator_cell(c))
        .collect::<Option<Vec<_>>>()?;
    aligns.resize(headers.len(), TableAlign::Left);
    aligns.truncate(headers.len());
    let mut rows = Vec::new();
    for line in &lines[2..] {
        let mut row = parse_table_row(line)?;
        row.resize(headers.len(), String::new());
        row.truncate(headers.len());
        rows.push(row);
    }
    Some(MarkdownTable {
        headers,
        aligns,
        rows,
    })
}

/// Visible display width of a cell **after** `inline()` styling. `inline()` strips markdown markers
/// (`*`, `_`, backticks) and emits ANSI (which `measure_text_width` ignores), so this is the true
/// on-screen column width. Measuring the raw cell over-counts by the marker bytes and drifts borders.
fn cell_display_width(s: &str) -> usize {
    measure_text_width(&inline(s))
}

fn table_natural_width(table: &MarkdownTable) -> usize {
    let mut widths: Vec<usize> = table
        .headers
        .iter()
        .map(|h| cell_display_width(h).max(1))
        .collect();
    for row in &table.rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell_display_width(cell));
        }
    }
    widths.iter().sum::<usize>() + table.headers.len() * 3 + 1
}

fn display_pad(s: &str, width: usize, align: TableAlign) -> String {
    let gap = width.saturating_sub(measure_text_width(s).min(width));
    let (left, right) = match align {
        TableAlign::Left => (0, gap),
        TableAlign::Right => (gap, 0),
        TableAlign::Center => (gap / 2, gap - gap / 2),
    };
    format!("{}{}{}", " ".repeat(left), s, " ".repeat(right))
}

fn border_line(left: char, join: char, right: char, widths: &[usize]) -> String {
    let mut s = String::new();
    s.push(left);
    for (i, width) in widths.iter().enumerate() {
        if i > 0 {
            s.push(join);
        }
        s.push_str(&"─".repeat(width + 2));
    }
    s.push(right);
    s
}

fn render_table_tty(table: &MarkdownTable, cols: usize) -> String {
    let budget = cols.saturating_sub(measure_text_width(&gutter())).max(8);
    if table.headers.len() > 5 || table_natural_width(table) > budget {
        return render_table_stacked_tty(table, cols);
    }
    // Widths are the VISIBLE width post-`inline()` (markers stripped, ANSI ignored) so styled cells
    // line up with the border. Measuring the raw cell over-counts by the marker bytes → borders drift.
    let mut widths: Vec<usize> = table
        .headers
        .iter()
        .map(|h| cell_display_width(h).max(1))
        .collect();
    for row in &table.rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(cell_display_width(cell));
        }
    }
    let mut out = String::new();
    let border = |out: &mut String, line: String| {
        out.push_str(&gutter());
        out.push_str(&theme::accent_dim(line).to_string());
        out.push('\n');
    };
    border(&mut out, border_line('╭', '┬', '╮', &widths));
    out.push_str(&gutter());
    out.push_str(&theme::accent_dim("│").to_string());
    for (i, cell) in table.headers.iter().enumerate() {
        out.push(' ');
        out.push_str(
            &theme::accent(display_pad(&inline(cell), widths[i], TableAlign::Center))
                .bold()
                .to_string(),
        );
        out.push(' ');
        out.push_str(&theme::accent_dim("│").to_string());
    }
    out.push('\n');
    border(&mut out, border_line('├', '┼', '┤', &widths));
    for row in &table.rows {
        out.push_str(&gutter());
        out.push_str(&theme::accent_dim("│").to_string());
        for (i, cell) in row.iter().enumerate() {
            out.push(' ');
            out.push_str(&display_pad(&inline(cell), widths[i], table.aligns[i]));
            out.push(' ');
            out.push_str(&theme::accent_dim("│").to_string());
        }
        out.push('\n');
    }
    border(&mut out, border_line('╰', '┴', '╯', &widths));
    out
}

fn render_table_stacked_tty(table: &MarkdownTable, cols: usize) -> String {
    let prefix = format!("{}  ", gutter());
    let key_width = table
        .headers
        .iter()
        .map(|h| measure_text_width(h))
        .max()
        .unwrap_or(1)
        .min(18);
    let budget = cols
        .saturating_sub(measure_text_width(&prefix) + key_width + 3)
        .max(8);
    let rows = if table.rows.is_empty() {
        vec![vec![String::new(); table.headers.len()]]
    } else {
        table.rows.clone()
    };
    let mut out = String::new();
    for (ri, row) in rows.iter().enumerate() {
        out.push_str(&gutter());
        out.push_str(&theme::accent(format!("◆ {}", ri + 1)).bold().to_string());
        out.push('\n');
        for (i, header) in table.headers.iter().enumerate() {
            let key = truncate_display(header, key_width);
            let value = row.get(i).map(String::as_str).unwrap_or("");
            let wrapped = {
                let v = wrap_plain(value, budget);
                if v.is_empty() {
                    vec![String::new()]
                } else {
                    v
                }
            };
            for (wi, line) in wrapped.iter().enumerate() {
                out.push_str(&prefix);
                if wi == 0 {
                    out.push_str(
                        &theme::muted(display_pad(&key, key_width, TableAlign::Right)).to_string(),
                    );
                    out.push_str(&theme::accent_dim(" : ").to_string());
                } else {
                    out.push_str(&" ".repeat(key_width + 3));
                }
                out.push_str(&inline(line));
                out.push('\n');
            }
        }
    }
    out
}

/// Convert Markdown tables to stacked plain text for chat clients without pipe-table rendering.
pub fn render_plain_blocks(input: &str) -> String {
    let lines: Vec<&str> = input.lines().collect();
    let trailing_nl = input.ends_with('\n');
    let mut out = String::new();
    let mut i = 0usize;
    let mut in_fence = false;
    while i < lines.len() {
        let line = lines[i];
        if line.trim_start().starts_with("```") {
            in_fence = !in_fence;
            out.push_str(line);
            out.push('\n');
            i += 1;
            continue;
        }
        if !in_fence && i + 1 < lines.len() {
            let width = parse_table_row(line).map_or(0, |r| r.len());
            if looks_like_table_row(line) && is_table_separator(lines[i + 1], width) {
                let start = i;
                i += 2;
                while i < lines.len() && looks_like_table_row(lines[i]) {
                    i += 1;
                }
                let block: Vec<String> = lines[start..i].iter().map(|s| (*s).to_string()).collect();
                if let Some(table) = parse_table(&block) {
                    for (ri, row) in table.rows.iter().enumerate() {
                        if table.rows.len() > 1 {
                            out.push_str(&format!("◆ {}\n", ri + 1));
                        }
                        for (ci, header) in table.headers.iter().enumerate() {
                            out.push_str(header);
                            out.push_str(": ");
                            out.push_str(row.get(ci).map(String::as_str).unwrap_or(""));
                            out.push('\n');
                        }
                        if ri + 1 < table.rows.len() {
                            out.push('\n');
                        }
                    }
                    continue;
                }
                i = start;
            }
        }
        out.push_str(line);
        out.push('\n');
        i += 1;
    }
    if !trailing_nl && out.ends_with('\n') {
        out.pop();
    }
    out
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
    (t.len() >= 3)
        && (t.chars().all(|c| c == '-')
            || t.chars().all(|c| c == '*')
            || t.chars().all(|c| c == '_'))
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
    if let Some(rest) = after
        .strip_prefix(". ")
        .or_else(|| after.strip_prefix(") "))
    {
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
        "py" | "python" | "sh" | "bash" | "zsh" | "yaml" | "yml" | "toml" | "ruby" | "rb" | "r" => {
            &["#"]
        }
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
    "fn",
    "let",
    "const",
    "var",
    "function",
    "def",
    "class",
    "struct",
    "enum",
    "impl",
    "trait",
    "pub",
    "return",
    "if",
    "else",
    "elif",
    "for",
    "while",
    "loop",
    "match",
    "case",
    "switch",
    "break",
    "continue",
    "import",
    "from",
    "use",
    "mod",
    "package",
    "async",
    "await",
    "yield",
    "type",
    "interface",
    "public",
    "private",
    "protected",
    "static",
    "final",
    "void",
    "new",
    "self",
    "this",
    "super",
    "true",
    "false",
    "null",
    "none",
    "nil",
    "and",
    "or",
    "not",
    "in",
    "is",
    "as",
    "with",
    "try",
    "catch",
    "except",
    "finally",
    "throw",
    "raise",
    "defer",
    "go",
    "func",
    "where",
    "extends",
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
            while j < chars.len()
                && (chars[j].is_ascii_alphanumeric() || chars[j] == '.' || chars[j] == '_')
            {
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
        assert_eq!(
            s.push(raw),
            raw,
            "non-TTY must be byte-identical passthrough"
        );
        assert_eq!(s.finish(), "");
    }

    #[test]
    fn strips_inline_markers() {
        let out = strip_ansi_codes(&render_all("a **bold** and *it* and `c` word\n")).to_string();
        assert!(!out.contains("**"), "bold markers consumed: {out:?}");
        assert!(out.contains("bold") && out.contains("it") && out.contains('c'));
        // the lone * / ` delimiters are gone
        assert!(
            !out.contains('`'),
            "inline-code backticks consumed: {out:?}"
        );
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
        assert!(
            out.contains("• one") && out.contains("• two"),
            "bullets: {out:?}"
        );
        assert!(out.contains("1. first"), "numbered: {out:?}");
    }

    #[test]
    fn code_fence_gets_box_and_holds_lines() {
        let mut s = MarkdownStream::new(true, 80);
        // opening fence line alone: should NOT yet print the code, but should print a top border
        let top = strip_ansi_codes(&s.push("```rust\n")).to_string();
        assert!(
            top.contains("╭") && top.contains("rust"),
            "fence top with lang: {top:?}"
        );
        let body = strip_ansi_codes(&s.push("let x = 1;\n")).to_string();
        assert!(
            body.contains("let x = 1;"),
            "code body preserved verbatim: {body:?}"
        );
        assert!(body.contains('│'), "code body has the left rule");
        let bottom = strip_ansi_codes(&s.push("```\n")).to_string();
        assert!(bottom.contains("╰"), "fence bottom border: {bottom:?}");
    }

    #[test]
    fn partial_line_held_until_newline() {
        let mut s = MarkdownStream::new(true, 80);
        assert_eq!(
            s.push("no newline yet"),
            "",
            "a line with no \\n is buffered"
        );
        let out = s.push(" done\n");
        assert!(
            strip_ansi_codes(&out).contains("no newline yet done"),
            "completed line flushes whole: {out:?}"
        );
    }

    #[test]
    fn unterminated_fence_closed_on_finish() {
        let mut s = MarkdownStream::new(true, 80);
        s.push("```\ncode without close\n");
        let tail = strip_ansi_codes(&s.finish()).to_string();
        assert!(
            tail.contains("╰"),
            "finish() closes a dangling fence: {tail:?}"
        );
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
        assert!(
            lines.len() > 1,
            "a 150-char line must wrap into several rows"
        );
        for l in &lines {
            assert!(
                measure_text_width(l) <= cols,
                "row wider than the window: {l:?} = {}",
                measure_text_width(l)
            );
            assert!(l.contains('▌'), "every wrapped row keeps the gutter: {l:?}");
        }
    }

    #[test]
    fn over_long_word_hard_splits() {
        // A token with no spaces (e.g. a URL) longer than the width must still be broken up.
        let url = "x".repeat(120);
        let rows = wrap_plain(&url, 30);
        assert!(
            rows.len() >= 4,
            "long unbroken token splits: {} rows",
            rows.len()
        );
        assert!(rows.iter().all(|r| measure_text_width(r) <= 30));
    }

    #[test]
    fn link_keeps_text_and_url() {
        let out = strip_ansi_codes(&render_all("see [docs](https://x.io)\n")).to_string();
        assert!(
            out.contains("docs") && out.contains("https://x.io"),
            "link text+url kept: {out:?}"
        );
        assert!(
            !out.contains('[') && !out.contains(']'),
            "link brackets consumed: {out:?}"
        );
    }

    #[test]
    fn wide_and_narrow_tables_are_responsive() {
        let md = "| File | Status | Count |\n|:---|:---:|---:|\n| src/lib.rs | done | 12 |\n";
        let wide = strip_ansi_codes(&render_all(md)).to_string();
        assert!(
            wide.contains('╭') && wide.contains('┼') && wide.contains("src/lib.rs"),
            "{wide}"
        );
        assert!(wide.lines().all(|l| measure_text_width(l) <= 80));
        let mut s = MarkdownStream::new(true, 28);
        let mut narrow = s.push(md);
        narrow.push_str(&s.finish());
        let narrow = strip_ansi_codes(&narrow).to_string();
        assert!(
            narrow.contains("◆ 1") && narrow.contains("File") && narrow.contains("src/lib.rs"),
            "{narrow}"
        );
        assert!(narrow.lines().all(|l| measure_text_width(l) <= 28));
    }

    #[test]
    fn table_body_markers_do_not_drift_borders() {
        // The screenshot bug: body cells with inline markdown (`*italic*`, `**bold**`, `` `code` ``)
        // were measured RAW (markers counted) but rendered via `inline()` (markers stripped), so each
        // styled cell came out short and the closing │ walked left row by row. Every rendered row —
        // borders, header, body — must share one visible width, and no marker byte may leak.
        let md = "| Effect | Where | Feel |\n|---|---|---|\n\
                  | Pixel Snow | full-page bg | steel |\n\
                  | DecryptedText | hero *remembers you* | cipher |\n\
                  | ClickSpark | `every click` | **thin** |\n";
        let plain = strip_ansi_codes(&render_all(md)).to_string();
        let box_lines: Vec<&str> = plain
            .lines()
            .filter(|l| l.contains('│') || l.contains('╭') || l.contains('┼') || l.contains('╰'))
            .collect();
        assert!(
            box_lines.len() >= 6,
            "top+header+sep+3 body+bottom: {plain}"
        );
        let widths: Vec<usize> = box_lines.iter().map(|l| measure_text_width(l)).collect();
        assert!(
            widths.iter().all(|w| *w == widths[0]),
            "every table row same visible width, got {widths:?}: {plain}"
        );
        // Markers consumed, text kept.
        assert!(
            plain.contains("remembers you")
                && plain.contains("every click")
                && plain.contains("thin"),
            "{plain}"
        );
        assert!(
            !plain.contains('*') && !plain.contains('`'),
            "markdown markers must be stripped, not padded: {plain}"
        );
    }

    #[test]
    fn table_streaming_is_chunk_invariant_and_escaped_pipe_survives() {
        let md = "before\n| Key | Value |\n|---|---|\n| a\\|b | tiếng Việt |\nafter\n";
        let expected = render_all(md);
        for cut in 0..=md.len() {
            if !md.is_char_boundary(cut) {
                continue;
            }
            let mut s = MarkdownStream::new(true, 80);
            let mut got = s.push(&md[..cut]);
            got.push_str(&s.push(&md[cut..]));
            got.push_str(&s.finish());
            assert_eq!(
                strip_ansi_codes(&got),
                strip_ansi_codes(&expected),
                "cut={cut}"
            );
        }
        assert!(strip_ansi_codes(&expected).contains("a|b"));
    }

    #[test]
    fn malformed_table_and_code_fence_fail_open() {
        let malformed =
            strip_ansi_codes(&render_all("| a | b |\n| not | separator |\nplain\n")).to_string();
        assert!(malformed.contains("| a | b |") && malformed.contains("| not | separator |"));
        let fenced =
            strip_ansi_codes(&render_all("```txt\n| a | b |\n|---|---|\n```\n")).to_string();
        assert!(
            fenced.contains("|---|---|") && !fenced.contains('┼'),
            "{fenced}"
        );
    }

    #[test]
    fn diagram_preserves_topology_and_truncates_instead_of_wrapping() {
        let mut s = MarkdownStream::new(true, 32);
        let mut out =
            s.push("```diagram\n[A]    -->    [B]\n012345678901234567890123456789012345\n```\n");
        out.push_str(&s.finish());
        let plain = strip_ansi_codes(&out).to_string();
        assert!(
            plain.contains("[A]    -->    [B]") && plain.contains('…'),
            "{plain}"
        );
        assert_eq!(
            plain.lines().filter(|l| l.contains("012345")).count(),
            1,
            "{plain}"
        );
        assert!(plain.lines().all(|l| measure_text_width(l) <= 32));
    }

    #[test]
    fn short_separator_still_detects_table() {
        // The bug from the screenshot: a `|--|` / `|---|` header separator with 1-2 dashes per cell
        // used to fail the 3-dash minimum, so the whole table dumped its raw pipes to the screen.
        // GFM only needs one hyphen per delimiter cell.
        for sep in ["|--|--|", "|-|-|", "| - | - |"] {
            let md = format!("| A | B |\n{sep}\n| x | y |\n");
            let out = strip_ansi_codes(&render_all(&md)).to_string();
            assert!(
                out.contains('┼') && out.contains('╭'),
                "short sep {sep:?} must render a table box: {out}"
            );
            assert!(
                !out.contains("|--"),
                "raw pipes must not leak for {sep:?}: {out}"
            );
        }
    }

    #[test]
    fn separator_column_count_need_not_match_header() {
        // The screenshot bug: a 3-column header (`| Loại | Số lượng | Ghi chú |`) with a 2-column
        // separator (`|---:|---|`) failed the exact-count check and dumped raw pipes. A delimiter
        // row that is short (or long) must still be detected; `parse_table` reconciles the columns.
        let short = "| Loại | Số lượng | Ghi chú |\n|---:|---|\n| a | 1 | note |\n";
        let out = strip_ansi_codes(&render_all(short)).to_string();
        assert!(
            out.contains('┼') && out.contains('╭'),
            "mismatched separator must still render a box: {out}"
        );
        assert!(!out.contains("|---"), "raw pipes must not leak: {out}");
        assert!(
            out.contains("Ghi chú") && out.contains("note"),
            "all three columns must survive: {out}"
        );
        // A separator wider than the header reconciles the other way (extra delimiter dropped).
        let long = "| A | B |\n|---|---|---|\n| x | y |\n";
        let out = strip_ansi_codes(&render_all(long)).to_string();
        assert!(out.contains('┼') && !out.contains("|---"), "{out}");
    }

    #[test]
    fn code_box_is_a_true_rectangle() {
        // The other screenshot bug: on a wide terminal the frame was capped narrow while the body ran
        // to the full width, so a diagram sprawled past its box. Every line of the box (top, body,
        // bottom) must now share one width and carry the right rule.
        let cols = 100;
        let mut s = MarkdownStream::new(true, cols);
        let mut out =
            s.push("```diagram\nA --------------------------------------------------> B\n```\n");
        out.push_str(&s.finish());
        let plain = strip_ansi_codes(&out).to_string();
        let box_lines: Vec<&str> = plain
            .lines()
            .filter(|l| l.contains('╭') || l.contains('╰') || l.contains('│'))
            .collect();
        assert!(box_lines.len() >= 3, "top + body + bottom: {plain}");
        let widths: Vec<usize> = box_lines.iter().map(|l| measure_text_width(l)).collect();
        assert!(
            widths.iter().all(|w| *w == widths[0]),
            "every box row same width, got {widths:?}: {plain}"
        );
        assert!(
            plain
                .lines()
                .filter(|l| l.contains('│'))
                .all(|l| l.trim_end().ends_with('│')),
            "body rows closed by a right rule: {plain}"
        );
    }

    #[test]
    fn plain_block_renderer_stacks_tables_and_preserves_diagrams() {
        let md = "Intro\n| A | B |\n|---|---|\n| x | y |\n\n```diagram\nA --> B\n```";
        let out = render_plain_blocks(md);
        assert!(
            out.contains("A: x\nB: y") && !out.contains("|---|---|"),
            "{out}"
        );
        assert!(out.contains("```diagram\nA --> B\n```"), "{out}");
    }
}
