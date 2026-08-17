//! The plain-mode chat box: a bordered single-line editor read key-by-key through `console`.
//!
//! Used by the non-TTY / `AIZEN_NO_STICKY=1` REPL fallback. The retained TUI has its own, much
//! richer input surface (`ui::tui`) — the two are deliberately separate: this one must work with
//! nothing but raw mode and a cursor, on a terminal that may not support mouse capture at all.

use crate::ui::{image_input, splash};
use anyhow::Result;
use console::style;

/// A bordered single-line input box (the "chat box") read key-by-key via `console` (raw mode), so
/// the box redraws as you type and the cursor sits inside it. A small line editor:
/// - type / **Backspace** / **Del** insert+delete at the cursor; **←/→** move; **Home/End** jump;
/// - **↑/↓** walk `history` (most-recent first; ↓ past the newest restores your in-progress draft);
/// - **Enter** submits; **Esc** clears the line AND any attached images (quits only when both are
///   already empty); **Ctrl-C/Ctrl-D** quit.
/// - **Attach an image** (vision) two ways (Ctrl-V can't be used — Windows Terminal eats it):
///   **Ctrl-O** grabs a copied screenshot from the clipboard (Win+Shift+S), or **drag an image file
///   onto the window** (the terminal pastes its path; the caller turns image-file paths on the line
///   into attachments on Enter). An `[N img]` tag shows in the top border; **Ctrl-X** removes the
///   most recent attachment (keeps your text).
///
/// Returns `Some((line, images))` on Enter (`images` = `data:` URLs of clipboard attachments; the
/// caller adds any file-path attachments), or `None` to quit (Esc-empty / Ctrl-C/D / EOF / non-TTY).
/// The visible window scrolls horizontally so the cursor stays in view on long lines.
pub(crate) fn read_input_box(history: &[String]) -> Result<Option<(String, Vec<String>)>> {
    use console::{Key, Term};
    use std::io::Write;
    const W: usize = 66; // inner width between the │ borders
    let text_cols = W - 3; // columns for editable text (after " ❯ ")

    let term = Term::stdout();
    let accent = splash::ACCENT;
    let bar = |l: &str, r: &str| {
        style(format!("{l}{}{r}", "─".repeat(W)))
            .color256(accent)
            .to_string()
    };
    // A small status tag in the TOP border (`╭───────[1 img]─╮`). ASCII-only + right-aligned, so the
    // width is exact and the border never tears (an emoji caption mis-measures by a cell). Empty tag
    // → a plain border.
    let top_bar = |tag: &str| -> String {
        if tag.is_empty() {
            return bar("╭", "╮");
        }
        let t = format!("[{tag}]");
        let fill = W.saturating_sub(t.chars().count() + 1);
        style(format!("╭{}{t}─╮", "─".repeat(fill)))
            .color256(accent)
            .to_string()
    };
    // Attachment count → tag text (empty when none, so the border goes plain).
    let count_tag = |n: usize| -> String {
        if n == 0 {
            String::new()
        } else {
            format!("{n} img")
        }
    };

    // Render the middle line for (chars, cursor), scrolling so the cursor is visible. Returns the
    // line + how far left to shift the cursor from the line end to land on `cursor`. (Char widths
    // are treated as 1 — fine for ASCII/Latin/Vietnamese; exotic wide input may wobble by a cell.)
    let render = |chars: &[char], cursor: usize, scroll: &mut usize| -> (String, usize) {
        if cursor < *scroll {
            *scroll = cursor;
        }
        if cursor >= *scroll + text_cols {
            *scroll = cursor + 1 - text_cols;
        }
        let end = (*scroll + text_cols).min(chars.len());
        let shown: String = chars[*scroll..end].iter().collect();
        let shown_w = end - *scroll;
        let pad = text_cols - shown_w;
        let line = format!(
            "{l} {arrow} {shown}{sp}{l}",
            l = style("│").color256(accent),
            arrow = style("❯").color256(accent).bold(),
            sp = " ".repeat(pad)
        );
        let cursor_col = cursor - *scroll;
        let back = (shown_w - cursor_col) + pad + 1; // chars after cursor + pad + right border
        (line, back)
    };

    let mut scroll = 0usize;
    let (mid0, back0) = render(&[], 0, &mut scroll);
    println!("{}", top_bar(""));
    println!("{mid0}");
    print!("{}", bar("╰", "╯"));
    std::io::stdout().flush().ok();
    term.move_cursor_up(1).ok();

    let place = |line: &str, back: usize| {
        let _ = term.clear_line();
        let mut o = std::io::stdout();
        let _ = write!(o, "\r{line}");
        let _ = o.flush();
        let _ = term.move_cursor_left(back);
    };
    place(&mid0, back0);

    // Repaint the TOP border (cursor sits on the middle line) — used by the image attach/remove keys
    // to reflect the count tag, then return to the middle line (the loop's `place` restores the
    // cursor column).
    let redraw_top = |s: &str| {
        let _ = term.move_cursor_up(1);
        let _ = term.clear_line();
        let mut o = std::io::stdout();
        let _ = write!(o, "\r{s}");
        let _ = o.flush();
        let _ = term.move_cursor_down(1);
    };

    let mut chars: Vec<char> = Vec::new();
    let mut cursor = 0usize;
    let mut hist_idx: Option<usize> = None; // Some = currently browsing history
    let mut draft: Vec<char> = Vec::new(); // the in-progress line saved when entering history
    let mut images: Vec<String> = Vec::new(); // pasted vision attachments (data: URLs)

    loop {
        let key = match term.read_key() {
            Ok(k) => k,
            Err(_) => return Ok(None),
        };
        match key {
            Key::Enter => {
                let text: String = chars.iter().collect();
                // Collapse the 3-line box into a single compact `> …` echo (nothing when empty), so
                // the scrollback reads as a clean transcript instead of a stack of empty boxes — AND
                // so the box's presence is the unambiguous "your turn to type" signal (no box +
                // spinner/⊙ traces = the agent is working).
                term.move_cursor_down(1).ok(); // → bottom border
                term.clear_line().ok();
                term.move_cursor_up(1).ok(); // → middle line
                term.clear_line().ok();
                term.move_cursor_up(1).ok(); // → top border
                term.clear_line().ok();
                print!("\r");
                let trimmed = text.trim();
                if !trimmed.is_empty() {
                    println!(
                        "{} {}",
                        style("❯").color256(accent).bold(),
                        style(trimmed).dim()
                    );
                } else if !images.is_empty() {
                    println!(
                        "{} {}",
                        style("❯").color256(accent).bold(),
                        style(format!("📎 {} image(s)", images.len())).dim()
                    );
                }
                std::io::stdout().flush().ok();
                return Ok(Some((text, images)));
            }
            Key::Char('\u{f}') => {
                // Ctrl-O: grab a copied screenshot from the clipboard (Win+Shift+S / "Copy image").
                // Explicit, so it works in Windows Terminal (which eats Ctrl-V but forwards Ctrl-O).
                let tag = match image_input::clipboard_image_data_url() {
                    Ok(Some(url)) => {
                        images.push(url);
                        count_tag(images.len())
                    }
                    Ok(None) => "no image".to_string(),
                    Err(_) => "clip error".to_string(),
                };
                redraw_top(&top_bar(&tag));
            }
            Key::Char('\u{18}') => {
                // Ctrl-X: remove the most recently attached image (keeps your typed text). The tag
                // reflects the new count (gone when the last one is removed); no-op when none.
                if images.pop().is_some() {
                    redraw_top(&top_bar(&count_tag(images.len())));
                }
            }
            Key::Escape => {
                // Nothing typed AND nothing attached → quit. Otherwise clear the line AND drop any
                // attached images (a quick way to start over / undo a wrong attachment).
                if chars.is_empty() && images.is_empty() {
                    term.move_cursor_down(1).ok();
                    println!();
                    return Ok(None);
                }
                chars.clear();
                cursor = 0;
                hist_idx = None;
                if !images.is_empty() {
                    images.clear();
                    redraw_top(&top_bar(""));
                }
            }
            Key::Char('\u{3}') | Key::Char('\u{4}') => {
                term.move_cursor_down(1).ok();
                println!();
                return Ok(None);
            }
            Key::Char(c) if c.is_control() => continue, // ignore stray control chars (no redraw)
            Key::Char(c) => {
                chars.insert(cursor, c);
                cursor += 1;
            }
            Key::Backspace => {
                if cursor > 0 {
                    chars.remove(cursor - 1);
                    cursor -= 1;
                }
            }
            Key::Del => {
                if cursor < chars.len() {
                    chars.remove(cursor);
                }
            }
            Key::ArrowLeft => cursor = cursor.saturating_sub(1),
            Key::ArrowRight => {
                if cursor < chars.len() {
                    cursor += 1;
                }
            }
            Key::Home => cursor = 0,
            Key::End => cursor = chars.len(),
            Key::ArrowUp => {
                if history.is_empty() {
                    continue;
                }
                let next = match hist_idx {
                    None => {
                        draft = chars.clone(); // save the in-progress line
                        history.len() - 1
                    }
                    Some(0) => continue, // already at the oldest
                    Some(i) => i - 1,
                };
                hist_idx = Some(next);
                chars = history[next].chars().collect();
                cursor = chars.len();
            }
            Key::ArrowDown => match hist_idx {
                None => continue,
                Some(i) if i + 1 < history.len() => {
                    hist_idx = Some(i + 1);
                    chars = history[i + 1].chars().collect();
                    cursor = chars.len();
                }
                Some(_) => {
                    hist_idx = None; // past the newest → restore the draft
                    chars = draft.clone();
                    cursor = chars.len();
                }
            },
            _ => continue, // unhandled key → no redraw
        }
        let (m, b) = render(&chars, cursor, &mut scroll);
        place(&m, b);
    }
}
