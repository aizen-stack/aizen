//! Where the last frame actually put things, so the input thread can hit-test a mouse click.
//!
//! The draw thread owns layout; it publishes what it painted into these slots and the input thread
//! reads them. That is the only reason a click can be turned back into a transcript row, a caret
//! position inside the draft, or a selection — none of which the input thread could compute itself.

use super::*;

/// Geometry of the last transcript viewport — input thread maps mouse coords → (line, col).
#[derive(Clone, Debug, Default)]
pub(super) struct TranscriptGeom {
    pub(crate) start: usize,
    pub(crate) visible: usize,
    pub(crate) total: usize,
    pub(crate) area: Rect,
    /// Plain (SGR-stripped) wrapped rows of the full transcript at last draw — used to extract the
    /// selected text on mouse-up without re-rendering on the input thread.
    pub(crate) plain_rows: Vec<String>,
    /// Raw rendered rows WITH SGR colour codes — used by the hyperlink injector to re-print link
    /// spans baked inside OSC 8 sequences after `terminal.draw()`. Parallel to `plain_rows`.
    pub(crate) sgr_rows: Vec<String>,
    /// Screen rect of the floating "jump to bottom" button, present only while the transcript is
    /// scrolled up off the tail. `None` when at the tail (button hidden). The input thread hit-tests
    /// a left-click against this before anything else so the button lands the viewport back on tail.
    pub(crate) jump_button: Option<Rect>,
}

pub(super) fn transcript_geom_slot() -> &'static Mutex<TranscriptGeom> {
    static SLOT: OnceLock<Mutex<TranscriptGeom>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(TranscriptGeom::default()))
}

/// Snapshot of the last transcript geometry for mouse hit-testing (selection / scrollbar drag).
pub(crate) fn last_transcript_geom() -> (usize, usize, usize, Rect) {
    let g = transcript_geom_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    (g.start, g.visible, g.total, g.area)
}

/// Screen rect of the floating "jump to bottom" button at last draw, or `None` if the transcript is
/// already at the tail (button hidden). The input thread hit-tests a left-click against this.
pub(crate) fn jump_button_rect() -> Option<Rect> {
    let g = transcript_geom_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    g.jump_button
}

/// Where the last frame put a SELECTABLE overlay's option rows, so the input thread can turn a
/// left-click into a row index. Present only while a menu overlay (approval, question, model,
/// sessions, palette) is painted — informational overlays wrap their text, so their screen rows do
/// not map 1:1 to lines and they publish nothing.
#[derive(Clone, Copy, Debug)]
pub(crate) struct OverlayMenuGeom {
    /// Inner rect of the overlay panel (inside the border) — option rows start at its top row.
    pub(crate) inner: Rect,
    /// Rows hidden above the top at last draw (the clamped scroll actually applied).
    pub(crate) scroll: usize,
    /// Number of PICKABLE rows. The hint line painted below them is not one — a click there is dead.
    pub(crate) rows: usize,
}

pub(super) fn overlay_menu_slot() -> &'static Mutex<Option<OverlayMenuGeom>> {
    static SLOT: OnceLock<Mutex<Option<OverlayMenuGeom>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

/// Publish (or clear) the selectable-overlay geometry for this frame. Draw-thread only.
pub(super) fn set_overlay_menu(g: Option<OverlayMenuGeom>) {
    *overlay_menu_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = g;
}

/// Menu row index a click at (`col`, `row`) points at, or `None` when no selectable overlay is on
/// screen or the click missed its option rows (border, hint line, outside the panel).
pub(crate) fn overlay_menu_hit(col: u16, row: u16) -> Option<usize> {
    let g = (*overlay_menu_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()))?;
    menu_hit_index(&g, col, row)
}

/// Pure half of [`overlay_menu_hit`], separated so the mapping is testable without a terminal.
pub(super) fn menu_hit_index(g: &OverlayMenuGeom, col: u16, row: u16) -> Option<usize> {
    let in_x = col >= g.inner.x && col < g.inner.x.saturating_add(g.inner.width);
    let in_y = row >= g.inner.y && row < g.inner.y.saturating_add(g.inner.height);
    if !in_x || !in_y {
        return None;
    }
    let idx = g.scroll.saturating_add((row - g.inner.y) as usize);
    (idx < g.rows).then_some(idx)
}

/// Where the input box's typed text landed on screen at the last draw, so the input thread can turn a
/// mouse column into a caret position in the draft.
///
/// The draft is a `Vec<char>` but the screen is a grid of display CELLS, and the two do not line up:
/// a CJK char or an emoji is two cells wide, a `\n` is painted as a one-cell `↵`, and only a window of
/// a long draft is on screen at all. So the mapping is published as the column each visible char was
/// actually painted at — measured by the code that painted it — rather than recomputed on the input
/// thread from a width function that might disagree.
#[derive(Clone, Debug, Default)]
pub(super) struct InputGeom {
    /// Screen rect of the `❯ …` row. `width == 0` means nothing has been painted yet: no mapping.
    pub(crate) row: Rect,
    /// Draft index of the first char in the visible window.
    pub(crate) start: usize,
    /// Absolute start column of each visible char, plus one entry for the cell just past the last —
    /// where a click beyond the end of the text parks the caret. Length is `visible chars + 1`.
    pub(crate) cols: Vec<u16>,
}

pub(super) fn input_geom_slot() -> &'static Mutex<InputGeom> {
    static SLOT: OnceLock<Mutex<InputGeom>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(InputGeom::default()))
}

/// Draft index a click at column `col` points at, ignoring the row.
///
/// Row-blind on purpose: it backs the DRAG half of a selection, where the pointer routinely leaves the
/// one-row input box while the user is still selecting inside it. `input_hit` is the row-checked
/// variant that decides whether a click belongs to the box in the first place.
pub(crate) fn input_hit_col(col: u16) -> Option<usize> {
    let g = input_geom_slot().lock().unwrap_or_else(|e| e.into_inner());
    if g.row.width == 0 || g.cols.is_empty() {
        return None;
    }
    Some(hit_index(&g.cols, g.start, col))
}

/// Which draft char the cell at absolute column `col` belongs to, given a published column map. Pure so
/// the mapping — the one piece of this that an off-by-one would make silently aim one char wrong — is
/// testable without a terminal.
///
/// `cols[i + 1]` is where char `i` ends, so the first char whose end is past the click owns the cell the
/// click landed on, and the caret goes ON that cell: exactly where the block cursor is then drawn. Both
/// cells of a double-width char therefore resolve to that char, a click left of the text resolves to the
/// start of the window, and anything past the end resolves to just after the last visible char.
pub(super) fn hit_index(cols: &[u16], start: usize, col: u16) -> usize {
    let visible = cols.len().saturating_sub(1);
    for i in 0..visible {
        if col < cols[i + 1] {
            return start + i;
        }
    }
    start + visible
}

/// Draft index a click at (`col`, `row`) points at, or `None` when the click is not on the input row.
pub(crate) fn input_hit(col: u16, row: u16) -> Option<usize> {
    let on_row = {
        let g = input_geom_slot().lock().unwrap_or_else(|e| e.into_inner());
        g.row.width > 0
            && row == g.row.y
            && col >= g.row.x
            && col < g.row.x.saturating_add(g.row.width)
    };
    on_row.then(|| input_hit_col(col)).flatten()
}

/// Where `draw_footer` last asked ratatui to park the input caret (`frame.set_cursor_position`).
///
/// ratatui shows and positions the caret as the FINAL step of `draw`
/// (`apply_buffer_with_cursor`), so the hyperlink injector — which runs after that and moves the
/// cursor around to overprint spans — has to put it back. Published every frame from the draw
/// thread, read by the injector call site just below.
pub(super) fn caret_slot() -> &'static Mutex<Option<(u16, u16)>> {
    static SLOT: OnceLock<Mutex<Option<(u16, u16)>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub(super) fn last_caret() -> Option<(u16, u16)> {
    *caret_slot().lock().unwrap_or_else(|e| e.into_inner())
}

/// Screen rects painted OVER the transcript at last draw (overlay panel, Copy menu). The hyperlink
/// injector must not print link text into these — it writes at absolute coordinates after the frame
/// is composited, so anything floating above the transcript would be scribbled on.
pub(super) fn occluders_slot() -> &'static Mutex<Vec<Rect>> {
    static SLOT: OnceLock<Mutex<Vec<Rect>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(Vec::new()))
}

pub(super) fn last_occluders() -> Vec<Rect> {
    occluders_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// The selection currently PAINTED, mirrored out of the render thread's `AppState` so the input
/// thread can act on it without owning it.
///
/// The input thread's own `selecting` can NOT serve this purpose: it is `take()`n on mouse-up while
/// the highlight deliberately stays on screen, so between releasing the drag and the next click —
/// precisely when someone reaches for the right button — it is `None` even though text is visibly
/// selected. This mirror is written only by [`set_selection`] / [`clear_selection`], the same two
/// calls that change the highlight, so it cannot disagree with what is on screen.
pub(super) fn live_selection_slot() -> &'static Mutex<Option<SelectionRange>> {
    static SLOT: OnceLock<Mutex<Option<SelectionRange>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub(crate) fn live_selection() -> Option<SelectionRange> {
    *live_selection_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Extract plain text covering `sel` from the last drawn plain rows. Empty if nothing is selected
/// or geometry is stale.
pub(crate) fn extract_selection_text(sel: SelectionRange) -> String {
    let g = transcript_geom_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    extract_from_plain_rows(&g.plain_rows, sel)
}

fn extract_from_plain_rows(rows: &[String], sel: SelectionRange) -> String {
    if rows.is_empty() {
        return String::new();
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
    let a_line = a_line.min(rows.len().saturating_sub(1));
    let b_line = b_line.min(rows.len().saturating_sub(1));
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate().take(b_line + 1).skip(a_line) {
        let plain = console::strip_ansi_codes(row);
        let start_col = if i == a_line { a_col } else { 0 };
        let end_col = if i == b_line {
            b_col
        } else {
            console::measure_text_width(plain.as_ref()).saturating_add(1)
        };
        let slice = slice_by_display_cols(plain.as_ref(), start_col, end_col);
        if i > a_line {
            out.push('\n');
        }
        out.push_str(&slice);
    }
    out
}

/// Take the substring of `s` whose display-cell range is `[start_col, end_col)`.
fn slice_by_display_cols(s: &str, start_col: usize, end_col: usize) -> String {
    if end_col <= start_col {
        return String::new();
    }
    let mut out = String::new();
    let mut col = 0usize;
    for ch in s.chars() {
        let w = console::measure_text_width(&ch.to_string()).max(1);
        let next = col.saturating_add(w);
        if next > start_col && col < end_col {
            out.push(ch);
        }
        col = next;
        if col >= end_col {
            break;
        }
    }
    out
}
