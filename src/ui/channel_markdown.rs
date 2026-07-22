//! Telegram-native Markdown rendering for the hostbot surface.
//!
//! Telegram HTML is deliberately used instead of MarkdownV2: the supported tag set is small, links
//! and code are predictable, and escaping is auditable. Rendering is block-based so every chunk is a
//! complete, balanced HTML document fragment — no chunk can cut through an entity, tag, or `<pre>`.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

/// One independently-sendable Telegram message, with the plain fallback used when Telegram rejects
/// the HTML entity graph. `html` is always balanced and `plain` carries the same visible content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TelegramChunk {
    pub html: String,
    pub plain: String,
}

#[derive(Clone, Debug, Default)]
struct Block {
    html: String,
    plain: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InlineTag {
    Bold,
    Italic,
    Strike,
    Link,
}

#[derive(Clone, Debug)]
struct OpenInline {
    kind: InlineTag,
    open: String,
    close: &'static str,
}

#[derive(Clone, Debug)]
struct ListState {
    next: Option<u64>,
}

#[derive(Default)]
struct Renderer {
    blocks: Vec<Block>,
    html: String,
    plain: String,
    inline: Vec<OpenInline>,
    lists: Vec<ListState>,
    quote_depth: usize,
    item_prefix: Option<String>,
    in_code: bool,
    code_lang: String,
    code: String,
    in_table: bool,
    table_in_head: bool,
    table_headers: Vec<String>,
    table_row: Vec<String>,
    table_cell: String,
    table_rows: Vec<Vec<String>>,
    table_row_index: usize,
    image_alt: Option<String>,
}

impl Renderer {
    fn push_visible(&mut self, text: &str) {
        self.html.push_str(&escape_text(text));
        self.plain.push_str(text);
    }

    fn start_inline(&mut self, kind: InlineTag, open: String, close: &'static str) {
        self.html.push_str(&open);
        self.inline.push(OpenInline { kind, open, close });
    }

    fn end_inline(&mut self, kind: InlineTag) {
        if let Some(pos) = self.inline.iter().rposition(|t| t.kind == kind) {
            let trailing = self.inline.split_off(pos + 1);
            let current = self.inline.pop().expect("matching inline tag");
            for tag in trailing.iter().rev() {
                self.html.push_str(tag.close);
            }
            self.html.push_str(current.close);
            for tag in &trailing {
                self.html.push_str(&tag.open);
            }
            self.inline.extend(trailing);
        }
    }

    fn finish_block(&mut self) {
        if self.html.trim().is_empty() && self.plain.trim().is_empty() {
            self.html.clear();
            self.plain.clear();
            return;
        }
        for tag in self.inline.iter().rev() {
            self.html.push_str(tag.close);
        }
        let html = self.html.trim_matches('\n').trim_end().to_string();
        let plain = self.plain.trim_matches('\n').trim_end().to_string();
        self.blocks.push(Block { html, plain });
        self.html.clear();
        self.plain.clear();
        for tag in &self.inline {
            self.html.push_str(&tag.open);
        }
    }

    fn list_prefix(&mut self) -> String {
        let indent = "  ".repeat(self.lists.len().saturating_sub(1));
        let marker = match self.lists.last_mut() {
            Some(ListState { next: Some(next) }) => {
                let shown = *next;
                *next = next.saturating_add(1);
                format!("{shown}.")
            }
            _ => "•".to_string(),
        };
        format!("{indent}{marker} ")
    }

    fn begin_text_block(&mut self) {
        if let Some(prefix) = self.item_prefix.take() {
            self.push_visible(&prefix);
        }
        if self.quote_depth > 0 && self.plain.is_empty() {
            let prefix = "▏ ".repeat(self.quote_depth);
            self.push_visible(&prefix);
        }
    }

    fn finish_table(&mut self) {
        if !self.table_cell.is_empty() {
            self.table_row.push(std::mem::take(&mut self.table_cell));
        }
        if !self.table_row.is_empty() {
            self.table_rows.push(std::mem::take(&mut self.table_row));
        }
        let headers = std::mem::take(&mut self.table_headers);
        let body = std::mem::take(&mut self.table_rows);
        self.in_table = false;
        self.table_in_head = false;
        if headers.is_empty() {
            return;
        }
        if body.is_empty() {
            self.push_visible(&headers.join(" · "));
            self.finish_block();
            return;
        }
        for (ri, row) in body.iter().enumerate() {
            let mut html = String::new();
            let mut plain = String::new();
            if body.len() > 1 {
                let n = format!("{}", ri + 1);
                html.push_str("<b>");
                html.push_str(&n);
                html.push_str("</b>\n");
                plain.push_str(&n);
                plain.push('\n');
            }
            for (ci, header) in headers.iter().enumerate() {
                let value = row.get(ci).map(String::as_str).unwrap_or("");
                html.push_str("<b>");
                html.push_str(&escape_text(header));
                html.push_str(":</b> ");
                html.push_str(&render_inline_fragment(value));
                html.push('\n');
                plain.push_str(header);
                plain.push_str(": ");
                plain.push_str(&plain_inline_fragment(value));
                plain.push('\n');
            }
            self.blocks.push(Block {
                html: html.trim_end().to_string(),
                plain: plain.trim_end().to_string(),
            });
        }
    }

    fn finish_code(&mut self) {
        let code = std::mem::take(&mut self.code);
        let lang = std::mem::take(&mut self.code_lang);
        self.in_code = false;
        let class = safe_language(&lang)
            .map(|lang| format!(" class=\"language-{}\"", escape_attr(&lang)))
            .unwrap_or_default();
        self.blocks.push(Block {
            html: format!(
                "<pre><code{class}>{}</code></pre>",
                escape_text(code.trim_end_matches('\n'))
            ),
            plain: format!("```{}\n{}\n```", lang.trim(), code.trim_end_matches('\n')),
        });
    }
}

/// Parse Markdown once, render only Telegram-supported HTML, then pack complete blocks under `max`
/// UTF-16 units. Oversized prose and code are split into independently balanced chunks.
pub fn render_telegram_chunks(input: &str, max: usize) -> Vec<TelegramChunk> {
    let max = max.max(64);
    let mut r = Renderer::default();
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_FOOTNOTES;

    for event in Parser::new_ext(input, options) {
        if r.in_code {
            match event {
                Event::End(TagEnd::CodeBlock) => r.finish_code(),
                Event::Text(t) | Event::Code(t) | Event::Html(t) | Event::InlineHtml(t) => {
                    r.code.push_str(&t)
                }
                Event::SoftBreak | Event::HardBreak => r.code.push('\n'),
                _ => {}
            }
            continue;
        }
        if r.in_table {
            match event {
                Event::Start(Tag::TableHead) => {
                    r.table_in_head = true;
                    r.table_row.clear();
                }
                Event::End(TagEnd::TableHead) => {
                    if !r.table_cell.is_empty() {
                        r.table_row.push(std::mem::take(&mut r.table_cell));
                    }
                    if !r.table_row.is_empty() && r.table_headers.is_empty() {
                        r.table_headers = std::mem::take(&mut r.table_row);
                        r.table_row_index = 1;
                    }
                    r.table_in_head = false;
                }
                Event::Start(Tag::TableRow) => r.table_row.clear(),
                Event::End(TagEnd::TableRow) => {
                    if !r.table_cell.is_empty() {
                        r.table_row.push(std::mem::take(&mut r.table_cell));
                    }
                    if r.table_in_head || r.table_headers.is_empty() {
                        r.table_headers = std::mem::take(&mut r.table_row);
                    } else {
                        r.table_rows.push(std::mem::take(&mut r.table_row));
                    }
                    r.table_row_index += 1;
                }
                Event::Start(Tag::TableCell) => r.table_cell.clear(),
                Event::End(TagEnd::TableCell) => {
                    r.table_row.push(std::mem::take(&mut r.table_cell))
                }
                Event::Text(t) | Event::Code(t) => r.table_cell.push_str(&t),
                Event::SoftBreak | Event::HardBreak => r.table_cell.push(' '),
                Event::End(TagEnd::Table) => r.finish_table(),
                _ => {}
            }
            continue;
        }

        match event {
            Event::Start(Tag::Paragraph) => r.begin_text_block(),
            Event::End(TagEnd::Paragraph) => r.finish_block(),
            Event::Start(Tag::Heading { .. }) => {
                r.finish_block();
                r.html.push_str("<b>");
                r.inline.push(OpenInline {
                    kind: InlineTag::Bold,
                    open: "<b>".to_string(),
                    close: "</b>",
                });
            }
            Event::End(TagEnd::Heading(_)) => {
                r.end_inline(InlineTag::Bold);
                r.finish_block();
            }
            Event::Start(Tag::BlockQuote(_)) => r.quote_depth += 1,
            Event::End(TagEnd::BlockQuote(_)) => {
                r.finish_block();
                r.quote_depth = r.quote_depth.saturating_sub(1);
            }
            Event::Start(Tag::List(start)) => r.lists.push(ListState { next: start }),
            Event::End(TagEnd::List(_)) => {
                r.finish_block();
                r.lists.pop();
            }
            Event::Start(Tag::Item) => {
                r.finish_block();
                r.item_prefix = Some(r.list_prefix());
            }
            Event::End(TagEnd::Item) => r.finish_block(),
            Event::Start(Tag::CodeBlock(kind)) => {
                r.finish_block();
                r.in_code = true;
                r.code_lang = match kind {
                    CodeBlockKind::Fenced(lang) => lang.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
            }
            Event::Start(Tag::Strong) => r.start_inline(InlineTag::Bold, "<b>".to_string(), "</b>"),
            Event::End(TagEnd::Strong) => r.end_inline(InlineTag::Bold),
            Event::Start(Tag::Emphasis) => {
                r.start_inline(InlineTag::Italic, "<i>".to_string(), "</i>")
            }
            Event::End(TagEnd::Emphasis) => r.end_inline(InlineTag::Italic),
            Event::Start(Tag::Strikethrough) => {
                r.start_inline(InlineTag::Strike, "<s>".to_string(), "</s>")
            }
            Event::End(TagEnd::Strikethrough) => r.end_inline(InlineTag::Strike),
            Event::Start(Tag::Link { dest_url, .. }) => {
                let url = dest_url.to_string();
                if safe_link(&url) {
                    r.start_inline(
                        InlineTag::Link,
                        format!("<a href=\"{}\">", escape_attr(&url)),
                        "</a>",
                    );
                }
            }
            Event::End(TagEnd::Link) => r.end_inline(InlineTag::Link),
            Event::Start(Tag::Image { .. }) => r.image_alt = Some(String::new()),
            Event::End(TagEnd::Image) => {
                if let Some(alt) = r.image_alt.take() {
                    r.push_visible(&format!("[image: {alt}]"));
                }
            }
            Event::Start(Tag::Table(_)) => {
                r.finish_block();
                r.in_table = true;
                r.table_headers.clear();
                r.table_rows.clear();
                r.table_row_index = 0;
            }
            Event::Text(t) => {
                if let Some(alt) = r.image_alt.as_mut() {
                    alt.push_str(&t);
                } else {
                    r.push_visible(&t);
                }
            }
            Event::Code(t) => {
                r.html.push_str("<code>");
                r.html.push_str(&escape_text(&t));
                r.html.push_str("</code>");
                r.plain.push('`');
                r.plain.push_str(&t);
                r.plain.push('`');
            }
            Event::Html(t) | Event::InlineHtml(t) => r.push_visible(&t),
            Event::SoftBreak => r.push_visible(" "),
            Event::HardBreak => r.push_visible("\n"),
            Event::Rule => {
                r.finish_block();
                r.blocks.push(Block {
                    html: "────────".to_string(),
                    plain: "────────".to_string(),
                });
            }
            Event::TaskListMarker(done) => r.push_visible(if done { "☑ " } else { "☐ " }),
            Event::FootnoteReference(name) => r.push_visible(&format!("[{}]", name)),
            Event::Start(_) | Event::End(_) | Event::InlineMath(_) | Event::DisplayMath(_) => {}
        }
    }
    if r.in_code {
        r.finish_code();
    }
    if r.in_table {
        r.finish_table();
    }
    r.finish_block();

    let mut normalized = Vec::new();
    for block in r.blocks {
        normalized.extend(split_block(block, max));
    }
    pack_blocks(normalized, max)
}

fn pack_blocks(blocks: Vec<Block>, max: usize) -> Vec<TelegramChunk> {
    let mut out = Vec::new();
    let mut html = String::new();
    let mut plain = String::new();
    for block in blocks {
        let join_html = if html.is_empty() { 0 } else { 2 };
        let next_units = utf16_len(&html) + join_html + utf16_len(&block.html);
        if next_units > max && !html.is_empty() {
            out.push(TelegramChunk {
                html: std::mem::take(&mut html),
                plain: std::mem::take(&mut plain),
            });
        }
        if !html.is_empty() {
            html.push_str("\n\n");
        }
        if !plain.is_empty() {
            plain.push_str("\n\n");
        }
        html.push_str(&block.html);
        plain.push_str(&block.plain);
        debug_assert!(utf16_len(&html) <= max || html.is_empty());
    }
    if !html.is_empty() || !plain.is_empty() {
        out.push(TelegramChunk { html, plain });
    }
    out
}

fn split_block(block: Block, max: usize) -> Vec<Block> {
    if utf16_len(&block.html) <= max {
        return vec![block];
    }
    if block.html.starts_with("<pre><code") && block.html.ends_with("</code></pre>") {
        return split_code_block(&block, max);
    }
    split_plain_block(&block.plain, max)
}

fn split_code_block(block: &Block, max: usize) -> Vec<Block> {
    let lang = block
        .html
        .strip_prefix("<pre><code class=\"language-")
        .and_then(|s| s.split_once("\">"))
        .map(|(lang, _)| lang.to_string());
    let prefix = lang
        .as_deref()
        .map(|l| format!("<pre><code class=\"language-{l}\">"))
        .unwrap_or_else(|| "<pre><code>".to_string());
    let suffix = "</code></pre>";
    let code = block
        .plain
        .strip_prefix("```")
        .and_then(|s| s.split_once('\n'))
        .map(|(_, rest)| rest.strip_suffix("\n```").unwrap_or(rest))
        .unwrap_or(&block.plain);
    let budget = max
        .saturating_sub(utf16_len(&prefix) + utf16_len(suffix))
        .max(16);
    split_text(code, budget)
        .into_iter()
        .map(|piece| Block {
            html: format!("{prefix}{}</code></pre>", escape_text(&piece)),
            plain: format!("```{}\n{}\n```", lang.as_deref().unwrap_or(""), piece),
        })
        .collect()
}

fn split_plain_block(plain: &str, max: usize) -> Vec<Block> {
    split_text(plain, max.saturating_sub(16).max(16))
        .into_iter()
        .map(|piece| Block {
            html: render_inline_fragment(&piece),
            plain: piece,
        })
        .collect()
}

fn split_text(text: &str, budget: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut units = 0usize;
    for segment in text.split_inclusive('\n') {
        let seg_units = utf16_len(segment);
        if seg_units <= budget {
            if units + seg_units > budget && !current.is_empty() {
                out.push(std::mem::take(&mut current));
                units = 0;
            }
            current.push_str(segment);
            units += seg_units;
            continue;
        }
        if !current.is_empty() {
            out.push(std::mem::take(&mut current));
            units = 0;
        }
        for word in segment.split_inclusive(char::is_whitespace) {
            let word_units = utf16_len(word);
            if word_units > budget {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
                let mut piece = String::new();
                let mut piece_units = 0usize;
                for ch in word.chars() {
                    let u = ch.len_utf16();
                    if piece_units + u > budget && !piece.is_empty() {
                        out.push(std::mem::take(&mut piece));
                        piece_units = 0;
                    }
                    piece.push(ch);
                    piece_units += u;
                }
                current = piece;
                units = piece_units;
            } else {
                if units + word_units > budget && !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                    units = 0;
                }
                current.push_str(word);
                units += word_units;
            }
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out.into_iter().map(|s| s.trim_end().to_string()).collect()
}

fn render_inline_fragment(input: &str) -> String {
    let mut out = String::new();
    let options = Options::ENABLE_STRIKETHROUGH;
    let mut stack: Vec<(InlineTag, &'static str)> = Vec::new();
    for event in Parser::new_ext(input, options) {
        match event {
            Event::Start(Tag::Strong) => {
                out.push_str("<b>");
                stack.push((InlineTag::Bold, "</b>"));
            }
            Event::End(TagEnd::Strong) => close_fragment_tag(&mut out, &mut stack, InlineTag::Bold),
            Event::Start(Tag::Emphasis) => {
                out.push_str("<i>");
                stack.push((InlineTag::Italic, "</i>"));
            }
            Event::End(TagEnd::Emphasis) => {
                close_fragment_tag(&mut out, &mut stack, InlineTag::Italic)
            }
            Event::Start(Tag::Strikethrough) => {
                out.push_str("<s>");
                stack.push((InlineTag::Strike, "</s>"));
            }
            Event::End(TagEnd::Strikethrough) => {
                close_fragment_tag(&mut out, &mut stack, InlineTag::Strike)
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                let url = dest_url.to_string();
                if safe_link(&url) {
                    out.push_str(&format!("<a href=\"{}\">", escape_attr(&url)));
                    stack.push((InlineTag::Link, "</a>"));
                }
            }
            Event::End(TagEnd::Link) => close_fragment_tag(&mut out, &mut stack, InlineTag::Link),
            Event::Code(t) => out.push_str(&format!("<code>{}</code>", escape_text(&t))),
            Event::Text(t) | Event::Html(t) | Event::InlineHtml(t) => {
                out.push_str(&escape_text(&t))
            }
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            _ => {}
        }
    }
    for (_, close) in stack.iter().rev() {
        out.push_str(close);
    }
    out
}

fn close_fragment_tag(
    out: &mut String,
    stack: &mut Vec<(InlineTag, &'static str)>,
    kind: InlineTag,
) {
    if let Some(pos) = stack.iter().rposition(|(k, _)| *k == kind) {
        let trailing = stack.split_off(pos + 1);
        let (_, close) = stack.pop().expect("matching fragment tag");
        for (_, close) in trailing.iter().rev() {
            out.push_str(close);
        }
        out.push_str(close);
        stack.extend(trailing);
    }
}

fn plain_inline_fragment(input: &str) -> String {
    let mut out = String::new();
    for event in Parser::new_ext(input, Options::ENABLE_STRIKETHROUGH) {
        match event {
            Event::Text(t) | Event::Code(t) | Event::Html(t) | Event::InlineHtml(t) => {
                out.push_str(&t)
            }
            Event::SoftBreak | Event::HardBreak => out.push(' '),
            _ => {}
        }
    }
    out
}

fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(ch),
        }
    }
    out
}

fn escape_attr(s: &str) -> String {
    escape_text(s).replace('"', "&quot;")
}

fn safe_link(url: &str) -> bool {
    url::Url::parse(url).is_ok_and(|u| matches!(u.scheme(), "http" | "https"))
}

fn safe_language(lang: &str) -> Option<String> {
    let lang = lang.trim();
    (!lang.is_empty()
        && lang.len() <= 32
        && lang
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '+')))
    .then(|| lang.to_ascii_lowercase())
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_native_html_and_escapes_raw_markup() {
        let chunks = render_telegram_chunks(
            "# Kết quả\n\n**Đã xong** với `a < b` và [docs](https://x.io/?a=1&b=2).\n\n<i>raw</i>",
            3500,
        );
        assert_eq!(chunks.len(), 1);
        let h = &chunks[0].html;
        assert!(h.contains("<b>Kết quả</b>"), "{h}");
        assert!(h.contains("<b>Đã xong</b>"), "{h}");
        assert!(h.contains("<code>a &lt; b</code>"), "{h}");
        assert!(h.contains("href=\"https://x.io/?a=1&amp;b=2\""), "{h}");
        assert!(h.contains("&lt;i&gt;raw&lt;/i&gt;"), "{h}");
    }

    #[test]
    fn unsafe_link_is_text_not_clickable() {
        let chunks = render_telegram_chunks("[boom](javascript:alert(1))", 3500);
        assert!(!chunks[0].html.contains("<a "), "{}", chunks[0].html);
        assert!(chunks[0].html.contains("boom"));
    }

    #[test]
    fn tables_are_compact_stacked_records() {
        let chunks = render_telegram_chunks(
            "| File | State |\n|---|---|\n| a.rs | **done** |\n| b.rs | pending |",
            3500,
        );
        let h = &chunks[0].html;
        assert!(!h.contains('|'), "{h}");
        assert!(h.contains("<b>File:</b> a.rs"), "{h}");
        assert!(h.contains("<b>State:</b> done"), "{h}");
        assert!(h.contains("<b>2</b>"), "{h}");
    }

    #[test]
    fn chunks_are_utf16_bounded_and_code_stays_balanced() {
        let input = format!(
            "```rust\n{}\n```",
            "println!(\"xin chào 🚀\");\n".repeat(100)
        );
        let chunks = render_telegram_chunks(&input, 260);
        assert!(chunks.len() > 1);
        for chunk in chunks {
            assert!(utf16_len(&chunk.html) <= 260, "{}", utf16_len(&chunk.html));
            assert!(
                chunk
                    .html
                    .starts_with("<pre><code class=\"language-rust\">"),
                "{}",
                chunk.html
            );
            assert!(chunk.html.ends_with("</code></pre>"), "{}", chunk.html);
        }
    }

    #[test]
    fn oversized_rich_paragraph_degrades_to_plain_but_stays_bounded() {
        let input = format!("**{}**", "đậm 🚀 ".repeat(300));
        let chunks = render_telegram_chunks(&input, 240);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| utf16_len(&c.html) <= 240));
        assert!(chunks.iter().all(|c| !c.html.contains("<b>")), "split fallback must not emit broken tags");
    }

    #[test]
    fn long_unicode_prose_chunks_without_splitting_scalars() {
        let input = "tiếng Việt 🚀 ".repeat(500);
        let chunks = render_telegram_chunks(&input, 300);
        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|c| utf16_len(&c.html) <= 300));
        assert_eq!(
            chunks
                .iter()
                .map(|c| c.plain.trim_end())
                .collect::<Vec<_>>()
                .join(" "),
            input.trim_end()
        );
    }
}
