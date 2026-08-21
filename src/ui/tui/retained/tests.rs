//! Unit tests for the retained backend. Kept beside it rather than inline: the module was a
//! third of the file.

use super::*;

#[test]
fn overlay_menu_hit_maps_rows_scroll_and_dead_zones() {
    let g = OverlayMenuGeom {
        inner: Rect {
            x: 10,
            y: 5,
            width: 40,
            height: 6,
        },
        scroll: 2,
        rows: 7,
    };
    // Top visible row is index `scroll`; the mapping is row-relative plus scroll.
    assert_eq!(menu_hit_index(&g, 10, 5), Some(2), "top row, left edge");
    assert_eq!(
        menu_hit_index(&g, 49, 9),
        Some(6),
        "last pickable row, right edge"
    );
    // Inside the panel but past the pickable rows (the hint line) → dead, not a phantom pick.
    assert_eq!(menu_hit_index(&g, 20, 10), None, "hint row is not pickable");
    // Outside the panel on every side → None (the click belongs to the transcript handler).
    assert_eq!(menu_hit_index(&g, 9, 6), None, "left of the panel");
    assert_eq!(menu_hit_index(&g, 50, 6), None, "right of the panel");
    assert_eq!(menu_hit_index(&g, 20, 4), None, "above the panel");
    assert_eq!(menu_hit_index(&g, 20, 11), None, "below the panel");
    // The published slot round-trips, and clearing it kills the hit-test (stale-rect guard).
    set_overlay_menu(Some(g));
    assert_eq!(overlay_menu_hit(10, 5), Some(2));
    set_overlay_menu(None);
    assert_eq!(
        overlay_menu_hit(10, 5),
        None,
        "no overlay → no phantom picks"
    );
}

#[test]
fn sanitizes_terminal_control_sequences() {
    let got = sanitize_text("ok\x1b[2J\x1b[31mred\x1b[0m\nnext\r");
    assert_eq!(got, "okred\nnext");
}

#[test]
fn keep_sgr_drops_cursor_moves_but_keeps_colour() {
    // Erase (`\x1b[2J`) and a save-cursor (`\x1b7`) are stripped; the colour codes survive so
    // `ansi_spans` can turn them into styled spans.
    let got = sanitize_keep_sgr("\x1b7\x1b[2Jok\x1b[31mred\x1b[0m\nnext\r");
    assert_eq!(got, "ok\x1b[31mred\x1b[0m\nnext");
}

/// A draft of `text` with the caret at `cursor` and the box scrolled to wrapped row `row_scroll`.
fn input_state(text: &str, cursor: usize, row_scroll: usize) -> AppState {
    let mut state = AppState::new("intro", "status");
    state.input.draft = text.chars().collect();
    state.input.cursor = cursor;
    state.input_row_scroll = row_scroll;
    state
}

/// Full text of a laid-out box, rows joined by `|` — enough to assert where the breaks landed.
fn joined(layout: &InputLayout) -> String {
    layout
        .rows
        .iter()
        .map(|r| r.shown.clone())
        .collect::<Vec<_>>()
        .join("|")
}

#[test]
fn a_long_draft_wraps_downward_instead_of_scrolling_away() {
    // THE BUG: the box was ONE row, so everything before the window slid off the left and could not
    // be read back or copied. A draft wider than the box must now occupy more rows, with every char
    // of it still on screen.
    let long: String = ('a'..='z').cycle().take(80).collect();
    // width 24 → text area 24 - 2 (`❯ `) - 1 (caret margin) = 21 cells per row.
    let layout = input_layout(&input_state(&long, 80, 0), 24, 10);
    assert!(layout.rows.len() > 1, "80 chars at 21 cells must wrap");
    assert_eq!(layout.total, 4, "80 chars / 21 cells = 4 rows");
    let painted: String = layout.rows.iter().map(|r| r.shown.clone()).collect();
    assert_eq!(painted, long, "every char stays on screen, in order");
    assert_eq!(layout.scroll, 0, "4 rows fit under a 10-row cap");
}

#[test]
fn wrapping_breaks_on_spaces_but_still_splits_a_long_word() {
    // Prose wraps at the last space, so a wrapped prompt reads as prose. Width 11 leaves 8 cells of
    // text (11 - 2 for `❯ ` - 1 for the caret margin), so each word lands on its own row.
    let layout = input_layout(&input_state("hello world again", 0, 0), 11, 10);
    assert_eq!(joined(&layout), "hello |world |again");
    // A single word wider than the box has no space to break on — it breaks hard rather than
    // producing a row that never ends.
    let layout = input_layout(&input_state("aaaaaaaaaaaaaaaa", 0, 0), 14, 10);
    assert_eq!(joined(&layout), "aaaaaaaaaaa|aaaaa");
}

#[test]
fn embedded_newlines_start_a_new_row() {
    // Shift+Enter and pasted blocks break rows on their own, whatever the width. The newline keeps a
    // cell of its own (painted blank) so a click at the end of a line lands BEFORE the break.
    let layout = input_layout(&input_state("a\nb\nc", 5, 0), 40, 10);
    assert_eq!(layout.rows.len(), 3);
    assert_eq!(joined(&layout), "a |b |c");
    assert_eq!(layout.rows[1].start, 2, "row 2 starts after the first \\n");
    // A draft ending in a newline gets the empty row the caret is sitting on.
    let layout = input_layout(&input_state("a\n", 2, 0), 40, 10);
    assert_eq!(layout.rows.len(), 2);
    assert_eq!(layout.caret, (1, 0), "caret on the fresh empty row");
}

#[test]
fn the_caret_lands_on_the_row_that_owns_it() {
    let layout = input_layout(&input_state("abc\ndef", 5, 0), 40, 10);
    assert_eq!(layout.caret, (1, 1), "second row, one cell in");
    // End of the draft: the last row, past its last char.
    let layout = input_layout(&input_state("abc\ndef", 7, 0), 40, 10);
    assert_eq!(layout.caret, (1, 3));
    // On the newline itself: still the END of the first row, not the start of the second.
    let layout = input_layout(&input_state("abc\ndef", 3, 0), 40, 10);
    assert_eq!(layout.caret, (0, 3));
}

#[test]
fn the_box_scrolls_by_rows_only_once_it_hits_its_ceiling() {
    let many: String = (0..8).map(|i| format!("line{i}\n")).collect();
    let end = many.chars().count();
    // 9 wrapped rows (8 newline-terminated + the trailing empty one), box capped at 3.
    let layout = input_layout(&input_state(&many, end, 0), 40, 3);
    assert_eq!(layout.total, 9);
    assert_eq!(layout.rows.len(), 3, "never taller than the cap");
    assert_eq!(layout.scroll, 6, "the caret's row is the bottom one");
    assert_eq!(layout.caret.0, 2);
    // Caret walked back above the window → the window follows it up.
    let layout = input_layout(&input_state(&many, 0, 6), 40, 3);
    assert_eq!(layout.scroll, 0);
    // Caret INSIDE the window → the window does not move, so reading back a long paste is stable.
    let layout = input_layout(&input_state(&many, end - 1, 6), 40, 3);
    assert_eq!(
        layout.scroll, 6,
        "a caret already on screen must not scroll"
    );
}

#[test]
fn max_input_rows_leaves_the_transcript_room() {
    assert_eq!(max_input_rows(40), 10, "capped by MAX_INPUT_TEXT_ROWS");
    assert_eq!(max_input_rows(10), 4, "10 - 3 chrome - 3 transcript");
    assert_eq!(max_input_rows(6), 1, "a tiny terminal still gets one row");
    assert_eq!(max_input_rows(1), 1, "never zero");
}

#[test]
fn an_empty_draft_is_one_placeholder_row() {
    let layout = input_layout(&AppState::new("intro", "status"), 60, 10);
    assert_eq!(layout.rows.len(), 1);
    assert!(layout.placeholder);
    assert!(layout.rows[0].shown.starts_with("Type a message"));
    assert_eq!(
        layout.rows[0].cols,
        vec![0],
        "the placeholder maps no chars, so a click on it parks the caret at 0"
    );
    assert_eq!(layout.hint, "↵ send · Tab complete");
}

#[test]
fn column_map_covers_every_char_on_the_row_and_one_past_the_end() {
    let layout = input_layout(&input_state("abc", 3, 0), 40, 10);
    assert_eq!(
        layout.rows[0].cols,
        vec![0, 1, 2, 3],
        "3 chars + the past-the-end cell"
    );
    // A wide char takes two cells, so the map is not a 1:1 index → column relation. `♥` is width 1;
    // use a CJK char, which every wcwidth table agrees is 2.
    let wide = input_layout(&input_state("a漢b", 3, 0), 40, 10);
    assert_eq!(wide.rows[0].cols, vec![0, 1, 3, 4]);
    // Both cells of the wide char resolve to it; past the end lands after the last char.
    let cols: Vec<u16> = wide.rows[0].cols.iter().map(|c| *c as u16).collect();
    assert_eq!(hit_index(&cols, 0, 1), 1, "left cell of 漢");
    assert_eq!(
        hit_index(&cols, 0, 2),
        1,
        "right cell of 漢 is the same char"
    );
    assert_eq!(hit_index(&cols, 0, 3), 2, "the char after it");
    assert_eq!(
        hit_index(&cols, 0, 9),
        3,
        "far right of the row → end of text"
    );
    assert_eq!(hit_index(&cols, 0, 0), 0, "first cell");
}

#[test]
fn a_selection_spanning_wrapped_rows_lights_every_row_it_covers() {
    // The highlight is one range of DRAFT indices but several rows of cells, so each row clips it to
    // its own slice. Without that, only the row the drag started on would light up.
    let layout = input_layout(&input_state("hello world again", 0, 0), 11, 10);
    // Rows: "hello " (0..6), "world " (6..12), "again" (12..17). Select "lo world ag".
    let sel = Some((3usize, 14usize));
    let off = layout.text_off;
    assert_eq!(
        sel_cells(sel, layout.rows[0].start, &layout.rows[0].cols, off),
        Some((off + 3, off + 6)),
        "from the anchor to the end of the first row"
    );
    assert_eq!(
        sel_cells(sel, layout.rows[1].start, &layout.rows[1].cols, off),
        Some((off, off + 6)),
        "the whole middle row"
    );
    assert_eq!(
        sel_cells(sel, layout.rows[2].start, &layout.rows[2].cols, off),
        Some((off, off + 2)),
        "up to the cursor on the last row"
    );
}

#[test]
fn hit_index_offsets_by_the_window_start() {
    // The map only covers what is visible, so the index it yields is relative to the window.
    let cols: Vec<u16> = vec![10, 11, 12, 13];
    assert_eq!(hit_index(&cols, 40, 10), 40);
    assert_eq!(hit_index(&cols, 40, 12), 42);
    // Left of the text (the `❯ ` prompt) parks the caret at the start of the window, not at 0 —
    // char 0 of the draft may be scrolled far off screen.
    assert_eq!(hit_index(&cols, 40, 0), 40);
}

#[test]
fn selection_cells_clip_to_the_visible_window() {
    // Window shows draft chars 10..14 at row cells 2..6 (text_off = 2 for `❯ `).
    let cols = vec![0, 1, 2, 3, 4];
    // Fully inside.
    assert_eq!(sel_cells(Some((11, 13)), 10, &cols, 2), Some((3, 5)));
    // Starts before the window → clipped to its left edge.
    assert_eq!(sel_cells(Some((0, 12)), 10, &cols, 2), Some((2, 4)));
    // Runs past the end → clipped to the last visible cell.
    assert_eq!(sel_cells(Some((12, 99)), 10, &cols, 2), Some((4, 6)));
    // Entirely off-window in either direction, and nothing selected at all → nothing painted.
    assert_eq!(sel_cells(Some((0, 9)), 10, &cols, 2), None);
    assert_eq!(sel_cells(Some((20, 30)), 10, &cols, 2), None);
    assert_eq!(sel_cells(None, 10, &cols, 2), None);
}

#[test]
fn ansi_spans_splits_on_colour_and_collapses_when_plain() {
    // A plain row (no SGR) → exactly one span in the base style, so uncoloured output is byte-
    // identical to before.
    let plain = ansi_spans("hello", Style::default().fg(Color::Gray));
    assert_eq!(plain.len(), 1);
    assert_eq!(plain[0].content.as_ref(), "hello");

    // A coloured segment becomes its own span; a reset (`\x1b[0m`) returns to the base style.
    let spans = ansi_spans("a\x1b[31mred\x1b[0mb", Style::default().fg(Color::Gray));
    let texts: Vec<&str> = spans.iter().map(|s| s.content.as_ref()).collect();
    assert_eq!(texts, vec!["a", "red", "b"]);
    assert_eq!(
        spans[1].style.fg,
        Some(Color::Red),
        "the middle span is red"
    );
    assert_eq!(
        spans[2].style.fg,
        Some(Color::Gray),
        "reset returns to base"
    );
}

#[test]
fn ansi_spans_reads_256_colour() {
    // The app emits 256-colour SGR (`\x1b[38;5;Nm`) for its accent/ok/err palette — parse it.
    let spans = ansi_spans(
        "\x1b[38;5;71mgreen\x1b[0m",
        Style::default().fg(Color::Gray),
    );
    assert_eq!(spans[0].style.fg, Some(Color::Indexed(71)));
}

#[test]
fn assistant_cache_is_width_and_content_keyed() {
    let mut cache = RenderCache::default();
    let mut block = UiBlock {
        id: 7,
        kind: BlockKind::Assistant,
        payload: Payload::Text("hello world".into()),
        complete: false,
    };
    let a = cache.get_or_render(&block, 40);
    let b = cache.get_or_render(&block, 40);
    assert_eq!(a, b);
    assert_eq!(cache.hits, 1);
    if let Payload::Text(s) = &mut block.payload {
        s.push('!');
    }
    let _ = cache.get_or_render(&block, 40);
    let _ = cache.get_or_render(&block, 20);
    assert_eq!(cache.misses, 3);
}

#[test]
fn assistant_rows_consume_inline_code_backticks() {
    // Regression guard for the live/replay parity fix: the live retained path now renders the
    // active assistant block through the SAME `MarkdownStream` the replay path uses, instead of
    // the old pulldown-cmark `render_retained`. The old renderer left inline `` `code` `` as
    // literal backticks (`line.push('`')`); `MarkdownStream`'s `inline()` consumes them and tints
    // the span. Backtick-stripping is the one divergence observable WITHOUT a colour terminal —
    // under `console`'s test default (no TTY ⇒ SGR suppressed) both renderers drop `**` markers,
    // so bold is not distinguishable here, but the literal backtick is. If the two renderers ever
    // split again, this breaks.
    let code = render_assistant_rows("call `foo()` now", 40).join("\n");
    assert!(
        !code.contains('`'),
        "inline code backticks must be consumed, not shown literally: {code:?}"
    );
    assert!(
        code.contains("foo()"),
        "the code text itself must survive: {code:?}"
    );
}

/// Codepoints in the Dingbats block (U+2700..U+27BF) carrying `Emoji=Yes` in Unicode's
/// `emoji-data.txt`. They default to TEXT presentation, so `unicode-width` — which reads
/// East_Asian_Width, a different table — reports them as 1 cell and waves them through. But a
/// terminal that resolves them through its emoji font (Windows Terminal with Segoe UI Emoji in
/// the fallback chain does exactly this) paints them 2 cells wide. That is the failure mode a
/// width assertion cannot see, so the spinner alphabet is checked against this list instead.
const DINGBAT_EMOJI: &[char] = &[
    '\u{2702}', '\u{2705}', '\u{2708}', '\u{2709}', '\u{270A}', '\u{270B}', '\u{270C}', '\u{270D}',
    '\u{270F}', '\u{2712}', '\u{2714}', '\u{2716}', '\u{271D}', '\u{2721}', '\u{2728}', '\u{2733}',
    '\u{2734}', '\u{2744}', '\u{2747}', '\u{274C}', '\u{274E}', '\u{2753}', '\u{2754}', '\u{2755}',
    '\u{2757}', '\u{2763}', '\u{2764}', '\u{2795}', '\u{2796}', '\u{2797}', '\u{27A1}', '\u{27B0}',
    '\u{27BF}',
];

#[test]
fn bloom_frames_never_render_two_cells_wide() {
    // THE constraint on the spinner alphabet. The caption is drawn immediately right of the
    // glyph, so a frame that measures 2 cells shoves the whole line sideways once per cycle —
    // a visible horizontal twitch, not a subtle one.
    //
    // The obvious picks for a "star bloom" are exactly the trap: ✳ (U+2733) and ✴ (U+2734) sit
    // mid-sequence by shape, and BOTH are `Emoji=Yes`. Note the ordering of the two assertions
    // below — the width check ALONE is not enough and would pass on those two, because their
    // East_Asian_Width is Narrow. The deny-list is what actually keeps them out.
    for f in BLOOM {
        assert_eq!(
            f.chars().count(),
            1,
            "frame {f:?} must be a single char — a variation selector or ZWJ sequence flips \
             renderers into emoji presentation, and thus double-width"
        );
        let c = f.chars().next().unwrap();
        assert!(
            !DINGBAT_EMOJI.contains(&c),
            "frame {f:?} (U+{:04X}) is Emoji=Yes: a terminal resolving it through an emoji \
             font draws it 2 cells and the caption jumps sideways once per cycle",
            c as u32
        );
        assert_eq!(
            console::measure_text_width(f),
            1,
            "spinner frame {f:?} must be exactly one cell wide"
        );
    }
}

#[test]
fn bloom_opens_on_the_brand_mark_and_ping_pongs() {
    // Two things the sequence must do, both user-visible:
    //  1. `Working(false)` zeroes `frame`, so frame 0 is what every turn OPENS on — it has to be
    //     the ✦ brand mark, not an arbitrary mid-bloom star.
    //  2. The tail mirrors the head (ping-pong), so the cycle CLOSES back toward ✦ instead of
    //     snapping from the widest star straight to the mark.
    assert_eq!(BLOOM[0], "✦", "a turn must open on the brand mark");
    assert_eq!(
        BLOOM.len() % 2,
        0,
        "an odd length can't mirror cleanly around its peak"
    );
    let peak = BLOOM.len() / 2;
    for i in 1..peak {
        assert_eq!(
            BLOOM[peak - i],
            BLOOM[peak + i],
            "frame {} and {} must mirror for the bloom to close symmetrically",
            peak - i,
            peak + i
        );
    }
    // No braille anywhere: U+2800..=U+28FF is the generic-CLI spinner block, and wearing it
    // would trade the ✦ brand (shared with the `✦ ultimate` chip) for anonymity.
    for f in BLOOM {
        let c = f.chars().next().unwrap();
        assert!(
            !('\u{2800}'..='\u{28FF}').contains(&c),
            "frame {f:?} is braille — that's the look this spinner exists to avoid"
        );
    }
}

#[test]
fn working_line_advances_glyph_with_the_frame_counter() {
    // The spinner shares the typewriter's tick (no second clock), so a `Tick` must move BOTH.
    // Guards against a future "let's give the spinner its own timer" refactor silently pinning
    // the glyph to frame 0 while the caption keeps typing.
    let mut state = AppState::new("intro", "status");
    apply_command(&mut state, Command::Working(true));
    state.set_work_caption("Reading retained.rs".to_string());

    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..BLOOM.len() {
        let (plain, _) = working_line(&state).remove(0);
        let glyph = plain.chars().next().unwrap();
        seen.insert(glyph);
        apply_command(&mut state, Command::Tick);
    }
    // A full cycle shows every DISTINCT frame; the ping-pong reuses ✶/✷/✹, so 8 frames render 5.
    let distinct: std::collections::BTreeSet<char> =
        BLOOM.iter().map(|f| f.chars().next().unwrap()).collect();
    assert_eq!(
        seen, distinct,
        "one full cycle must render every distinct bloom frame"
    );
    assert!(seen.len() > 2, "a 2-glyph blink is what we replaced");
}

#[test]
fn working_line_draws_no_caret_of_its_own() {
    // The working line must NOT imitate a text cursor. The terminal's real cursor is already on
    // screen — ratatui's `apply_buffer_with_cursor` calls `show_cursor()` for any frame that sets
    // a position, so `draw_footer`'s `set_cursor_position` re-shows it every frame and the `Hide`
    // in `TerminalSession::enter` survives exactly one. A second, drawn caret reads as a stray
    // blinking cursor, and worst of all when the caption is empty between tool steps: it collapsed
    // onto the glyph as `✦ ▏`, looking like a cursor stuck to the spinner.
    //
    // Checked in three states, because the old code only drew the caret while `!done` — testing
    // just the settled state would have passed against the very bug this guards.
    let mut state = AppState::new("intro", "status");
    apply_command(&mut state, Command::Working(true));
    state.set_work_caption("Reading retained.rs".to_string());

    // 1. mid-typewriter
    apply_command(&mut state, Command::Tick);
    apply_command(&mut state, Command::Tick);
    let (typing, _) = working_line(&state).remove(0);
    assert!(
        !typing.contains('▏'),
        "no drawn caret while typing: {typing:?}"
    );

    // 2. fully revealed
    for _ in 0..state.work_caption.chars().count() {
        apply_command(&mut state, Command::Tick);
    }
    let (settled, _) = working_line(&state).remove(0);
    assert!(
        !settled.contains('▏'),
        "no drawn caret once settled: {settled:?}"
    );

    // 3. empty caption — the case that produced `✦ ▏`.
    state.work_caption.clear();
    state.work_reveal = 0;
    let (empty, _) = working_line(&state).remove(0);
    assert!(
        !empty.contains('▏'),
        "an empty caption must not collapse a caret onto the spinner: {empty:?}"
    );
    // The spinner and clock still render — this removed a caret, not the line. (Frame-agnostic:
    // by now the bloom has advanced well past frame 0.)
    let head = empty.chars().next().unwrap().to_string();
    assert!(
        BLOOM.contains(&head.as_str()),
        "a bloom frame still leads the line: {empty:?}"
    );
    assert!(empty.contains("Esc to stop"), "clock survives: {empty:?}");
}

#[test]
fn working_caption_types_out_then_holds() {
    // The typewriter: `Working(true)` seeds a whimsical verb and starts the reveal at zero, each
    // tick exposes one more char, and the reveal CLAMPS at the caption length (it must not run past
    // the end and start slicing nothing, nor keep incrementing forever).
    let mut state = AppState::new("intro", "status");
    apply_command(&mut state, Command::Working(true));
    assert!(!state.work_caption.is_empty(), "a turn seeds a verb");
    assert_eq!(state.work_reveal, 0, "reveal starts from scratch");

    let len = state.work_caption.chars().count();
    for _ in 0..len {
        apply_command(&mut state, Command::Tick);
    }
    assert_eq!(state.work_reveal, len, "one char per tick, fully revealed");
    // Extra ticks must not overshoot.
    for _ in 0..5 {
        apply_command(&mut state, Command::Tick);
    }
    assert_eq!(
        state.work_reveal, len,
        "reveal clamps at the caption length"
    );

    // The rendered line shows the whole caption plus the elapsed clock + stop hint.
    let (plain, _) = working_line(&state).remove(0);
    assert!(
        plain.contains(&state.work_caption),
        "the revealed caption must appear: {plain:?}"
    );
    assert!(
        plain.contains("Esc to stop"),
        "the stop hint rides the working line now: {plain:?}"
    );
}

#[test]
fn tool_caption_replaces_verb_then_falls_back() {
    // The hybrid caption: a tool call re-points it at a concrete action (restarting the reveal so
    // the new text types out), and an empty `WorkCaption` — what `emit_tool_result` sends when the
    // call ends — falls back to the whimsical verb rather than freezing on the finished action.
    let mut state = AppState::new("intro", "status");
    apply_command(&mut state, Command::Working(true));
    let verb = state.work_verb.clone();
    apply_command(&mut state, Command::Tick); // reveal 1 char of the verb

    apply_command(
        &mut state,
        Command::WorkCaption("Reading retained.rs".into()),
    );
    assert_eq!(state.work_caption, "Reading retained.rs");
    assert_eq!(state.work_reveal, 0, "a new caption retypes from scratch");

    // Re-asserting the SAME caption must not stutter the reveal back to zero.
    apply_command(&mut state, Command::Tick);
    apply_command(
        &mut state,
        Command::WorkCaption("Reading retained.rs".into()),
    );
    assert_eq!(state.work_reveal, 1, "same text ⇒ reveal is preserved");

    apply_command(&mut state, Command::WorkCaption(String::new()));
    assert_eq!(state.work_caption, verb, "empty falls back to the verb");
}

#[test]
fn working_line_only_rides_the_transcript_while_working() {
    // The working indicator moved OUT of the HUD and onto the transcript tail. It must appear only
    // while a turn is in flight — and `Working(false)` must clear the caption so no stale "Reading
    // …" row lingers under the finished answer.
    let mut state = AppState::new("intro", "status");
    apply_command(&mut state, Command::Working(true));
    apply_command(&mut state, Command::WorkCaption("Run cargo test".into()));
    assert!(state.working, "turn in flight");

    apply_command(&mut state, Command::Working(false));
    assert!(!state.working);
    assert!(
        state.work_caption.is_empty() && state.work_reveal == 0,
        "finishing a turn clears the caption, leaving no stale row"
    );
}

#[test]
fn ultimate_recolours_the_input_box_to_gold() {
    // `/ultimate` recolours the input box framing to the reserved gold so the heightened mode is
    // unmistakable, and toggling back returns it to moonlight. The state is PUSHED (never read from
    // disk in the draw path, which runs at ~9fps).
    let mut state = AppState::new("intro", "status");
    assert!(!state.ultimate, "moonlight by default");

    apply_command(&mut state, Command::Ultimate(true));
    assert!(state.ultimate);
    apply_command(&mut state, Command::Ultimate(false));
    assert!(!state.ultimate, "toggling off returns to moonlight");
    // Guard the palette choice itself: gold must stay distinct from the default silver, or the
    // recolour would be invisible.
    assert_ne!(crate::ui::theme::WARN, crate::ui::theme::ACCENT_DIM);
}

#[test]
fn pruning_keeps_whole_blocks() {
    let mut state = AppState::new("intro", "status");
    for i in 0..BLOCK_LIMIT + 20 {
        state.push_text(BlockKind::Generic, format!("line-{i}"), true);
    }
    assert_eq!(state.blocks.len(), BLOCK_LIMIT);
    assert!(state
        .blocks
        .iter()
        .all(|b| !matches!(&b.payload, Payload::Text(s) if s.is_empty())));
}

#[test]
fn scroll_routes_to_open_overlay_not_the_transcript() {
    let mut state = AppState::new("intro", "status");
    // No overlay → scroll moves the transcript.
    apply_command(&mut state, Command::Scroll(-3));
    assert_eq!(state.scroll_from_tail, 3);
    assert_eq!(state.overlay_scroll, 0);

    // Open an overlay → scroll now moves ITS content, leaving the transcript offset untouched.
    apply_command(
        &mut state,
        Command::OpenOverlay(OverlaySnapshot {
            title: "info".into(),
            lines: (0..40).map(|i| format!("row {i}")).collect(),
            selected: None,
            hint: String::new(),
        }),
    );
    assert_eq!(state.overlay_scroll, 0, "a fresh overlay starts at the top");
    apply_command(&mut state, Command::Scroll(-5));
    assert_eq!(state.overlay_scroll, 5, "overlay scrolled");
    assert_eq!(
        state.scroll_from_tail, 3,
        "transcript offset is left alone while the overlay is up"
    );

    // Home/End resets the overlay while it's open.
    apply_command(&mut state, Command::ScrollEnd);
    assert_eq!(state.overlay_scroll, 0);
    assert_eq!(state.scroll_from_tail, 3);

    // Closing the overlay hands scroll back to the transcript.
    apply_command(&mut state, Command::CloseOverlay);
    apply_command(&mut state, Command::Scroll(-2));
    assert_eq!(state.scroll_from_tail, 5);
}

/// `/workflows` republishes itself about once a second so its elapsed times tick. If a refresh
/// re-opened the overlay it would yank the reader back to the top every second — unusable while
/// paging through the history section. `UpdateOverlay` swaps the body and leaves scroll alone.
#[test]
fn a_live_overlay_refresh_keeps_the_readers_scroll_position() {
    let mut state = AppState::new("intro", "status");
    apply_command(
        &mut state,
        Command::OpenOverlay(OverlaySnapshot {
            title: "Activity".into(),
            lines: (0..40).map(|i| format!("row {i} · 3s")).collect(),
            selected: None,
            hint: "Esc/q close".into(),
        }),
    );
    apply_command(&mut state, Command::Scroll(-12));
    assert_eq!(state.overlay_scroll, 12);

    apply_command(
        &mut state,
        Command::UpdateOverlay((0..40).map(|i| format!("row {i} · 4s")).collect()),
    );

    assert_eq!(state.overlay_scroll, 12, "the reader's position survives");
    let overlay = state.input.overlay.as_ref().expect("still open");
    assert!(overlay.lines[0].ends_with("4s"), "body was refreshed");
    assert_eq!(overlay.title, "Activity", "title/hint are not disturbed");
    assert_eq!(overlay.hint, "Esc/q close");
}

/// A refresh that races the Esc which closed the panel must not resurrect it: the refresher thread
/// sleeps between publishes, so one in-flight update after the close is entirely ordinary.
#[test]
fn a_refresh_arriving_after_close_does_not_reopen_the_overlay() {
    let mut state = AppState::new("intro", "status");
    apply_command(
        &mut state,
        Command::OpenOverlay(OverlaySnapshot {
            title: "Activity".into(),
            lines: vec!["row".into()],
            selected: None,
            hint: String::new(),
        }),
    );
    apply_command(&mut state, Command::CloseOverlay);
    apply_command(&mut state, Command::UpdateOverlay(vec!["late".into()]));
    assert!(state.input.overlay.is_none(), "stays closed");
}

/// The per-tool result line carries a sub-agent dispatch's whole runtime, and that can be hours.
/// `· 7203.4s` is correct and unreadable; the minute/hour tiers exist for exactly that row.
#[test]
fn tool_row_time_gains_minute_and_hour_tiers() {
    assert_eq!(fmt_elapsed(None), "");
    assert_eq!(fmt_elapsed(Some(940)), " · 940ms");
    assert_eq!(fmt_elapsed(Some(1_250)), " · 1.2s");
    assert_eq!(fmt_elapsed(Some(59_900)), " · 59.9s");
    assert_eq!(fmt_elapsed(Some(60_000)), " · 1m00s");
    assert_eq!(fmt_elapsed(Some(7_203_400)), " · 2h00m");
}

/// The highlight is the ONLY selection surface now — there is no floating Copy button to keep in
/// sync with it, because Ctrl-C copies whatever is highlighted. So the two state transitions that
/// remain have to be exact: setting a selection paints it, clearing it takes it away completely.
#[test]
fn selection_state_tracks_set_and_clear() {
    let mut state = AppState::new("intro", "status");
    let sel = SelectionRange {
        anchor_line: 1,
        anchor_col: 0,
        cursor_line: 1,
        cursor_col: 4,
    };
    apply_command(&mut state, Command::SetSelection(sel));
    assert_eq!(state.selection, Some(sel));

    apply_command(&mut state, Command::ClearSelection);
    assert!(state.selection.is_none());
}

#[test]
fn transcript_scroll_follows_tail_anchors_and_clamps() {
    // At the bottom (offset 0): stay pinned to the tail as content streams in.
    let (start, offset, last) = resolve_transcript_scroll(0, 100, 120, 10);
    assert_eq!(offset, 0, "offset stays 0 at the bottom");
    assert_eq!(start, 110, "view follows the new tail");
    assert_eq!(last, 120);

    // Scrolled up (offset 5) while 20 lines are appended: the offset grows by the delta so the
    // content under the reader's eyes does not move.
    let (start, offset, _) = resolve_transcript_scroll(5, 100, 120, 10);
    assert_eq!(offset, 25, "offset bumped by the 20 appended lines");
    // tail_start = 120 - 10 = 110; start = 110 - 25 = 85 (same absolute rows as 105 - 5 before).
    assert_eq!(start, 85);

    // PageUp past the top must not inflate the offset: it clamps to tail_start and writes back, so
    // a single PageDown moves the view immediately (no dead zone).
    let (start, offset, _) = resolve_transcript_scroll(9999, 50, 50, 10);
    assert_eq!(
        offset, 40,
        "clamped to tail_start (50 - 10), not left at 9999"
    );
    assert_eq!(start, 0, "pinned at the very top");

    // Fewer lines than the viewport: nothing to scroll, offset collapses to 0.
    let (start, offset, _) = resolve_transcript_scroll(3, 4, 4, 10);
    assert_eq!(offset, 0);
    assert_eq!(start, 0);
}

// ── the mockup redesign: structured transcript blocks ────────────────────
fn plain(s: &str) -> String {
    console::strip_ansi_codes(s).into_owned()
}

#[test]
fn tool_row_puts_digest_and_time_on_the_line_below() {
    // The call `⚙ file_read   src/auth.rs` sits on top; the result digest drops to an indented
    // `└ 142 lines · <time>` line beneath it (not right-aligned on the same row).
    let ev = ToolEvent {
        seq: 1,
        icon: "⚙".into(),
        name: "file_read".into(),
        target: "src/auth.rs".into(),
        digest: "142 lines".into(),
        state: ToolState::Ok,
        elapsed_ms: Some(1200),
    };
    let row = plain(&render_tool_row(&ev, 60));
    let lines: Vec<&str> = row.split('\n').collect();
    assert_eq!(lines.len(), 2, "call line + result line: {row:?}");
    assert_eq!(
        lines[0], "⚙ file_read   src/auth.rs",
        "call on top: {row:?}"
    );
    assert_eq!(
        lines[1], "└ 142 lines · 1.2s",
        "digest + time below: {row:?}"
    );
}

#[test]
fn tool_row_shows_ms_for_subsecond_runs() {
    let ev = ToolEvent {
        seq: 1,
        icon: "⚙".into(),
        name: "search_files".into(),
        target: "foo".into(),
        digest: "3 match(es)".into(),
        state: ToolState::Ok,
        elapsed_ms: Some(940),
    };
    let row = plain(&render_tool_row(&ev, 60));
    assert!(
        row.ends_with("└ 3 match(es) · 940ms"),
        "sub-second → ms: {row:?}"
    );
}

#[test]
fn tool_row_omits_time_when_unknown() {
    // A restored / eager-adopted call has no timing → the result line carries the digest only.
    let ev = ToolEvent {
        seq: 1,
        icon: "⚙".into(),
        name: "file_read".into(),
        target: "x.rs".into(),
        digest: "10 lines".into(),
        state: ToolState::Ok,
        elapsed_ms: None,
    };
    let row = plain(&render_tool_row(&ev, 60));
    assert!(row.ends_with("└ 10 lines"), "no time appended: {row:?}");
    assert!(!row.contains('·'), "no time separator: {row:?}");
}

#[test]
fn tool_row_omits_result_line_when_no_digest() {
    // A still-running call (empty digest) is just the call line, no `└` result line yet.
    let ev = ToolEvent {
        seq: 1,
        icon: "⚙".into(),
        name: "shell_run".into(),
        target: "cargo check".into(),
        digest: String::new(),
        state: ToolState::Running,
        elapsed_ms: None,
    };
    let row = plain(&render_tool_row(&ev, 60));
    assert_eq!(row, "⚙ shell_run   cargo check");
    assert!(!row.contains('\n'), "no result line while running: {row:?}");
}

#[test]
fn plan_box_frames_and_counts() {
    let rows = vec![
        PlanRow {
            status: 2,
            text: "design".into(),
        },
        PlanRow {
            status: 1,
            text: "implement".into(),
        },
        PlanRow {
            status: 0,
            text: "verify".into(),
        },
    ];
    let out: Vec<String> = render_plan_box(&rows, 40)
        .iter()
        .map(|s| plain(s))
        .collect();
    assert!(
        out[0].contains("☑ 1/3 · plan"),
        "header counts done/total: {:?}",
        out[0]
    );
    assert!(
        out.iter().any(|l| l.contains("✓ design")),
        "done row: {out:?}"
    );
    assert!(
        out.iter().any(|l| l.contains("▸ implement")),
        "in-progress row: {out:?}"
    );
    assert!(
        out.iter().any(|l| l.contains("○ verify")),
        "pending row: {out:?}"
    );
    // Top border, three rows, bottom border.
    assert_eq!(out.len(), 5);
    // Every framed line is the same display width (a true rectangle).
    let w0 = console::measure_text_width(&out[0]);
    assert!(
        out.iter().all(|l| console::measure_text_width(l) == w0),
        "uniform width: {out:?}"
    );
}

#[test]
fn plan_update_is_in_place_not_appended() {
    // A second todo_write REPLACES the panel rather than stacking a fresh box (only one Plan block
    // ever exists), and it keeps its original position among other blocks.
    let mut state = AppState::new("intro", "status");
    state.push_text(BlockKind::Generic, "before".into(), true);
    apply_command(
        &mut state,
        Command::Plan(vec![PlanRow {
            status: 0,
            text: "a".into(),
        }]),
    );
    state.push_text(BlockKind::Generic, "after".into(), true);
    let plan_pos = state
        .blocks
        .iter()
        .position(|b| b.kind == BlockKind::Plan)
        .unwrap();
    apply_command(
        &mut state,
        Command::Plan(vec![
            PlanRow {
                status: 2,
                text: "a".into(),
            },
            PlanRow {
                status: 1,
                text: "b".into(),
            },
        ]),
    );
    assert_eq!(
        state
            .blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Plan)
            .count(),
        1,
        "exactly one plan block"
    );
    assert_eq!(
        state.blocks.iter().position(|b| b.kind == BlockKind::Plan),
        Some(plan_pos),
        "the panel stays where it first appeared"
    );
    // An empty list removes the panel entirely.
    apply_command(&mut state, Command::Plan(vec![]));
    assert!(
        state.blocks.iter().all(|b| b.kind != BlockKind::Plan),
        "cleared plan leaves no box"
    );
}

#[test]
fn tool_event_updates_the_same_line_by_seq() {
    // A Running event opens a line; the Ok event with the same seq updates it in place (no second
    // Tool block appended), and flips it to complete.
    let mut state = AppState::new("intro", "status");
    apply_command(
        &mut state,
        Command::Tool(ToolEvent {
            seq: 7,
            icon: "⚙".into(),
            name: "file_read".into(),
            target: "x.rs".into(),
            digest: String::new(),
            state: ToolState::Running,
            elapsed_ms: None,
        }),
    );
    assert_eq!(
        state
            .blocks
            .iter()
            .filter(|b| b.kind == BlockKind::Tool)
            .count(),
        1
    );
    apply_command(
        &mut state,
        Command::Tool(ToolEvent {
            seq: 7,
            icon: "⚙".into(),
            name: "file_read".into(),
            target: "x.rs".into(),
            digest: "10 lines".into(),
            state: ToolState::Ok,
            elapsed_ms: Some(42),
        }),
    );
    let tools: Vec<&UiBlock> = state
        .blocks
        .iter()
        .filter(|b| b.kind == BlockKind::Tool)
        .collect();
    assert_eq!(tools.len(), 1, "same line updated in place, not appended");
    assert!(tools[0].complete, "result flips it complete");
    match &tools[0].payload {
        Payload::Tool(t) => assert_eq!(t.digest, "10 lines"),
        _ => panic!("expected a Tool payload"),
    }
}

#[test]
fn diff_box_frames_add_and_del() {
    let d = DiffPayload {
        path: "src/auth.rs".into(),
        adds: 2,
        dels: 1,
        lines: vec![(true, "let x = 1;".into()), (false, "let y = 2;".into())],
    };
    let out: Vec<String> = render_diff_box(&d, 48).iter().map(|s| plain(s)).collect();
    assert!(
        out[0].contains("diff · src/auth.rs"),
        "header names the path: {:?}",
        out[0]
    );
    assert!(
        out[0].contains("+2 −1"),
        "header carries the counts: {:?}",
        out[0]
    );
    assert!(
        out.iter().any(|l| l.contains("+ let x = 1;")),
        "added line: {out:?}"
    );
    assert!(
        out.iter().any(|l| l.contains("− let y = 2;")),
        "removed line: {out:?}"
    );
}

#[test]
fn verify_line_reads_green_success() {
    let v = VerifyPayload {
        cmd: "cargo check".into(),
        detail: "0 errors · verify gate passed".into(),
    };
    let row = plain(&render_verify_line(&v, 80));
    assert_eq!(row, "✓ cargo check — 0 errors · verify gate passed");
}

/// Paint one whole frame into an in-memory terminal and read the rows back as plain text. Wraps the
/// only end-to-end check there is that the footer's height, its rules and its text rows agree — the
/// layout arithmetic being right is no use if the widgets are handed the wrong rects.
fn painted_rows(state: &mut AppState, w: u16, h: u16) -> Vec<String> {
    let mut term = Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
    term.draw(|f| draw(f, state)).unwrap();
    let buf = term.backend().buffer().clone();
    (0..h)
        .map(|y| {
            let row: String = (0..w).map(|x| buf[(x, y)].symbol()).collect();
            row.trim_end().to_string()
        })
        .collect()
}

#[test]
fn a_long_prompt_is_painted_down_the_screen_not_off_the_side() {
    // THE BUG this box exists to fix: a prompt longer than one row scrolled sideways, so most of
    // what you typed was hidden and there was no way to read it back — or select it to copy.
    let mut state = AppState::new("intro", "status");
    let prompt = "hay giup toi viet mot doan van rat dai de kiem tra xem hop nhap co xuong dong";
    state.input.draft = prompt.chars().collect();
    state.input.cursor = state.input.draft.len();
    let rows = painted_rows(&mut state, 40, 12);
    assert_eq!(rows[8], "❯ hay giup toi viet mot doan van rat");
    assert_eq!(rows[9], "  dai de kiem tra xem hop nhap co");
    assert_eq!(rows[10], "  xuong dong");
    assert_eq!(rows[7], "─".repeat(40), "top rule sits above the text rows");
    assert_eq!(rows[11], "─".repeat(40), "bottom rule closes the box");
    // Nothing is dropped: the painted rows re-join into the prompt exactly.
    let joined = rows[8..=10]
        .iter()
        .map(|r| r.trim_start_matches("❯ ").trim_start())
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(joined, prompt);
}

#[test]
fn a_draft_taller_than_the_box_stops_growing_and_says_what_is_hidden() {
    // The box grows, but not without limit: the transcript keeps its floor, and the rule reports the
    // rows behind it so a capped composer never reads as a truncated one.
    let mut state = AppState::new("intro", "status");
    let many: String = (0..14).map(|i| format!("dong so {i}\n")).collect();
    state.input.draft = many.chars().collect();
    state.input.cursor = 30;
    let rows = painted_rows(&mut state, 40, 16);
    assert_eq!(rows[5], "❯ dong so 0");
    assert_eq!(rows[14], "  dong so 9", "10 text rows is the ceiling");
    assert!(
        rows[15].ends_with("↓5 ──"),
        "the bottom rule counts the hidden rows: {:?}",
        rows[15]
    );
    assert_eq!(rows[0], "intro", "the transcript keeps its floor");
}
