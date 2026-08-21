//! Painting one frame: the transcript pane, the selection highlight, the working line, and the
//! footer with the composer — which wraps a long draft down as many rows as it needs.
//!
//! Everything here runs on the render thread and is a pure function of `AppState` plus the frame
//! it is handed — which is what makes the window/caret arithmetic testable without a terminal.

use super::*;

pub(super) fn draw(frame: &mut Frame<'_>, state: &mut AppState) {
    let area = frame.area();
    // The composer GROWS DOWNWARD with the draft instead of scrolling one row sideways, so the
    // footer's height is decided here — before the split — from the wrapped draft. `input_layout` is
    // pure over `AppState`, so the same call that sizes the box is the one handed to `draw_footer`
    // to paint it: the two cannot disagree by a row.
    let layout = input_layout(state, area.width as usize, max_input_rows(area.height));
    state.input_row_scroll = layout.scroll; // sticky across frames — see `AppState::input_row_scroll`
    let footer_rows = FOOTER_CHROME_ROWS.saturating_add(layout.rows.len() as u16);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(footer_rows)])
        .split(area);
    draw_transcript(frame, chunks[0], state);
    draw_footer(frame, chunks[1], state, &layout);
    // Rects floating ABOVE the transcript this frame. Collected here (rather than inferred later)
    // because only the draw pass knows what was actually painted — the post-draw hyperlink injector
    // writes at absolute coordinates and would otherwise print over them.
    let mut occluders: Vec<Rect> = Vec::new();
    if let Some(overlay) = state.input.overlay.clone() {
        // draw_overlay clamps the requested scroll against the overlay's own visible height and
        // returns the value actually used, so a stored offset past the end snaps back next frame.
        // It also publishes the menu hit-test geometry (selectable overlays only).
        let (scroll, rect) = draw_overlay(frame, area, &overlay, state.overlay_scroll);
        state.overlay_scroll = scroll;
        occluders.push(rect);
    } else {
        // No overlay this frame → no menu rows to click. Without this, a stale rect from the last
        // open menu would keep swallowing left-clicks as phantom row picks.
        set_overlay_menu(None);
    }
    if let Ok(mut slot) = occluders_slot().lock() {
        *slot = occluders;
    }
}

/// How many text rows the composer may occupy in a terminal `height` rows tall.
///
/// The box grows with the draft, but it must never eat the conversation: at least
/// `MIN_TRANSCRIPT_ROWS` are always left above it, and it never passes `MAX_INPUT_TEXT_ROWS` however
/// tall the terminal is — past that a pasted wall of text would BE the screen.
pub(super) fn max_input_rows(height: u16) -> usize {
    const MIN_TRANSCRIPT_ROWS: u16 = 3;
    let spare = height
        .saturating_sub(FOOTER_CHROME_ROWS)
        .saturating_sub(MIN_TRANSCRIPT_ROWS);
    spare.clamp(1, MAX_INPUT_TEXT_ROWS) as usize
}

/// Resolve the transcript scroll for one frame. Pure so it can be unit-tested without a backend.
///
/// `offset` is the current `scroll_from_tail` (wrapped lines up from the bottom; `0` = follow tail).
/// `last_total`/`total` are the wrapped-line counts at the previous and current frame; `visible` is
/// the viewport height. Returns `(start_row, new_offset, new_last_total)`:
///  - Content anchoring: while scrolled up (`offset > 0`) and lines were appended at the bottom, the
///    offset grows by the delta so the text under the reader's eyes stays put. At the bottom
///    (`offset == 0`) this is skipped, so streaming output is followed automatically.
///  - Clamp write-back: the offset is capped at `tail_start` (the top of the transcript) and returned
///    so a PageUp storm past the top can't inflate it into a dead zone that PageDown must burn through.
pub(super) fn resolve_transcript_scroll(
    offset: usize,
    last_total: usize,
    total: usize,
    visible: usize,
) -> (usize, usize, usize) {
    let tail_start = total.saturating_sub(visible);
    let anchored = if offset > 0 && total > last_total {
        offset.saturating_add(total - last_total)
    } else {
        offset
    };
    let clamped = anchored.min(tail_start);
    (tail_start - clamped, clamped, total)
}

pub(super) fn draw_transcript(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Leave 1 cell on the right for the scrollbar track when content overflows — content already
    // reserves `width-2` so the thumb never paints over text.
    let content_width = area.width.saturating_sub(2).max(8);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut plain_rows: Vec<String> = Vec::new();
    let mut sgr_rows: Vec<String> = Vec::new();
    for block in &state.blocks {
        let rows = state.cache.get_or_render(block, content_width);
        for row in rows {
            plain_rows.push(console::strip_ansi_codes(&row).into_owned());
            sgr_rows.push(row.clone());
            lines.push(styled_row(block.kind, row));
        }
    }
    // Apply selection reverse highlight before scrolling into the viewport.
    if let Some(sel) = state.selection {
        apply_selection_highlight(&mut lines, sel);
    }
    // The working indicator rides the BOTTOM of the transcript (Claude-CLI style) rather than a HUD
    // pill: a brand-bloom spinner (the ✦ mark opening out through ✶✷✹✺ and back) + a typewriter
    // caption that reads the agent's current action ("Reading retained.rs") or a whimsical verb
    // between steps. Blue caption (the aizen link-blue) so it reads as "live status", not transcript
    // prose. Appended after the selection highlight so a drag can't accidentally reverse-video the
    // spinner; it counts toward `total` so the tail-follow keeps it pinned to the last row as output
    // streams.
    if state.working {
        for (plain, styled) in working_line(state) {
            plain_rows.push(plain);
            lines.push(styled);
        }
    }
    let total = lines.len();
    let visible = area.height as usize;
    let (start, scroll, last) =
        resolve_transcript_scroll(state.scroll_from_tail, state.last_total, total, visible);
    state.scroll_from_tail = scroll;
    state.last_total = last;
    let paragraph = Paragraph::new(Text::from(lines))
        .style(Style::default().fg(Color::Gray))
        .scroll((start.min(u16::MAX as usize) as u16, 0));
    frame.render_widget(paragraph, area);

    // Dim vertical scrollbar when content overflows the viewport. Style is quiet (FAINT track,
    // MUTED thumb) so it doesn't compete with the transcript. Positioned on the right edge of
    // `area` — content_width already left a 2-cell gutter so text is never covered.
    if total > visible {
        let mut sb_state = ScrollbarState::new(total.saturating_sub(visible)).position(start);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .thumb_symbol("█")
            .style(Style::default().fg(Color::Indexed(crate::ui::theme::MUTED)))
            .track_style(Style::default().fg(Color::Indexed(crate::ui::theme::FAINT)));
        frame.render_stateful_widget(scrollbar, area, &mut sb_state);
    }

    // Floating "jump to bottom" button — only while scrolled up off the tail (`start` below the
    // tail_start means the reader has paged up). It sits in the bottom-right corner, just left of
    // the scrollbar gutter, so a click lands the viewport back on the live tail without having to
    // wheel all the way down. Hidden (rect = None) at the tail, so it never covers streaming output.
    let tail_start = total.saturating_sub(visible);
    let jump_button = if total > visible && start < tail_start {
        const LABEL: &str = " ↓ bottom ";
        let label_w = console::measure_text_width(LABEL) as u16;
        // Keep the button inside the content gutter (1 cell reserved on the right for the scrollbar).
        if area.width > label_w + 1 && area.height >= 1 {
            let bx = area.x + area.width - 1 - label_w;
            let by = area.y + area.height - 1;
            let brect = Rect::new(bx, by, label_w, 1);
            let btn = Paragraph::new(Line::from(Span::styled(
                LABEL,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Indexed(crate::ui::theme::ACCENT))
                    .add_modifier(Modifier::BOLD),
            )));
            frame.render_widget(btn, brect);
            Some(brect)
        } else {
            None
        }
    } else {
        None
    };

    // Stash geometry + plain rows so the input thread can map mouse → (line, col) and extract text.
    if let Ok(mut slot) = transcript_geom_slot().lock() {
        *slot = TranscriptGeom {
            start,
            visible,
            total,
            area,
            plain_rows,
            sgr_rows,
            jump_button,
        };
    }
}

/// Paint `REVERSED` over the spans that fall inside `sel` (absolute wrapped-line coords).
fn apply_selection_highlight(lines: &mut [Line<'static>], sel: SelectionRange) {
    if lines.is_empty() {
        return;
    }
    let (mut a_line, mut a_col, mut b_line, mut b_col) = (
        sel.anchor_line,
        sel.anchor_col,
        sel.cursor_line,
        sel.cursor_col,
    );
    if (a_line, a_col) > (b_line, b_col) {
        std::mem::swap(&mut a_line, &mut b_line);
        std::mem::swap(&mut a_col, &mut b_col);
    }
    let a_line = a_line.min(lines.len().saturating_sub(1));
    let b_line = b_line.min(lines.len().saturating_sub(1));
    for i in a_line..=b_line {
        let start_col = if i == a_line { a_col } else { 0 };
        // end_col exclusive; for mid-range lines reverse the whole row (large end_col).
        let end_col = if i == b_line { b_col } else { usize::MAX };
        if end_col <= start_col {
            continue;
        }
        reverse_line_cols(&mut lines[i], start_col, end_col);
    }
}

/// Split/restyle the spans of `line` so display-cells in `[start_col, end_col)` get REVERSED.
fn reverse_line_cols(line: &mut Line<'static>, start_col: usize, end_col: usize) {
    let old = std::mem::take(&mut line.spans);
    let mut new_spans: Vec<Span<'static>> = Vec::with_capacity(old.len() + 2);
    let mut col = 0usize;
    for span in old {
        let text = span.content;
        let style = span.style;
        if text.is_empty() {
            continue;
        }
        // Walk chars, grouping into before / inside / after the selection window.
        let mut before = String::new();
        let mut mid = String::new();
        let mut after = String::new();
        for ch in text.chars() {
            let w = console::measure_text_width(&ch.to_string()).max(1);
            let next = col.saturating_add(w);
            if next <= start_col {
                before.push(ch);
            } else if col >= end_col {
                after.push(ch);
            } else {
                mid.push(ch);
            }
            col = next;
        }
        if !before.is_empty() {
            new_spans.push(Span::styled(before, style));
        }
        if !mid.is_empty() {
            new_spans.push(Span::styled(mid, style.add_modifier(Modifier::REVERSED)));
        }
        if !after.is_empty() {
            new_spans.push(Span::styled(after, style));
        }
    }
    line.spans = new_spans;
}

fn styled_row(kind: BlockKind, row: String) -> Line<'static> {
    match kind {
        // Intro is sanitised plain (no SGR) → one flat dim line, as before.
        BlockKind::Intro => Line::styled(row, Style::default().fg(Color::DarkGray)),
        // Assistant + Generic carry SGR now: parse it into coloured spans over a grey base. The
        // moonlight `▌` gutter keeps its own colour because it rode through as SGR; uncoloured text
        // collapses to one grey span (unchanged look). The structured kinds (Tool/Plan/Diff/Verify)
        // also emit their palette as SGR from their render fns, so they take the same span path.
        BlockKind::Assistant
        | BlockKind::Generic
        | BlockKind::Tool
        | BlockKind::Plan
        | BlockKind::Diff
        | BlockKind::Verify => Line::from(ansi_spans(&row, Style::default().fg(Color::Gray))),
    }
}

/// The brand-bloom spinner: the ✦ mark opening out through stars of growing radius and closing back.
/// A ping-pong, so the cycle returns to ✦ — and since `Working(false)` resets `frame` to 0, every turn
/// OPENS on the brand mark rather than mid-bloom.
///
/// Two properties are load-bearing, not decoration:
///
///  - **Every frame renders one display cell.** The caption sits immediately to the right, so a
///    2-cell frame shoves it sideways once per cycle. This rules out the otherwise-obvious ✳
///    (U+2733) and ✴ (U+2734): both are `Emoji=Yes`, so a terminal resolving them through an emoji
///    font paints them double-width. Their `East_Asian_Width` is Narrow, so a width measurement
///    does NOT catch this — `bloom_frames_never_render_two_cells_wide` checks emoji membership,
///    not width, for exactly that reason.
///  - **No braille.** U+28xx is the universal CLI spinner; wearing it would trade the ✦ brand (shared
///    with the `✦ ultimate` chip and the synthesizing phase mark) for anonymity.
///
/// 8 frames at the ~110ms working tick ⇒ a ~880ms bloom, which is deliberately close to the classic
/// 10×80ms braille rotation — fast enough to read as motion, slow enough not to flicker. That timing
/// is why the spinner does NOT need its own clock: it shares the typewriter's tick (see
/// [`AppState::work_reveal`]) at a rate that happens to suit both.
pub(super) const BLOOM: [&str; 8] = ["✦", "✶", "✷", "✹", "✺", "✹", "✷", "✶"];

/// Build the working line(s) shown at the bottom of the transcript while a turn is in flight: a
/// brand-bloom spinner + a typewriter-revealed caption in the aizen link-blue, then the elapsed clock.
///
/// Returns `(plain, styled)` pairs so the caller can push both the mouse-mapping plain row and the
/// coloured `Line`. One visual row today, but a Vec keeps the door open for a wrapped caption.
///
/// The caption reveal (`work_reveal`) is advanced by the ticker/timeout, so this fn is pure over the
/// current state — it just slices `work_caption` to the revealed prefix.
pub(super) fn working_line(state: &AppState) -> Vec<(String, Line<'static>)> {
    use crate::ui::theme;
    let glyph = BLOOM[state.frame % BLOOM.len()];
    // Reveal the first `work_reveal` chars of the caption (char-, not byte-, indexed so multibyte
    // captions never split mid-codepoint).
    let revealed: String = state.work_caption.chars().take(state.work_reveal).collect();
    let elapsed = state
        .working_since
        .map(|t| t.elapsed().as_secs())
        .unwrap_or(0);
    // `spinner caption   12s · Esc to stop` — spinner in moonlight (the structural brand colour),
    // caption in link-blue (live status, distinct from grey transcript prose), the elapsed clock +
    // stop hint faint at the tail so they inform without pulling the eye.
    //
    // NO drawn caret rides the caption. A `▏` here used to imitate a text cursor, which misread
    // badly for two reasons: the terminal's REAL cursor is already visible (ratatui's
    // `apply_buffer_with_cursor` calls `show_cursor` whenever a frame sets a position, so the
    // `Hide` in `TerminalSession::enter` lasts exactly one frame) and it blinks a few rows below in
    // the input box — and when the caption was empty between tool steps the imitation collapsed
    // onto the spinner as `✦ ▏`, reading as a second cursor stuck to the glyph. The bloom is the
    // liveness signal; one cursor on screen is the correct number.
    let spans = vec![
        Span::styled(
            format!("{glyph} "),
            Style::default().fg(Color::Indexed(theme::ACCENT)),
        ),
        Span::styled(revealed, Style::default().fg(Color::Indexed(theme::LINK))),
        Span::styled(
            format!("   {elapsed}s · Esc to stop"),
            Style::default().fg(Color::Indexed(theme::FAINT)),
        ),
    ];
    let plain: String = spans.iter().map(|s| s.content.as_ref()).collect();
    vec![(plain, Line::from(spans))]
}

/// Paint the footer: the HUD strip, the two framing rules, and the composer's text rows between
/// them. `layout` is the one computed in [`draw`] — it already decided how tall `area` is.
pub(super) fn draw_footer(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut AppState,
    layout: &InputLayout,
) {
    if area.height < FOOTER_ROWS || area.width == 0 {
        // No input row was painted, so retire the click map with it — a stale one would keep answering
        // hit-tests for a row that is no longer on screen.
        if let Ok(mut slot) = input_geom_slot().lock() {
            *slot = InputGeom::default();
        }
        return;
    }
    // `draw` asked for `3 + layout.rows.len()`, but a Layout under pressure can hand back less than
    // it asked for — so paint what actually arrived rather than what was requested.
    let text_rows = layout
        .rows
        .len()
        .min(area.height.saturating_sub(FOOTER_CHROME_ROWS) as usize)
        .max(1);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(text_rows as u16),
            Constraint::Length(1),
        ])
        .split(area);
    let width = area.width as usize;
    // Input-box accent: gold while ultimate mode is ON (tying the box to the `✦ ultimate` chip so the
    // heightened mode is unmistakable at a glance), the usual moonlight silver otherwise. Drives the
    // `❯` prompt arrow and both framing rules. `state.ultimate` is pushed via `Command::Ultimate`
    // (never a per-frame disk read).
    let box_accent = if state.ultimate {
        crate::ui::theme::WARN
    } else {
        crate::ui::theme::ACCENT_DIM
    };
    // Right of the HUD row: a coloured health chip + a compact context meter `⟦▓▓░░…⟧ N%`.
    // (Elapsed time / spinner moved to the transcript's working line — see `working_line`.)
    // Green = OK, yellow = slow/transient, red = permanent unavailability, muted = still checking.
    // The "working" state is NO LONGER shown here — it rides the bottom of the transcript now (a
    // Claude-CLI-style spinner + typewriter caption, see `working_line`), so the HUD stays a calm
    // status strip whether or not a turn is in flight.
    //
    // Built as spans (not a single pre-coloured string) so the bar can take its own fill colour
    // independent of the health dot.
    let right_spans: Vec<Span<'static>> = {
        let pm = state.ctx_permille.min(1000);
        let pct = (pm as f64 / 10.0).round() as u16;
        // Compact bar (6 cells) so it fits beside model/status on typical widths; colour tracks fill.
        const CELLS: usize = 6;
        let filled = (pm as usize * CELLS).div_ceil(1000).min(CELLS);
        let bar_color = if pm >= 900 {
            crate::ui::theme::ERR
        } else if pm >= 700 {
            crate::ui::theme::WARN
        } else {
            crate::ui::theme::ACCENT_DIM
        };
        let mut bar = String::with_capacity(CELLS);
        for i in 0..CELLS {
            bar.push(if i < filled { '▓' } else { '░' });
        }
        let health_style = Style::default().fg(Color::Indexed(state.health.color_code()));
        let bar_style = Style::default().fg(Color::Indexed(bar_color));
        let faint = Style::default().fg(Color::Indexed(crate::ui::theme::FAINT));
        let muted = Style::default().fg(Color::Indexed(crate::ui::theme::MUTED));
        vec![
            Span::styled(format!("● {} ", state.health.label(false)), health_style),
            Span::styled("⟦".to_string(), faint),
            Span::styled(bar, bar_style),
            Span::styled("⟧ ".to_string(), faint),
            Span::styled(format!("{pct}%"), muted),
        ]
    };
    let right_plain: String = right_spans.iter().map(|s| s.content.as_ref()).collect();
    let right_w = console::measure_text_width(&right_plain);
    let left_budget = width.saturating_sub(right_w + 1);
    let left = console::truncate_str(&state.input.status, left_budget, "…").into_owned();
    let gap = width.saturating_sub(console::measure_text_width(&left) + right_w);
    let mut hud_spans = vec![
        Span::styled(left, Style::default().fg(Color::DarkGray)),
        Span::raw(" ".repeat(gap)),
    ];
    hud_spans.extend(right_spans);
    frame.render_widget(Paragraph::new(Line::from(hud_spans)), rows[0]);
    // Framing rules. When the draft is taller than the box, the rule on that side carries the count
    // of rows hidden behind it (`↑12` / `↓3`) — without it a scrolled composer is indistinguishable
    // from a truncated one, which is the very thing this box exists to stop.
    let hidden_above = layout.scroll;
    let hidden_below = layout.total.saturating_sub(layout.scroll + text_rows);
    frame.render_widget(
        Paragraph::new(rule_line(width, box_accent, hidden_above, '↑')),
        rows[1],
    );
    frame.render_widget(
        Paragraph::new(rule_line(width, box_accent, hidden_below, '↓')),
        rows[3],
    );

    let body_style = if layout.placeholder {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::White)
    };
    let sel = crate::ui::tui::normalized_draft_sel(state.input.sel, state.input.draft.len());
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(text_rows);
    for (i, row) in layout.rows.iter().take(text_rows).enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(5);
        if i == 0 {
            spans.push(Span::styled(
                "❯ ",
                Style::default()
                    .fg(Color::Indexed(box_accent))
                    .add_modifier(Modifier::BOLD),
            ));
            // A `[Nimg]` chip when vision attachments are pending (Ctrl-O / dropped files) so the box
            // shows the attachment count. Its width is baked into `layout.text_off`, so the wrap and
            // the caret math already know about it.
            if state.input.images > 0 {
                spans.push(Span::styled(
                    format!("[{}img] ", state.input.images),
                    Style::default().fg(Color::Indexed(crate::ui::theme::ACCENT)),
                ));
            }
        } else {
            // Continuation rows are indented under the first row's text, so a wrapped prompt reads as
            // one block instead of a ragged left edge that the `❯` no longer lines up with.
            spans.push(Span::raw(" ".repeat(layout.text_off)));
        }
        spans.push(Span::styled(row.shown.clone(), body_style));
        // A quiet right-aligned key hint (`↵ send · Tab complete`), only on the first row and only
        // while the draft is empty — it must never fight typed text for the row.
        if i == 0 && !layout.hint.is_empty() {
            let used = layout.text_off + console::measure_text_width(&row.shown);
            let gap = width
                .saturating_sub(used + console::measure_text_width(layout.hint))
                .max(1);
            spans.push(Span::raw(" ".repeat(gap)));
            spans.push(Span::styled(
                layout.hint.to_string(),
                Style::default().fg(Color::DarkGray),
            ));
        }
        let mut line = Line::from(spans);
        // A mouse highlight inside the box, painted the same way the transcript paints its own. Each
        // row clips the draft-index range to its own slice, so a selection spanning several wrapped
        // rows lights all of them.
        if !layout.placeholder {
            if let Some((from_cell, to_cell)) =
                sel_cells(sel, row.start, &row.cols, layout.text_off)
            {
                reverse_line_cols(&mut line, from_cell, to_cell);
            }
        }
        lines.push(line);
    }
    frame.render_widget(Paragraph::new(Text::from(lines)), rows[2]);

    // Publish where each visible char landed so the input thread can map a click to a caret position.
    if let Ok(mut slot) = input_geom_slot().lock() {
        *slot = InputGeom {
            area: rows[2],
            rows: layout
                .rows
                .iter()
                .take(text_rows)
                .enumerate()
                .map(|(i, r)| InputRowGeom {
                    y: rows[2].y.saturating_add(i as u16),
                    start: r.start,
                    cols: r
                        .cols
                        .iter()
                        .map(|c| {
                            rows[2]
                                .x
                                .saturating_add(layout.text_off as u16)
                                .saturating_add(*c as u16)
                        })
                        .collect(),
                })
                .collect(),
        };
    }
    let (caret_row, caret_col) = layout.caret;
    let caret_row = caret_row.min(text_rows.saturating_sub(1));
    let cursor_x = rows[2]
        .x
        .saturating_add(layout.text_off as u16)
        .saturating_add(caret_col as u16);
    let caret = (
        cursor_x.min(rows[2].right().saturating_sub(1)),
        rows[2].y.saturating_add(caret_row as u16),
    );
    frame.set_cursor_position(caret);
    // Publish it so the post-draw hyperlink injector can restore the caret after moving the cursor.
    if let Ok(mut slot) = caret_slot().lock() {
        *slot = Some(caret);
    }
}

/// One framing rule of the input box. `hidden` > 0 stamps a `↑N` / `↓N` tag into the rule near its
/// right end — the only place a scrolled composer can report what is off screen without stealing a
/// cell from the text.
fn rule_line(width: usize, accent: u8, hidden: usize, arrow: char) -> Line<'static> {
    let accent_style = Style::default().fg(Color::Indexed(accent));
    let tag = format!(" {arrow}{hidden} ");
    let tag_w = console::measure_text_width(&tag);
    if hidden == 0 || width < tag_w + 6 {
        return Line::styled("─".repeat(width), accent_style);
    }
    Line::from(vec![
        Span::styled("─".repeat(width - tag_w - 2), accent_style),
        Span::styled(
            tag,
            Style::default().fg(Color::Indexed(crate::ui::theme::MUTED)),
        ),
        Span::styled("──".to_string(), accent_style),
    ])
}

/// Row cells `[from, to)` that a draft selection covers, clipped to the window actually on screen.
///
/// The selection is in DRAFT indices and the window is only part of the draft, so half of it — or all of
/// it — can be off screen; `cols` maps only what is visible. `None` when nothing of it lands in the
/// window, which is also what a collapsed selection gives. `text_off` is where the draft text starts
/// within the row (`❯ ` plus any `[Nimg]` chip), because [`reverse_line_cols`] counts from the row edge.
pub(super) fn sel_cells(
    sel: Option<(usize, usize)>,
    win_start: usize,
    cols: &[usize],
    text_off: usize,
) -> Option<(usize, usize)> {
    let (a, b) = sel?;
    let visible = cols.len().saturating_sub(1);
    let from = a.saturating_sub(win_start).min(visible);
    let to = b.saturating_sub(win_start).min(visible);
    // `b <= win_start` collapses to (0, 0) and a selection entirely to the right collapses to
    // (visible, visible) — both correctly read as nothing to paint.
    (to > from).then(|| (text_off + cols[from], text_off + cols[to]))
}

/// One painted row of the composer: what is on it, which draft chars they are, and where each of
/// them landed. The column map is the whole point — see [`InputGeom`].
pub(super) struct InputRowView {
    /// The text painted after `❯ ` (first row) or the matching indent (continuation rows). An
    /// embedded newline is painted as a blank cell — the row break itself is the glyph.
    pub(crate) shown: String,
    /// Draft index of the first char on this row.
    pub(crate) start: usize,
    /// Display cells from the start of the text area to the start of char `start + i`, plus one
    /// trailing entry for the cell just past the last char. Never empty.
    pub(crate) cols: Vec<usize>,
}

/// The composer's geometry for one frame: the wrapped draft rows that are on screen, where the caret
/// sits among them, and how far the window is scrolled.
///
/// Computed ONCE per frame in [`draw`] — it is what decides the footer's height — then handed to
/// [`draw_footer`] to paint. Pure over [`AppState`], so the arithmetic that used to be tangled up
/// with the widget calls is testable without a terminal.
pub(super) struct InputLayout {
    /// Only the rows actually on screen (`scroll..scroll + max_rows` of the wrapped draft).
    pub(crate) rows: Vec<InputRowView>,
    /// First wrapped row on screen. `0` unless the draft is taller than the box.
    pub(crate) scroll: usize,
    /// Total wrapped rows in the draft, on screen or not.
    pub(crate) total: usize,
    /// Caret as `(index into rows, display cells from the start of the text area)`.
    pub(crate) caret: (usize, usize),
    /// Cells from the row's left edge to its first text cell: `❯ ` plus any `[Nimg]` chip. The same
    /// on every row, so wrapped continuations line up under the first one.
    pub(crate) text_off: usize,
    /// `rows[0].shown` is the grey placeholder rather than draft text: paint it dim, map no chars.
    pub(crate) placeholder: bool,
    /// Right-aligned key hint for the first row. Empty unless the draft is empty.
    pub(crate) hint: &'static str,
}

/// Display cells one draft char occupies. A newline owns a cell of its own (painted blank) so a
/// click at the end of a line can park the caret BEFORE the break instead of at the start of the
/// next line, and so a selection that spans it shows the break it is about to copy.
fn draft_cellw(c: char) -> usize {
    // The whole draft is re-wrapped on every frame, so the common case must not allocate:
    // printable ASCII is width 1 by definition, and only the rest is worth a measuring call.
    if c == '\n' || c == ' ' || c.is_ascii_graphic() {
        return 1;
    }
    console::measure_text_width(&c.to_string()).max(1)
}

/// Lay a draft out over as many rows as it needs, at `width` display cells per row.
///
/// Two things end a row: an embedded newline (Shift+Enter, or a pasted block) and running out of
/// width. The width break prefers the last space on the row, so a long prompt wraps as prose instead
/// of being guillotined mid-word; a single word wider than the box still breaks hard, because the
/// alternative is a row that never ends.
///
/// Always returns at least one row, and always ends one: a draft ending in a newline gets the empty
/// trailing row the caret is sitting on.
fn wrap_draft(draft: &[char], width: usize) -> Vec<InputRowView> {
    let width = width.max(1);
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let (mut i, mut start, mut used) = (0usize, 0usize, 0usize);
    // Draft index just AFTER the last space on the row being built — the preferred break point.
    let mut last_space: Option<usize> = None;
    while i < draft.len() {
        let c = draft[i];
        let cw = draft_cellw(c);
        if used + cw > width && i > start {
            let brk = last_space.filter(|b| *b > start && *b < i).unwrap_or(i);
            ranges.push((start, brk));
            start = brk;
            used = 0;
            last_space = None;
            // Re-lay from the break: chars moved off the full row have to be measured again on the
            // fresh one. `brk > start` held before the assignment, so this always advances.
            i = brk;
            continue;
        }
        used += cw;
        i += 1;
        if c == '\n' {
            ranges.push((start, i));
            start = i;
            used = 0;
            last_space = None;
        } else if c == ' ' {
            last_space = Some(i);
        }
    }
    ranges.push((start, draft.len()));
    ranges
        .into_iter()
        .map(|(s, e)| row_view(draft, s, e))
        .collect()
}

/// Measure one row range into the map the painter and the hit-test both read.
fn row_view(draft: &[char], start: usize, end: usize) -> InputRowView {
    let mut shown = String::new();
    let mut cols = Vec::with_capacity(end.saturating_sub(start) + 1);
    let mut used = 0usize;
    for &c in &draft[start..end] {
        cols.push(used);
        shown.push(if c == '\n' { ' ' } else { c });
        used += draft_cellw(c);
    }
    // One entry past the last char, so a click beyond the text has a cell to land on.
    cols.push(used);
    InputRowView { shown, start, cols }
}

/// Which row the caret belongs to. A row spanning `[start, end)` owns carets `[start, end)`; the
/// caret at the very end of the draft belongs to the last row, which is why the fallback is not an
/// error.
fn caret_row_of(rows: &[InputRowView], cursor: usize) -> usize {
    for (i, r) in rows.iter().enumerate() {
        if cursor < r.start + r.cols.len().saturating_sub(1) {
            return i;
        }
    }
    rows.len().saturating_sub(1)
}

/// The grey line shown in place of the draft when there is nothing typed.
fn placeholder_text(state: &AppState) -> String {
    let q = if state.input.queued_count > 0 {
        format!(" · {} queued", state.input.queued_count)
    } else {
        String::new()
    };
    // Mirror the classic footer: while working, advertise Alt+Enter (steer the RUNNING turn) and
    // count steers the loop hasn't folded in yet — a steer has no transcript line of its own
    // until the agent picks it up, so this counter is the only "it landed" feedback.
    let s = match crate::core::steer::pending() {
        0 => String::new(),
        n => format!(" · ⤳{n}"),
    };
    if state.working {
        format!("Queue · Alt+↵ steers · Esc stops{q}{s}")
    } else {
        format!("Type a message · / commands{q}")
    }
}

/// Wrap the draft, place the caret in it, and pick the window of rows to show.
///
/// `width` is the full footer width; `max_rows` the tallest the box may grow (see
/// [`max_input_rows`]). Past that the window SCROLLS by rows — but only as far as the caret forces
/// it. Re-deriving the window from the caret every frame would pin the caret to the bottom row, so a
/// glance back up a long paste would snap away the moment anything repainted.
pub(super) fn input_layout(state: &AppState, width: usize, max_rows: usize) -> InputLayout {
    let max_rows = max_rows.max(1);
    let imgtag_w = if state.input.images > 0 {
        console::measure_text_width(&format!("[{}img] ", state.input.images))
    } else {
        0
    };
    // `❯ ` plus the chip. Reserved on continuation rows too, so wrapped text stays in one column.
    let text_off = 2 + imgtag_w;

    if state.input.draft.is_empty() {
        let hint = if state.input.overlay.is_none() {
            "↵ send · Tab complete"
        } else {
            ""
        };
        let hint_w = if hint.is_empty() {
            0
        } else {
            console::measure_text_width(hint) + 1
        };
        let budget = width.saturating_sub(text_off + hint_w + 1);
        // No draft ⇒ no chars to map: `cols` carries only the past-the-end entry, so any click on the
        // placeholder resolves to caret 0 (which is where an empty draft's caret already is).
        return InputLayout {
            rows: vec![InputRowView {
                shown: console::truncate_str(&placeholder_text(state), budget, "…").into_owned(),
                start: 0,
                cols: vec![0],
            }],
            scroll: 0,
            total: 1,
            caret: (0, 0),
            text_off,
            placeholder: true,
            hint,
        };
    }

    // One cell is held back on the right so the block caret has somewhere to sit at the end of a full
    // row, and so wrapped text never touches the frame edge.
    let wrap_w = width.saturating_sub(text_off + 1).max(1);
    let rows = wrap_draft(&state.input.draft, wrap_w);
    let cursor = state.input.cursor.min(state.input.draft.len());
    let caret_row = caret_row_of(&rows, cursor);
    let total = rows.len();

    // Sticky window: it moves only as far as the caret forces it.
    let mut scroll = state.input_row_scroll.min(total.saturating_sub(max_rows));
    if caret_row < scroll {
        scroll = caret_row;
    } else if caret_row >= scroll + max_rows {
        scroll = caret_row + 1 - max_rows;
    }
    let caret_col = rows
        .get(caret_row)
        .map(|r| {
            let within = cursor.saturating_sub(r.start);
            r.cols
                .get(within)
                .copied()
                .unwrap_or_else(|| r.cols.last().copied().unwrap_or(0))
        })
        .unwrap_or(0);
    let visible: Vec<InputRowView> = rows.into_iter().skip(scroll).take(max_rows).collect();
    InputLayout {
        caret: (caret_row.saturating_sub(scroll), caret_col),
        rows: visible,
        scroll,
        total,
        text_off,
        placeholder: false,
        hint: "",
    }
}
