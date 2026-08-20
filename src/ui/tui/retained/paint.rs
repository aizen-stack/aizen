//! Painting one frame: the transcript pane, the selection highlight, the working line, and the
//! footer with the input row.
//!
//! Everything here runs on the render thread and is a pure function of `AppState` plus the frame
//! it is handed — which is what makes the window/caret arithmetic testable without a terminal.

use super::*;

pub(super) fn draw(frame: &mut Frame<'_>, state: &mut AppState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(FOOTER_ROWS)])
        .split(area);
    draw_transcript(frame, chunks[0], state);
    draw_footer(frame, chunks[1], state);
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

pub(super) fn draw_footer(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    if area.height < FOOTER_ROWS || area.width == 0 {
        // No input row was painted, so retire the click map with it — a stale one would keep answering
        // hit-tests for a row that is no longer on screen.
        if let Ok(mut slot) = input_geom_slot().lock() {
            *slot = InputGeom::default();
        }
        return;
    }
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
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
    let rule = "─".repeat(width);
    frame.render_widget(
        Paragraph::new(Line::styled(
            rule.clone(),
            Style::default().fg(Color::Indexed(box_accent)),
        )),
        rows[1],
    );

    // A quiet right-aligned key hint (`↵ send · Tab complete`) shown only while the draft is empty
    // and an overlay isn't up — it must never fight the typed text or the `/`-command list for the
    // row. Reserve its width from the typing budget so the hint and the input never overlap.
    let hint = if state.input.draft.is_empty() && state.input.overlay.is_none() {
        "↵ send · Tab complete"
    } else {
        ""
    };
    let hint_w = if hint.is_empty() {
        0
    } else {
        console::measure_text_width(hint) + 1
    };
    // A `[Nimg]` chip when vision attachments are pending (Ctrl-O / dropped files) so the input box
    // shows the attachment count — matches the classic renderer. Its width is reserved from the
    // typing budget so it never overlaps the typed text or the caret math.
    let imgtag = if state.input.images > 0 {
        format!("[{}img] ", state.input.images)
    } else {
        String::new()
    };
    let imgtag_w = console::measure_text_width(&imgtag);
    let type_budget = width.saturating_sub(3 + imgtag_w + hint_w);
    let row = input_line(state, type_budget);
    let InputRow {
        shown,
        caret_off: cursor_off,
        start: win_start,
        cols: char_cols,
    } = row;
    state.input_win_start = win_start; // sticky across frames — see `AppState::input_win_start`
    let shown_w = console::measure_text_width(&shown);
    // Pad between the typed text and the right-aligned hint. `3` = `❯ ` (2) + one right margin.
    let hint_gap = width
        .saturating_sub(3 + imgtag_w + shown_w + hint_w.saturating_sub(1))
        .max(if hint.is_empty() { 0 } else { 1 });
    let mut prompt_spans = vec![Span::styled(
        "❯ ",
        Style::default()
            .fg(Color::Indexed(box_accent))
            .add_modifier(Modifier::BOLD),
    )];
    if !imgtag.is_empty() {
        prompt_spans.push(Span::styled(
            imgtag,
            Style::default().fg(Color::Indexed(crate::ui::theme::ACCENT)),
        ));
    }
    prompt_spans.push(Span::styled(shown, Style::default().fg(Color::White)));
    if !hint.is_empty() {
        prompt_spans.push(Span::raw(" ".repeat(hint_gap)));
        prompt_spans.push(Span::styled(
            hint.to_string(),
            Style::default().fg(Color::DarkGray),
        ));
    }
    // Cells from the start of the row to the first char of the visible draft window: `❯ ` plus the
    // `[Nimg]` chip. Both the highlight (row-relative) and the click map (absolute) hang off this.
    let text_off = 2 + imgtag_w;
    let mut prompt_line = Line::from(prompt_spans);
    // A mouse highlight inside the box, painted the same way the transcript paints its own.
    if let Some((from_cell, to_cell)) = sel_cells(
        crate::ui::tui::normalized_draft_sel(state.input.sel, state.input.draft.len()),
        win_start,
        &char_cols,
        text_off,
    ) {
        reverse_line_cols(&mut prompt_line, from_cell, to_cell);
    }
    frame.render_widget(Paragraph::new(prompt_line), rows[2]);
    frame.render_widget(
        Paragraph::new(Line::styled(
            rule,
            Style::default().fg(Color::Indexed(box_accent)),
        )),
        rows[3],
    );
    // Publish where each visible char landed so the input thread can map a click to a caret position.
    if let Ok(mut slot) = input_geom_slot().lock() {
        *slot = InputGeom {
            row: rows[2],
            start: win_start,
            cols: char_cols
                .iter()
                .map(|c| {
                    rows[2]
                        .x
                        .saturating_add(text_off as u16)
                        .saturating_add(*c as u16)
                })
                .collect(),
        };
    }
    let cursor_x = rows[2]
        .x
        .saturating_add(2)
        .saturating_add(imgtag_w as u16)
        .saturating_add(cursor_off as u16);
    let caret = (cursor_x.min(rows[2].right().saturating_sub(1)), rows[2].y);
    frame.set_cursor_position(caret);
    // Publish it so the post-draw hyperlink injector can restore the caret after moving the cursor.
    if let Ok(mut slot) = caret_slot().lock() {
        *slot = Some(caret);
    }
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

/// One painted input row: what fits on screen, where the caret goes, and where every visible char
/// landed. Returned as a struct because the column map is the whole point — see [`InputGeom`].
pub(super) struct InputRow {
    /// The text painted after `❯ ` and the `[Nimg]` chip: the window of the draft that fits, behind the
    /// `↵N · ` paste chip when the draft is many lines. The placeholder text when the draft is empty.
    pub(crate) shown: String,
    /// Display cells from the start of `shown` to the caret.
    pub(crate) caret_off: usize,
    /// Draft index of the first char of the visible window (`0` unless a long draft scrolled).
    pub(crate) start: usize,
    /// Display cells from the start of `shown` to the start of visible char `i` — i.e. draft char
    /// `start + i` — plus one trailing entry for the cell just past the last char. Never empty.
    pub(crate) cols: Vec<usize>,
}

pub(super) fn input_line(state: &AppState, budget: usize) -> InputRow {
    if state.input.draft.is_empty() {
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
        let ph = if state.working {
            format!("Queue · Alt+↵ steers · Esc stops{q}{s}")
        } else {
            format!("Type a message · / commands{q}")
        };
        // No draft ⇒ no chars to map: `cols` carries only the past-the-end entry, so any click on the
        // placeholder resolves to caret 0 (which is where an empty draft's caret already is).
        return InputRow {
            shown: console::truncate_str(&ph, budget, "…").into_owned(),
            caret_off: 0,
            start: 0,
            cols: vec![0],
        };
    }
    // Khi draft có ≥5 dòng (paste lớn), vẫn hiện prefix compact `↵N ·` để báo hiệu,
    // nhưng KHÔNG ẩn toàn bộ text — window quanh cursor vẫn hiện bình thường để người
    // dùng thấy những gì vừa gõ thêm. Trước đây trả về chip cứng nên text bị ẩn.
    let nlines = state.input.draft.iter().filter(|&&c| c == '\n').count() + 1;
    let paste_prefix = if nlines >= 5 {
        format!("↵{nlines} · ")
    } else {
        String::new()
    };
    let prefix_w = console::measure_text_width(&paste_prefix);
    let cursor = state.input.cursor.min(state.input.draft.len());
    // The input box is a single physical row, so render an embedded newline as a visible `↵`
    // glyph (width 1) rather than a raw `\n` that ratatui can't lay out on one line.
    let disp = |c: char| -> char {
        if c == '\n' {
            '↵'
        } else {
            c
        }
    };
    let cellw = |c: char| console::measure_text_width(&disp(c).to_string()).max(1);
    // Budget for the text window shrinks by the paste prefix (e.g. `↵12 · `) so both fit on one row.
    let text_budget = budget.saturating_sub(prefix_w);
    let draft = &state.input.draft;
    // ── Sticky window ──────────────────────────────────────────────────────────────────────────────
    // Start from where the window was last frame and move it only as far as the caret forces. One cell
    // is held back from the right edge so the block caret has somewhere to sit at the end of the text.
    let cap = text_budget.saturating_sub(1);
    let mut start = state.input_win_start.min(cursor);
    // Caret past the right edge (typing, or a jump to the end) → scroll right until it fits.
    let mut to_caret: usize = draft[start..cursor].iter().map(|&c| cellw(c)).sum();
    while to_caret > cap && start < cursor {
        to_caret -= cellw(draft[start]);
        start += 1;
    }
    // Tail no longer fills the window (after deleting, or a wider terminal) → pull it back left, so a
    // scrolled draft never shows dead space on the right with text hidden off the left.
    let mut tail: usize = draft[start..].iter().map(|&c| cellw(c)).sum();
    while start > 0 {
        let cw = cellw(draft[start - 1]);
        if tail + cw > cap {
            break;
        }
        tail += cw;
        start -= 1;
    }
    let mut shown = String::new();
    let mut cols = Vec::with_capacity(text_budget + 1);
    let mut used = 0usize;
    for &c in &draft[start..] {
        let cw = cellw(c);
        if used + cw > text_budget {
            break;
        }
        cols.push(prefix_w + used);
        shown.push(disp(c));
        used += cw;
    }
    // One entry past the last visible char, so a click beyond the text has a cell to land on.
    cols.push(prefix_w + used);
    // Re-measure the caret against the window that was actually chosen: the pull-back loop above can
    // move `start` left after `to_caret` was computed.
    let caret: usize = draft[start..cursor].iter().map(|&c| cellw(c)).sum();
    // Prepend the paste-count prefix and offset the cursor position past it.
    InputRow {
        shown: format!("{paste_prefix}{shown}"),
        caret_off: prefix_w + caret,
        start,
        cols,
    }
}
