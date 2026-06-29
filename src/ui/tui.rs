//! Sticky-footer interactive TUI for the bare-`ng` REPL: a chat input box **pinned to the bottom**
//! of the terminal that stays visible even while the agent is working, with three properties the
//! plain line-REPL can't give:
//!
//! 1. **Pinned borderless prompt** — an ANSI scroll region (`ESC[{top};{bot}r`) reserves the bottom
//!    rows for a borderless footer (a faint rule · the HUD · the moonlit `❯` prompt); all agent output
//!    scrolls in the region *above* it, so the prompt never scrolls away and never stacks up.
//! 2. **Continuous chat** — a background thread owns the keyboard and pushes each submitted line onto
//!    an unbounded queue. You can keep typing (and queue messages) while the agent runs; the REPL
//!    drains the queue and auto-fires the next one when the current turn finishes.
//! 3. **Cancel** — Esc / Ctrl-C while the agent is working sends a cancel signal; the REPL drops the
//!    in-flight turn (aborting the streaming HTTP request) and returns you to the prompt.
//!
//! Output coordination: a single render `Mutex` serialises every terminal write. The agent's
//! streaming output and tool traces go through [`emit`]/[`emit_line`] (which restore the saved output
//! cursor, print, re-save, then repaint the box); the input thread repaints the box on each keypress.
//! When the TUI isn't active (the one-shot `ng chat`/`agent` subcommands, pipes, CI) every entry
//! point degrades to a plain `print!` so nothing changes for non-interactive use.

use crate::ui::splash::ACCENT;
use crate::ui::theme;
use console::{measure_text_width, style, Key, Term};
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::mpsc as stdmpsc;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// Footer height in rows: faint rule + HUD status line + blank breather + the `❯` prompt line.
const FOOTER: u16 = 4;

/// Max rows the live slash palette draws above the input box.
const PALETTE_MAX: usize = 7;

/// A key whose `read_key()` returned within this many ms was ALREADY waiting in the OS input buffer
/// → it arrived as part of a burst (a paste), not a deliberate human keystroke. Used so a newline
/// *inside* a paste becomes a literal newline in the draft instead of submitting the line — the fix
/// for a multi-line paste firing one message per line. Comfortably above buffered-read scheduling
/// jitter (a few ms) yet far below the gap before a human reaches the Enter key (≥ ~100 ms).
const PASTE_COALESCE_MS: u64 = 50;

/// Slash commands offered in the live palette (primary name + one-line gist). Mirrors the `match`
/// in `handle_slash` (main.rs) — keep in sync. Order ≈ most-reached first.
pub const SLASH: &[(&str, &str)] = &[
    ("help", "commands & tips"),
    ("model", "list + pick the model"),
    ("sessions", "saved chats — restore / save / delete"),
    ("timeline", "time machine — rewind / re-apply code"),
    ("checkpoint", "save a code restore point"),
    ("compact", "compress context to free tokens"),
    ("memory", "your memory profile / search"),
    ("persona", "switch persona voice"),
    ("skills", "browse & toggle skills"),
    ("apps", "integrations"),
    ("mcp", "MCP servers summary"),
    ("commands", "your custom commands"),
    ("telegram", "telegram bot setup"),
    ("serve", "run the daemon"),
    ("config", "endpoint / key setup"),
    ("yolo", "toggle auto-approve"),
    ("smart", "toggle smart approval"),
    ("cost", "session token cost"),
    ("tokens", "context token status"),
    ("clear", "new conversation"),
    ("quit", "exit aizen"),
];

/// Rows the palette painted last time — so a shrinking/closing palette clears its stale lines.
static LAST_PAL: AtomicU16 = AtomicU16::new(0);

/// Slash commands matching the current draft. Empty unless the draft is a bare `/<prefix>` with no
/// space yet (once you type an argument the palette gets out of the way).
fn slash_matches(draft: &[char]) -> Vec<&'static (&'static str, &'static str)> {
    if draft.first() != Some(&'/') {
        return Vec::new();
    }
    let rest: String = draft[1..].iter().collect();
    if rest.chars().any(|c| c.is_whitespace()) {
        return Vec::new(); // argument phase → hide the palette
    }
    let typed = rest.to_lowercase();
    SLASH.iter().filter(|(n, _)| n.starts_with(&typed)).collect()
}

/// Whether the sticky TUI currently owns the terminal (gates `emit`'s behaviour + spinner suppression).
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether the agent is mid-turn. Set by the REPL around a turn; read by the input thread (Esc =
/// cancel when working, quit when idle) AND by `paint_box` (the box's working indicator).
static WORKING: AtomicBool = AtomicBool::new(false);

/// Cooperative cancel flag, set by the input thread on Esc/Ctrl-C while the agent is working.
///
/// The REPL races the turn future against `cancel.recv()` in a `select!`, but `select!` can only
/// observe the cancel when the future YIELDS (returns Pending). While a synchronous tool runs — a
/// `shell_run` busy-wait of up to 120s, a parallel read batch — the future never yields, so Esc was
/// silently ignored until the tool finished. This flag bridges that gap: the input thread sets it
/// immediately, the synchronous tool path (the shell poll loop) polls it and aborts the child, and
/// the agent loop yields once it's set so the `select!` can finally drop the turn.
static CANCEL_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Request cancellation of the in-flight turn (called by the input thread on Esc while working).
pub fn request_cancel() {
    CANCEL_REQUESTED.store(true, Ordering::Relaxed);
}
/// Whether a cancel has been requested — polled by the synchronous tool path + the agent loop.
pub fn cancel_requested() -> bool {
    CANCEL_REQUESTED.load(Ordering::Relaxed)
}
/// Clear the cancel flag (called by the REPL at each turn boundary so a stale Esc can't kill the next).
pub fn clear_cancel() {
    CANCEL_REQUESTED.store(false, Ordering::Relaxed);
}

/// Braille frames for the animated working indicator (a lone background thread advances this while
/// `WORKING`, so the spinner spins even when no token is streaming — e.g. before the first byte or
/// during a long tool call). Moonlight silver, drawn by [`paint_box`].
const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
/// Current spinner frame index (advanced by the ticker thread; read by `paint_box`).
static WORK_FRAME: AtomicUsize = AtomicUsize::new(0);
/// When the current task started — drives the "· Ns" elapsed counter in the working pill. Set on
/// `set_working(true)`, cleared on `set_working(false)`.
static WORK_START: OnceLock<Mutex<Option<Instant>>> = OnceLock::new();
/// Guards the single ticker thread so it's spawned at most once per process.
static TICKER_STARTED: AtomicBool = AtomicBool::new(false);

fn work_start_slot() -> &'static Mutex<Option<Instant>> {
    WORK_START.get_or_init(|| Mutex::new(None))
}

/// Seconds since the current task began (0 when idle / not yet started).
fn work_elapsed_secs() -> u64 {
    work_start_slot().lock().unwrap().map(|t| t.elapsed().as_secs()).unwrap_or(0)
}

/// Spawn the lone animation ticker (idempotent). While the agent is working it bumps the spinner
/// frame and repaints the box ~9×/s, so the indicator animates and the elapsed counter ticks even
/// when no output is streaming. Idle (not working) → it just sleeps; on a pipe/CI it never spawns.
fn start_ticker() {
    if !std::io::stdout().is_terminal() {
        return; // no animation on a pipe / CI
    }
    if TICKER_STARTED.swap(true, Ordering::SeqCst) {
        return; // already running
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(Duration::from_millis(110));
        if !ACTIVE.load(Ordering::Relaxed) || !WORKING.load(Ordering::Relaxed) {
            continue;
        }
        WORK_FRAME.fetch_add(1, Ordering::Relaxed);
        // Repaint the box only. paint_box uses absolute cursor moves and never touches the `\x1b7`
        // output-save slot (owned by `emit`), so animating from this thread cannot disturb where the
        // next streamed token lands. Serialized with emit/keystrokes on the render lock.
        let r = render().lock().unwrap();
        let mut buf = String::new();
        paint_box(&mut buf, &r);
        flush(&buf);
    });
}

/// What the user submitted from the input box.
#[derive(Debug, Clone, PartialEq)]
pub enum Submission {
    /// A normal chat/agent message (text + pasted image data URLs).
    Chat(String, Vec<String>),
    /// A slash command line (without the leading `/`). The input thread parks itself after sending
    /// this so the REPL can hand stdin to a `dialoguer` menu, then unparks it via the resume channel.
    Slash(String),
    /// Esc/Ctrl-C/Ctrl-D while idle with an empty draft → leave the REPL.
    Quit,
}

/// Live render state behind the global lock. `draft`/`cursor`/`images` mirror the input thread's
/// edit buffer so any repaint (keystroke OR agent output) draws a consistent box.
struct Render {
    cols: u16,
    rows: u16,
    draft: Vec<char>,
    cursor: usize,
    images: usize,
    status: String,
    /// Highlighted row in the live slash palette (index into the current matches; 0 = nearest box).
    palette_sel: usize,
}

fn render() -> &'static Mutex<Render> {
    static R: OnceLock<Mutex<Render>> = OnceLock::new();
    R.get_or_init(|| {
        Mutex::new(Render {
            cols: 80,
            rows: 24,
            draft: Vec::new(),
            cursor: 0,
            images: 0,
            status: String::new(),
            palette_sel: 0,
        })
    })
}

pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

// ── in-TUI per-action approval bridge ─────────────────────────────────────────
// The flagship sticky TUI used to be binary: deny everything, or `/yolo` to allow everything. This
// bridge adds a real per-action prompt — the agent loop blocks in `ask_approval`, the keyboard thread
// (which owns stdin) routes the next y/n/a key to it. `[a]` = allow every destructive op for the rest
// of the session (a softer, session-scoped `/yolo`).

/// Set while the agent loop is blocked awaiting a y/n/a decision; the input thread then routes the
/// next decision key to `approval_slot` instead of editing the draft.
static APPROVAL_PENDING: AtomicBool = AtomicBool::new(false);
/// "Allow all destructive ops this session" (the `[a]` choice) — short-circuits future prompts until
/// reset (`/clear`). Distinct from `/yolo` (persisted config) — this is in-memory + session-scoped.
static SESSION_ALLOW: AtomicBool = AtomicBool::new(false);

fn approval_slot() -> &'static Mutex<Option<stdmpsc::Sender<char>>> {
    static S: OnceLock<Mutex<Option<stdmpsc::Sender<char>>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// Whether the user chose "allow all this session" — the approval gate skips prompts when true.
pub fn session_allow_all() -> bool {
    SESSION_ALLOW.load(Ordering::Relaxed)
}

/// Clear the session-wide allow (called on `/clear` so a fresh conversation re-confirms).
pub fn reset_session_allow() {
    SESSION_ALLOW.store(false, Ordering::Relaxed);
}

/// Block until the user answers an in-TUI approval prompt; `true` = allow. Routed through the
/// keyboard thread so it composes with the pinned box instead of fighting it for stdin. MUST be
/// called from the SERIAL tool path on a tokio worker (the caller wraps it in `block_in_place`),
/// never from the parallel scoped-thread batch. Safe-denies if the TUI isn't active.
pub fn ask_approval(prompt_line: &str) -> bool {
    if session_allow_all() {
        return true;
    }
    if !active() {
        return false;
    }
    emit_line(prompt_line);
    let (tx, rx) = stdmpsc::channel::<char>();
    *approval_slot().lock().unwrap_or_else(|e| e.into_inner()) = Some(tx);
    APPROVAL_PENDING.store(true, Ordering::Relaxed);
    let ans = rx.recv().unwrap_or('n'); // a dropped sender (shouldn't happen) → safe-deny
    APPROVAL_PENDING.store(false, Ordering::Relaxed);
    match ans {
        'a' => {
            SESSION_ALLOW.store(true, Ordering::Relaxed);
            true
        }
        'y' => true,
        _ => false,
    }
}

/// The width (columns) the pinned box is drawn at — the canonical wrap width for streamed output so
/// the Markdown renderer wraps to exactly the box, not to a separately-probed (possibly larger)
/// window edge. When the TUI isn't active, falls back to the live terminal width.
pub fn width() -> usize {
    if active() {
        render().lock().unwrap().cols as usize
    } else {
        term_size().1 as usize
    }
}

fn term_size() -> (u16, u16) {
    // console returns (rows, cols); fall back to a sane default if it can't probe.
    let (r, c) = Term::stdout().size();
    (r.max(8), c.max(20))
}

/// Reconcile the stored geometry against the live terminal after a possible resize. Returns whether
/// anything changed (the caller repaints the box afterward either way it returns true).
///
/// THE resize fix: the footer is pinned to absolute rows derived from `r.rows`, and the scroll
/// region is `1..r.rows-FOOTER`. If the window's HEIGHT changes and we keep the stale `r.rows`, the
/// footer is painted at the old offset while a later paint lands one row off — leaving a *ghost*
/// status/box line behind (the "duplicated status line / vỡ layout" bug). So on a height change we
/// wipe the union of the old+new footer bands, rebuild the scroll region to the new size, and
/// re-anchor the output slot at the new region bottom (same move as [`resume`]); the transcript
/// above stays as scrollback. A width-only change just updates `cols` (the box + streamed wrap track
/// it) with no region rebuild. Emits ANSI into `buf` ONLY on an actual change, so per-keystroke
/// callers stay cheap and never disturb the output slot when nothing moved.
///
/// MUST NOT be called mid-stream (it moves the scroll region + output slot). Safe points only:
/// turn start/end ([`set_working`]), post-turn status ([`set_status`]), and idle keystrokes
/// ([`repaint`], which gates on `!WORKING`).
fn reconcile_geometry(r: &mut Render, buf: &mut String) -> bool {
    let (rows, cols) = term_size();
    if rows == r.rows && cols == r.cols {
        return false;
    }
    r.cols = cols;
    if rows == r.rows {
        return true; // width only — no region rebuild, no output re-anchor
    }
    // Height changed → the footer band moved. Clear from the topmost old/new footer row down to the
    // new bottom so no ghost box/status survives in the new viewport, then rebuild the region.
    let old_top = r.rows.saturating_sub(FOOTER) + 1;
    let new_top = rows.saturating_sub(FOOTER) + 1;
    for row in old_top.min(new_top)..=rows {
        goto(buf, row, 1);
        clear_line(buf);
    }
    LAST_PAL.store(0, Ordering::Relaxed);
    r.rows = rows;
    let region_bottom = rows.saturating_sub(FOOTER).max(1);
    reset_region(buf);
    set_region(buf, 1, region_bottom);
    goto(buf, region_bottom, 1); // re-anchor the output slot at the new region bottom…
    clear_line(buf); // …on a clean line so the next streamed token doesn't overprint residue
    save_cursor(buf);
    true
}

/// Truncate a plain (un-styled) string to `max` display columns, adding an ellipsis when it would
/// overflow. Width-aware (handles wide glyphs) so the status line can never wrap onto a second row —
/// a wrapped status is the other way the footer "doubles".
fn truncate_to_width(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if measure_text_width(s) <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1); // leave a cell for the ellipsis
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = measure_text_width(&ch.to_string());
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Style the HUD string: the whole line is muted, but the mode chip pops so the active approval mode
/// reads at a glance (the one spot of colour in an otherwise quiet status line). Per the design,
/// `⚡ yolo` burns warm **gold** (the reserved warm accent) while `◆ smart` stays calm **moonlight** —
/// the colour itself signals "this mode runs hot" vs "this mode is careful". Operates on the
/// already-truncated PLAIN string, so it never splits an ANSI escape.
fn style_hud(s: &str) -> String {
    if let Some(i) = s.find('⚡') {
        let (head, tail) = s.split_at(i);
        return format!("{}{}", theme::muted(head), theme::warn(tail)); // yolo → gold
    }
    if let Some(i) = s.find('◆') {
        let (head, tail) = s.split_at(i);
        return format!("{}{}", theme::muted(head), theme::accent(tail)); // smart → moonlight
    }
    theme::muted(s).to_string()
}

// ── ANSI helpers (written into a String, flushed under the lock) ──────────────────────────────
fn set_region(buf: &mut String, top: u16, bot: u16) {
    buf.push_str(&format!("\x1b[{top};{bot}r"));
}
fn reset_region(buf: &mut String) {
    buf.push_str("\x1b[r");
}
fn goto(buf: &mut String, row: u16, col: u16) {
    buf.push_str(&format!("\x1b[{row};{col}H"));
}
fn save_cursor(buf: &mut String) {
    buf.push_str("\x1b7");
}
fn restore_cursor(buf: &mut String) {
    buf.push('\x1b');
    buf.push('8');
}
fn clear_line(buf: &mut String) {
    buf.push_str("\x1b[2K");
}

/// Draw the live slash palette in the rows directly above the status line (`top_row`). Filters as
/// the user types `/…`; the top match is highlighted (Tab completes it). Rows are inside the scroll
/// region, so the palette transiently overlays the bottom transcript lines while a command is being
/// typed and is cleared the moment the palette shrinks or closes (tracked via `LAST_PAL`).
fn paint_palette(buf: &mut String, r: &Render, top_row: u16, w: usize) {
    let matches = slash_matches(&r.draft);
    let max_above = top_row.saturating_sub(1) as usize; // never draw above row 1
    let vis = matches.len().min(PALETTE_MAX).min(max_above); // rows actually drawn this frame
    let prev = LAST_PAL.load(Ordering::Relaxed) as usize;

    // Clear rows a previously-taller palette occupied (shrink or close).
    for i in vis..prev {
        if (i as u16) < top_row {
            goto(buf, top_row - 1 - i as u16, 1);
            clear_line(buf);
        }
    }
    if vis == 0 {
        LAST_PAL.store(0, Ordering::Relaxed);
        return;
    }
    // SCROLL: the match list can exceed the visible window (19 commands, 7 rows). Slide a window of
    // `vis` items that always contains the selection, so ↑/↓ can reach EVERY command (e.g. `/mcp`),
    // not just the first 7. `matches[start]` sits nearest the box; higher indices climb upward.
    let sel = r.palette_sel.min(matches.len() - 1);
    let start = if sel < vis { 0 } else { sel - vis + 1 };
    let more_above = start + vis < matches.len(); // higher-index items off the top
    let more_below = start > 0; // lower-index items off the bottom (toward the input box)
    for i in 0..vis {
        let mi = start + i;
        let (name, desc) = matches[mi];
        goto(buf, top_row - 1 - i as u16, 1);
        clear_line(buf);
        let is_sel = mi == sel;
        let icon = crate::ui::icons::g(crate::ui::icons::slash(name));
        let marker = if is_sel { style("›").color256(ACCENT).bold().to_string() } else { " ".to_string() };
        let nm = if is_sel {
            style(format!("/{name}")).color256(ACCENT).bold().to_string()
        } else {
            style(format!("/{name}")).color256(ACCENT).to_string()
        };
        // Trim the gist so the line can't overrun the terminal width.
        let budget = w.saturating_sub(name.len() + 8);
        let gist: String = desc.chars().take(budget).collect();
        let mut line = format!("  {marker} {icon}{nm}  {}", style(gist).dim());
        // A faint `⋯` on the edge row signals there are more commands to scroll to.
        if (i == vis - 1 && more_above) || (i == 0 && more_below) {
            line.push_str(&format!("  {}", theme::faint("⋯")));
        }
        buf.push_str(&line);
    }
    LAST_PAL.store(vis as u16, Ordering::Relaxed);
}

/// Append the borderless footer at the bottom `FOOTER` rows and leave the cursor at the input text
/// position. The footer is four stacked rows — a faint full-width rule, the HUD status line, a blank
/// breather, and the moonlit `❯` prompt — so the chat reads as one continuous column with no box around
/// it (the rule is the only thing dividing the live prompt from the scrolling transcript above).
/// Pure string-building; the caller writes+flushes under the lock.
fn paint_box(buf: &mut String, r: &Render) {
    let w = r.cols.max(20) as usize;
    let inner = w.saturating_sub(3); // "> " prefix (2 cells) + a 1-col right margin
    let top_row = r.rows.saturating_sub(FOOTER) + 1;

    // live slash palette, just above the footer band (overlays the bottom of the scroll region)
    paint_palette(buf, r, top_row, w);

    // row 1: a faint full-width rule — the borderless divider between transcript and footer.
    goto(buf, top_row, 1);
    clear_line(buf);
    buf.push_str(&theme::faint("─".repeat(w)).to_string());

    // row 2: HUD — muted "model · tokens · …" on the left, a state pill on the right edge of meaning.
    goto(buf, top_row + 1, 1);
    clear_line(buf);
    let work = if WORKING.load(Ordering::Relaxed) {
        let frame = SPIN[WORK_FRAME.load(Ordering::Relaxed) % SPIN.len()];
        let secs = work_elapsed_secs();
        format!(
            "{} {} {}",
            style(frame).color256(ACCENT).bold(),
            theme::muted("working"),
            theme::faint(format!("· {secs}s · Esc to stop"))
        )
    } else {
        format!("{} {}", theme::ok("●"), theme::faint("ready"))
    };
    // Bound the status so the right-side pill always fits and the line can never wrap onto a second
    // row (a wrapped HUD is how the footer visually "doubles").
    let avail = w.saturating_sub(measure_text_width(&work) + 3);
    let status = truncate_to_width(&r.status, avail);
    buf.push_str(&format!("{}   {work}", style_hud(&status)));

    // row 3: a blank breather between the HUD and the prompt (matches the airy mockup spacing).
    goto(buf, top_row + 2, 1);
    clear_line(buf);

    // row 4: the moonlit `❯` prompt — scroll so the cursor is visible.
    let imgtag = if r.images > 0 {
        style(format!("[{}img] ", r.images)).color256(ACCENT).to_string()
    } else {
        String::new()
    };
    // Per-char DISPLAY width (CJK/emoji = 2, combining marks = 0). All horizontal math below is in
    // cells, not chars, so wide glyphs place the caret correctly and the row never overflows/wraps.
    let cellw = |c: char| measure_text_width(&c.to_string());
    // `shown` = the prompt content, `caret_off` = the caret offset from the text start in CELLS. An
    // empty prompt shows a dim placeholder; a multi-line draft is collapsed to a chip.
    let (shown, caret_off) = if r.draft.is_empty() && r.images == 0 {
        // Empty prompt → dim placeholder hint; the caret rests at the text start (col 3).
        let ph: String = "Type a message  ·  / for commands  ·  Esc to exit".chars().take(inner).collect();
        (theme::faint(ph).italic().to_string(), 0)
    } else if r.draft.contains(&'\n') {
        // A multi-line draft is (almost always) a PASTE → collapse it to a single chip: line count +
        // a peek of the first non-empty line. The prompt stays one row and the model still receives
        // the whole text on submit. Mirrors the Claude-CLI "[Pasted N lines]" affordance.
        let text: String = r.draft.iter().collect();
        let nlines = text.lines().count().max(1);
        let first = text.lines().find(|l| !l.trim().is_empty()).map(str::trim).unwrap_or("");
        let head = format!("↵ {nlines} lines pasted");
        let room = inner.saturating_sub(head.chars().count() + 3);
        let chip = if room > 4 && !first.is_empty() {
            let peek: String = first.chars().take(room).collect();
            let ell = if first.chars().count() > room { "…" } else { "" };
            format!("{head} · {peek}{ell}")
        } else {
            head
        };
        let chip: String = chip.chars().take(inner).collect();
        let wc = measure_text_width(&chip);
        (style(chip).color256(ACCENT).to_string(), wc) // caret parks at the end of the chip
    } else {
        // Width-aware horizontal scroll: walk LEFT from the cursor accumulating cell widths until the
        // window would exceed `inner` (keeping a cell for the caret), then emit chars forward until the
        // cell budget is full. caret_off is the cell width of draft[scroll..cursor].
        let mut scroll = r.cursor;
        let mut used = 0usize;
        while scroll > 0 {
            let w = cellw(r.draft[scroll - 1]);
            if used + w > inner.saturating_sub(1) {
                break;
            }
            used += w;
            scroll -= 1;
        }
        let caret_off: usize = r.draft[scroll..r.cursor].iter().map(|&c| cellw(c)).sum();
        let mut shown = String::new();
        let mut w = 0usize;
        for &c in &r.draft[scroll..] {
            let cw = cellw(c);
            if w + cw > inner {
                break;
            }
            shown.push(c);
            w += cw;
        }
        (shown, caret_off)
    };
    goto(buf, top_row + 3, 1);
    clear_line(buf);
    buf.push_str(&format!(
        "{arrow} {imgtag}{shown}",
        arrow = style("❯").color256(ACCENT).bold()
    ));

    // Leave the cursor at the text insertion point. The visible prefix is `❯ ` = 2 cells, so text
    // starts at column 3; the insertion point before draft index (cursor - scroll) is at
    // column 3 + imgtag + caret_off.
    let col = 2 + imgtag_visible_len(r.images) + caret_off + 1;
    goto(buf, top_row + 3, col as u16);
}

/// Visible width of the `[Nimg] ` prefix (0 when no images) — kept in sync with `paint_box`.
fn imgtag_visible_len(images: usize) -> usize {
    if images > 0 {
        format!("[{images}img] ").chars().count()
    } else {
        0
    }
}

fn flush(buf: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(buf.as_bytes());
    let _ = out.flush();
}

/// Enter sticky mode: clear the screen, print `intro` into the (new) scroll region, seed the output
/// cursor, and paint the box. No-op when stdout isn't a TTY.
pub fn activate(intro: &str, status: &str) {
    if !std::io::stdout().is_terminal() {
        return;
    }
    let mut r = render().lock().unwrap();
    let (rows, cols) = term_size();
    r.rows = rows;
    r.cols = cols;
    r.status = status.to_string();
    let mut buf = String::new();
    reset_region(&mut buf);
    buf.push_str("\x1b[2J\x1b[H"); // clear + home
    set_region(&mut buf, 1, rows.saturating_sub(FOOTER).max(1));
    goto(&mut buf, 1, 1);
    buf.push_str(intro);
    buf.push('\n');
    save_cursor(&mut buf); // output continues from here
    paint_box(&mut buf, &r);
    flush(&buf);
    ACTIVE.store(true, Ordering::Relaxed);
}

/// Leave sticky mode: reset the scroll region and drop the cursor below the footer so the shell
/// prompt (or a `dialoguer` menu, via [`suspend`]) continues cleanly.
pub fn deactivate() {
    if !active() {
        return;
    }
    ACTIVE.store(false, Ordering::Relaxed);
    let r = render().lock().unwrap();
    let mut buf = String::new();
    reset_region(&mut buf);
    goto(&mut buf, r.rows, 1);
    buf.push('\n');
    flush(&buf);
}

/// Temporarily yield the terminal so a `dialoguer` slash menu can use stdin/redraw normally. The
/// input thread is already parked (it parks itself right after sending a `Slash`).
///
/// Crucially, ERASE the pinned box first: once the scroll region is reset to full screen, anything
/// the menu prints scrolls the whole screen — and a box left on those rows scrolls UP into the
/// transcript as a ghost (the bug where a stale input box stuck in the middle of the history). So we
/// clear the footer rows, drop the region, and park the cursor where the box was, so the menu
/// renders at the bottom of the existing transcript and continues it cleanly.
pub fn suspend() {
    if !active() {
        return;
    }
    ACTIVE.store(false, Ordering::Relaxed);
    let r = render().lock().unwrap();
    let top = r.rows.saturating_sub(FOOTER) + 1;
    let mut buf = String::new();
    for row in top..=r.rows {
        goto(&mut buf, row, 1);
        clear_line(&mut buf);
    }
    LAST_PAL.store(0, Ordering::Relaxed);
    reset_region(&mut buf);
    goto(&mut buf, top, 1);
    flush(&buf);
}

/// Re-enter sticky mode after a slash menu. A `dialoguer` menu leaves the physical cursor at an
/// unpredictable spot (often low, sometimes inside the footer zone). If we saved the output slot
/// THERE, the next streamed token would land mid-screen and overwrite the transcript — or worse,
/// collide with the pinned box. So we re-anchor the output slot to the BOTTOM row of the scroll
/// region: agent output always appends there and scrolls up, exactly like the steady state. The
/// menu's leftover lines stay above as scrollback (harmless) and scroll away as new output arrives.
pub fn resume(status: &str) {
    if !std::io::stdout().is_terminal() {
        return;
    }
    let mut r = render().lock().unwrap();
    let (rows, cols) = term_size();
    r.rows = rows;
    r.cols = cols;
    r.status = status.to_string();
    let region_bottom = rows.saturating_sub(FOOTER).max(1);
    let mut buf = String::new();
    reset_region(&mut buf);
    set_region(&mut buf, 1, region_bottom);
    goto(&mut buf, region_bottom, 1); // anchor the output slot at the region bottom…
    clear_line(&mut buf); // …on a clean line, so the next token doesn't overprint menu residue
    save_cursor(&mut buf);
    paint_box(&mut buf, &r);
    flush(&buf);
    ACTIVE.store(true, Ordering::Relaxed);
}

/// Print agent output into the scroll region above the box, then repaint the box. When the TUI
/// isn't active this is a plain `print!` so the `chat`/`agent` subcommands are unaffected.
pub fn emit(s: &str) {
    if !active() {
        print!("{s}");
        let _ = std::io::stdout().flush();
        return;
    }
    let r = render().lock().unwrap();
    let mut buf = String::new();
    restore_cursor(&mut buf); // back to the saved output position (inside the scroll region)
    buf.push_str(s);
    save_cursor(&mut buf); // remember where output continues
    paint_box(&mut buf, &r);
    flush(&buf);
}

/// `emit` a whole line.
pub fn emit_line(s: &str) {
    let mut line = String::with_capacity(s.len() + 1);
    line.push_str(s);
    line.push('\n');
    emit(&line);
}

/// Set the working flag (drives the box indicator + the input thread's Esc semantics) and repaint.
/// Always updates the flag even when the TUI is inactive, so the input thread sees it.
///
/// NOTE: only [`emit`] may touch the `\x1b7`/`\x1b8` save slot — it owns "the output position". A
/// box repaint just moves the physical cursor into the box; the next `emit` restores the saved
/// output position first, so leaving the cursor in the box here is harmless (and overwriting the
/// shared slot would corrupt where the next streamed token lands).
pub fn set_working(working: bool) {
    WORKING.store(working, Ordering::Relaxed);
    // Reset the elapsed-seconds clock + spinner frame at each task boundary so the counter starts at
    // 0s and the indicator restarts cleanly. The ticker thread animates it while `working`.
    if working {
        *work_start_slot().lock().unwrap() = Some(Instant::now());
        WORK_FRAME.store(0, Ordering::Relaxed);
        start_ticker();
    } else {
        *work_start_slot().lock().unwrap() = None;
    }
    if !active() {
        return;
    }
    let mut r = render().lock().unwrap();
    // Turn boundary (not mid-stream) → safe to fully reconcile a resized/maximised window: width so
    // the box + streamed wrap (`width()`) track it, AND height so the scroll region + footer rows
    // snap to the new size instead of leaving a ghost footer behind.
    let mut buf = String::new();
    reconcile_geometry(&mut r, &mut buf);
    paint_box(&mut buf, &r);
    flush(&buf);
}

/// Update the status text (model · tokens · yolo) and repaint. (Does not touch the output slot —
/// see [`set_working`].)
pub fn set_status(status: &str) {
    if !active() {
        return;
    }
    let mut r = render().lock().unwrap();
    r.status = status.to_string();
    let mut buf = String::new();
    // Post-turn (line 1854) — the first safe point to fix a resize that happened mid-turn.
    reconcile_geometry(&mut r, &mut buf);
    paint_box(&mut buf, &r);
    flush(&buf);
}

/// Handles to drive the REPL from the background input thread.
pub struct InputHandles {
    /// Submissions (chat / slash / quit), in the order the user pressed Enter.
    pub submissions: UnboundedReceiver<Submission>,
    /// Fires when the user asks to cancel an in-flight turn (Esc/Ctrl-C while working).
    pub cancel: UnboundedReceiver<()>,
    /// Send `()` to unpark the input thread after a slash command finishes.
    pub resume: stdmpsc::Sender<()>,
    /// Inject a synthetic submission into the same queue the keyboard thread feeds. Used to fire a
    /// custom slash command's expanded prompt back through the normal chat path.
    pub inject: UnboundedSender<Submission>,
    /// The keyboard thread (detached for the session; kept so the handle isn't dropped eagerly).
    _handle: JoinHandle<()>,
}

/// Spawn the background keyboard thread. It owns stdin for the session: edits the draft, repaints
/// the box on each key, and turns Enter/Esc into [`Submission`]s / cancel signals.
pub fn spawn_input() -> InputHandles {
    let (sub_tx, submissions) = mpsc::unbounded_channel::<Submission>();
    let (cancel_tx, cancel) = mpsc::unbounded_channel::<()>();
    let (resume_tx, resume_rx) = stdmpsc::channel::<()>();

    let inject = sub_tx.clone();
    let handle = std::thread::spawn(move || {
        input_loop(sub_tx, cancel_tx, resume_rx);
    });

    InputHandles { submissions, cancel, resume: resume_tx, inject, _handle: handle }
}

/// Repaint the box from the current shared state (used by the input thread after an edit).
fn repaint() {
    if !active() {
        return;
    }
    let mut r = render().lock().unwrap();
    let mut buf = String::new();
    // Catch a resize done while idle so the box snaps to the new bottom on the next keystroke. Gated
    // on `!WORKING`: a keystroke can arrive WHILE the agent streams (type-ahead queue), and rebuilding
    // the region then would move the live output slot mid-stream — let the post-turn paths fix it.
    if !WORKING.load(Ordering::Relaxed) {
        reconcile_geometry(&mut r, &mut buf);
    }
    paint_box(&mut buf, &r);
    flush(&buf);
}

fn input_loop(
    sub_tx: UnboundedSender<Submission>,
    cancel_tx: UnboundedSender<()>,
    resume_rx: stdmpsc::Receiver<()>,
) {
    let term = Term::stdout();
    let mut history: Vec<String> = Vec::new();
    let mut hist_idx: Option<usize> = None;
    let mut draft_saved: Vec<char> = Vec::new();

    loop {
        let t0 = Instant::now();
        let key = match term.read_key() {
            Ok(k) => k,
            Err(_) => {
                let _ = sub_tx.send(Submission::Quit);
                return;
            }
        };
        // A key that came back almost instantly was already waiting in the OS input buffer → it's
        // part of a burst (a paste), not a deliberate keystroke. See `PASTE_COALESCE_MS`.
        let buffered = t0.elapsed() < Duration::from_millis(PASTE_COALESCE_MS);
        // If the agent is awaiting a per-action approval, THIS keystroke is the answer — route a
        // y/n/a decision to the blocked gate and never treat it as draft input. Other keys are
        // ignored so a stray press can't accidentally approve.
        if APPROVAL_PENDING.load(Ordering::Relaxed) {
            let decided = match key {
                Key::Char('y') | Key::Char('Y') => Some('y'),
                Key::Char('a') | Key::Char('A') => Some('a'),
                Key::Char('n') | Key::Char('N') | Key::Escape => Some('n'),
                _ => None,
            };
            if let Some(c) = decided {
                if let Some(tx) = approval_slot().lock().unwrap_or_else(|e| e.into_inner()).take() {
                    let _ = tx.send(c);
                }
            }
            continue;
        }
        match key {
            // A newline INSIDE a paste → a literal newline in the draft, never a submit. This is the
            // fix for a multi-line paste firing one message per line: the whole paste accumulates in
            // one draft and is sent (and read by the model) as a single message.
            Key::Enter if buffered => {
                let mut r = render().lock().unwrap();
                let cur = r.cursor;
                r.draft.insert(cur, '\n');
                r.cursor += 1;
                r.palette_sel = 0;
                drop(r);
                hist_idx = None;
                repaint();
            }
            Key::Enter => {
                let (line, images, pick) = {
                    let mut r = render().lock().unwrap();
                    let line: String = r.draft.iter().collect();
                    let images = r.images;
                    // If the live palette is open, Enter runs the HIGHLIGHTED command — this is what
                    // resolves a partial `/se` (or an ↑/↓ pick) to the full command name.
                    let matches = slash_matches(&r.draft);
                    let pick = if images > 0 || matches.is_empty() {
                        None // an image attachment makes it a chat message, not a slash command
                    } else {
                        Some(matches[r.palette_sel.min(matches.len() - 1)].0.to_string())
                    };
                    r.draft.clear();
                    r.cursor = 0;
                    r.images = 0;
                    r.palette_sel = 0;
                    (line, images, pick)
                };
                hist_idx = None;
                repaint();
                if let Some(name) = pick {
                    history.push(format!("/{name}"));
                    if sub_tx.send(Submission::Slash(name)).is_err() {
                        return;
                    }
                    // Park to hand stdin to the REPL's dialoguer menu — but ONLY when idle. While the
                    // agent is working the REPL is blocked in its turn `select!` and won't consume this
                    // Slash until the turn ends; parking now would freeze ALL input (typing, queueing,
                    // even Esc) for the whole turn — the confirmed "can't chat while working" freeze.
                    // When working: leave it queued, keep reading keys; the REPL runs it after the turn.
                    if !WORKING.load(Ordering::Relaxed) {
                        while resume_rx.try_recv().is_ok() {} // discard resume buffered by a deferred slash
                        let _ = resume_rx.recv();
                    }
                    continue;
                }
                let trimmed = line.trim().to_string();
                if trimmed.is_empty() && images == 0 {
                    continue; // empty enter → ignore
                }
                if !trimmed.is_empty() {
                    history.push(trimmed.clone());
                }
                if let Some(cmd) = trimmed.strip_prefix('/').filter(|_| images == 0) {
                    if sub_tx.send(Submission::Slash(cmd.to_string())).is_err() {
                        return;
                    }
                    // Park to hand stdin to the dialoguer menu only when idle (see the pick branch):
                    // parking mid-turn would freeze all input until the turn ends.
                    if !WORKING.load(Ordering::Relaxed) {
                        while resume_rx.try_recv().is_ok() {} // discard resume buffered by a deferred slash
                        let _ = resume_rx.recv();
                    }
                } else {
                    // Image data URLs aren't carried here (the box only tracks a count); the REPL
                    // resolves attachments — for now we forward the text and image count is folded in
                    // by the caller via the clipboard buffer. We send just the text + an empty vec;
                    // the clipboard images live in shared state drained by the caller.
                    let imgs = take_pending_images();
                    if sub_tx.send(Submission::Chat(line, imgs)).is_err() {
                        return;
                    }
                }
            }
            Key::Escape | Key::Char('\u{3}') | Key::Char('\u{4}') => {
                if WORKING.load(Ordering::Relaxed) {
                    request_cancel(); // cooperative: lets a running tool (e.g. a long shell) abort now
                    let _ = cancel_tx.send(()); // and wake the REPL's select! at the next yield point
                } else {
                    let empty = {
                        let mut r = render().lock().unwrap();
                        if r.draft.is_empty() && r.images == 0 {
                            true
                        } else {
                            r.draft.clear();
                            r.cursor = 0;
                            r.images = 0;
                            clear_pending_images();
                            false
                        }
                    };
                    if empty {
                        let _ = sub_tx.send(Submission::Quit);
                        return;
                    }
                    hist_idx = None;
                    repaint();
                }
            }
            Key::Tab => {
                // Complete the highlighted slash command into the draft (with a trailing space so
                // you can type args); this also closes the palette.
                let name = {
                    let r = render().lock().unwrap();
                    let m = slash_matches(&r.draft);
                    (!m.is_empty()).then(|| m[r.palette_sel.min(m.len() - 1)].0.to_string())
                };
                if let Some(name) = name {
                    let mut r = render().lock().unwrap();
                    r.draft = format!("/{name} ").chars().collect();
                    r.cursor = r.draft.len();
                    r.palette_sel = 0;
                    drop(r);
                    hist_idx = None;
                    repaint();
                }
            }
            Key::Char('\u{f}') => {
                // Ctrl-O: grab a clipboard screenshot (Win+Shift+S) as a vision attachment.
                if let Ok(Some(url)) = crate::ui::image_input::clipboard_image_data_url() {
                    push_pending_image(url);
                    render().lock().unwrap().images = pending_image_count();
                    repaint();
                }
            }
            Key::Char('\u{18}') => {
                // Ctrl-X: drop the most recent image attachment.
                if pop_pending_image() {
                    render().lock().unwrap().images = pending_image_count();
                    repaint();
                }
            }
            Key::Char(c) if c.is_control() => {} // ignore stray control chars
            Key::Char(c) => {
                let mut r = render().lock().unwrap();
                let cur = r.cursor;
                r.draft.insert(cur, c);
                r.cursor += 1;
                r.palette_sel = 0; // matches changed → reset highlight to the nearest
                drop(r);
                hist_idx = None;
                repaint();
            }
            Key::Backspace => {
                let mut r = render().lock().unwrap();
                if r.cursor > 0 {
                    let cur = r.cursor - 1;
                    r.draft.remove(cur);
                    r.cursor = cur;
                    r.palette_sel = 0;
                    drop(r);
                    repaint();
                }
            }
            Key::Del => {
                let mut r = render().lock().unwrap();
                if r.cursor < r.draft.len() {
                    let cur = r.cursor;
                    r.draft.remove(cur);
                    r.palette_sel = 0;
                    drop(r);
                    repaint();
                }
            }
            Key::ArrowLeft => {
                let mut r = render().lock().unwrap();
                if r.cursor > 0 {
                    r.cursor -= 1;
                    drop(r);
                    repaint();
                }
            }
            Key::ArrowRight => {
                let mut r = render().lock().unwrap();
                if r.cursor < r.draft.len() {
                    r.cursor += 1;
                    drop(r);
                    repaint();
                }
            }
            Key::Home => {
                render().lock().unwrap().cursor = 0;
                repaint();
            }
            Key::End => {
                let mut r = render().lock().unwrap();
                r.cursor = r.draft.len();
                drop(r);
                repaint();
            }
            Key::ArrowUp => {
                // While the slash palette is open, ↑/↓ move the highlight (over the FULL match list —
                // the palette window scrolls to follow) instead of recalling history.
                let pal = {
                    let r = render().lock().unwrap();
                    slash_matches(&r.draft).len()
                };
                if pal > 0 {
                    let mut r = render().lock().unwrap();
                    if r.palette_sel + 1 < pal {
                        r.palette_sel += 1;
                    }
                    drop(r);
                    repaint();
                    continue;
                }
                if history.is_empty() {
                    continue;
                }
                let mut r = render().lock().unwrap();
                let idx = match hist_idx {
                    None => {
                        draft_saved = r.draft.clone();
                        history.len() - 1
                    }
                    Some(0) => 0,
                    Some(i) => i - 1,
                };
                hist_idx = Some(idx);
                r.draft = history[idx].chars().collect();
                r.cursor = r.draft.len();
                drop(r);
                repaint();
            }
            Key::ArrowDown => {
                let pal = {
                    let r = render().lock().unwrap();
                    slash_matches(&r.draft).len()
                };
                if pal > 0 {
                    let mut r = render().lock().unwrap();
                    if r.palette_sel > 0 {
                        r.palette_sel -= 1;
                    }
                    drop(r);
                    repaint();
                    continue;
                }
                let mut r = render().lock().unwrap();
                match hist_idx {
                    Some(i) if i + 1 < history.len() => {
                        hist_idx = Some(i + 1);
                        r.draft = history[i + 1].chars().collect();
                        r.cursor = r.draft.len();
                    }
                    Some(_) => {
                        hist_idx = None;
                        r.draft = draft_saved.clone();
                        r.cursor = r.draft.len();
                    }
                    None => {}
                }
                drop(r);
                repaint();
            }
            _ => {}
        }
    }
}

// ── pending clipboard image attachments (set by Ctrl-O in the input thread, drained on submit) ──
fn pending_images() -> &'static Mutex<Vec<String>> {
    static P: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(Vec::new()))
}
fn push_pending_image(url: String) {
    pending_images().lock().unwrap().push(url);
}
fn pop_pending_image() -> bool {
    pending_images().lock().unwrap().pop().is_some()
}
fn pending_image_count() -> usize {
    pending_images().lock().unwrap().len()
}
fn clear_pending_images() {
    pending_images().lock().unwrap().clear();
}
fn take_pending_images() -> Vec<String> {
    std::mem::take(&mut *pending_images().lock().unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn imgtag_len_matches_render() {
        assert_eq!(imgtag_visible_len(0), 0);
        assert_eq!(imgtag_visible_len(3), "[3img] ".chars().count());
    }

    #[test]
    fn session_allow_short_circuits_approval() {
        reset_session_allow();
        assert!(!session_allow_all(), "starts off");
        // When session-allow is set, ask_approval returns true immediately (no input thread needed).
        SESSION_ALLOW.store(true, Ordering::Relaxed);
        assert!(ask_approval("⚙ file_edit x — approve?"), "allow-all short-circuits to true");
        reset_session_allow();
        assert!(!session_allow_all(), "reset clears it");
    }

    #[test]
    fn elapsed_counter_is_zero_when_idle_and_frames_are_single_cell() {
        *work_start_slot().lock().unwrap() = None;
        assert_eq!(work_elapsed_secs(), 0, "no task started → 0s");
        // every braille frame must measure as one cell so the right-edge pill stays aligned
        for f in SPIN {
            assert_eq!(measure_text_width(f), 1, "{f:?} must be a single cell");
        }
    }

    #[test]
    fn paint_box_is_borderless_with_rule_and_prompt() {
        // Structure-only (no WORKING assertion: that atomic is shared + mutated by sibling tests in
        // parallel, so the work-pill text is racy — the borderless shape is what this guards).
        let r = Render {
            cols: 40,
            rows: 24,
            draft: "hello world".chars().collect(),
            cursor: 11,
            images: 0,
            status: "model · 1K tok".into(),
            palette_sel: 0,
        };
        let mut buf = String::new();
        paint_box(&mut buf, &r);
        assert!(buf.contains('─'), "has the faint footer rule");
        assert!(buf.contains('❯'), "has the borderless moonlit prompt");
        assert!(!buf.contains('╭') && !buf.contains('╮'), "no box border");
        assert!(!buf.contains('│'), "no box sides");
    }

    /// The caret must land at the text insertion point: prefix `❯ ` is 2 cells → text at col 3,
    /// so an empty draft parks the caret at col 3 and N typed chars push it to col 3+N. Guards the
    /// off-by-one that stranded the caret one cell left (looked stuck at the start).
    #[test]
    fn caret_lands_at_text_insertion_point() {
        // Extract the column of the LAST cursor-move (`ESC[row;colH`) paint_box emits.
        fn last_goto_col(buf: &str) -> usize {
            let i = buf.rfind('\x1b').unwrap();
            let esc = &buf[i + 2..]; // skip "\x1b["
            let h = esc.find('H').unwrap();
            esc[..h].split(';').nth(1).unwrap().parse().unwrap()
        }
        let mk = |draft: &str, cursor: usize| Render {
            cols: 40,
            rows: 24,
            draft: draft.chars().collect(),
            cursor,
            images: 0,
            status: "m".into(),
            palette_sel: 0,
        };
        let mut buf = String::new();
        paint_box(&mut buf, &mk("", 0));
        assert_eq!(last_goto_col(&buf), 3, "empty draft → caret at text start (col 3)");

        buf.clear();
        paint_box(&mut buf, &mk("hello", 5));
        assert_eq!(last_goto_col(&buf), 8, "caret after 5 chars → col 8");

        // Wide glyphs (CJK) are 2 cells each → the caret offset must be in CELLS, not chars.
        buf.clear();
        paint_box(&mut buf, &mk("你好", 2));
        assert_eq!(last_goto_col(&buf), 7, "2 CJK chars = 4 cells → caret at col 3+4=7");
    }

    #[test]
    fn multiline_draft_collapses_to_a_paste_chip() {
        // A multi-line draft (a paste) must render as ONE collapsed chip — line count + a peek — not
        // the raw lines crammed into the box. The full text is still what gets submitted.
        WORKING.store(false, Ordering::Relaxed);
        let draft = "Trả lời tự nhiên\nKhông nhắc là AI\nCó thể pha trò";
        let r = Render {
            cols: 60,
            rows: 24,
            draft: draft.chars().collect(),
            cursor: draft.chars().count(),
            images: 0,
            status: "m".into(),
            palette_sel: 0,
        };
        let mut buf = String::new();
        paint_box(&mut buf, &r);
        assert!(buf.contains("3 lines pasted"), "chip shows the line count");
        assert!(!buf.contains("Không nhắc là AI"), "interior lines are collapsed, not shown raw");
        // The collapsed prompt row must still fit the width (no wrap).
        let top = r.rows - FOOTER + 1;
        let start = buf.find(&format!("\x1b[{};1H", top + 3)).unwrap();
        let rest = &buf[start..];
        let end = rest[1..].find('\x1b').map(|i| start + 1 + i).unwrap_or(buf.len());
        assert!(measure_text_width(&buf[start..end]) <= 60, "collapsed row fits the box width");
    }

    #[test]
    fn truncate_to_width_bounds_and_ellipsises() {
        assert_eq!(truncate_to_width("hello", 10), "hello", "fits → untouched");
        assert_eq!(truncate_to_width("hello", 5), "hello", "exact fit → untouched");
        let t = truncate_to_width("hello world", 5);
        assert!(measure_text_width(&t) <= 5, "never exceeds the budget");
        assert!(t.ends_with('…'), "overflow gets an ellipsis");
        assert_eq!(truncate_to_width("anything", 0), "", "zero budget → empty");
    }

    #[test]
    fn status_line_never_exceeds_box_width() {
        // A very long status on a narrow box must be truncated so the line can't wrap (wrap doubles
        // the footer). Check the painted status row stays within `cols`.
        WORKING.store(false, Ordering::Relaxed);
        let r = Render {
            cols: 30,
            rows: 24,
            draft: Vec::new(),
            cursor: 0,
            images: 0,
            status: "opus-4-8  ·  ~1.0K/200K tok  ·  9 turns  ·  42% ctx  ·  ⚡ yolo".into(),
            palette_sel: 0,
        };
        let mut buf = String::new();
        paint_box(&mut buf, &r);
        // The HUD sits on the 2nd footer row (after the rule). Slice it out by its two bracketing
        // cursor-moves (HUD row → blank row), then measure visible width (measure_text_width strips
        // the SGR/clear codes in between).
        let top = r.rows - FOOTER + 1;
        let start = buf.find(&format!("\x1b[{};1H", top + 1)).unwrap();
        let end = buf.find(&format!("\x1b[{};1H", top + 2)).unwrap();
        let row = &buf[start..end];
        assert!(measure_text_width(row) <= 30, "HUD row fits the width (no wrap)");
    }

    #[test]
    fn slash_palette_filters_live() {
        let v = |s: &str| s.chars().collect::<Vec<_>>();
        assert!(slash_matches(&v("hello")).is_empty(), "no leading slash → no palette");
        assert_eq!(slash_matches(&v("/")).len(), SLASH.len(), "bare / lists everything");
        let se: Vec<&str> = slash_matches(&v("/se")).iter().map(|(n, _)| *n).collect();
        assert!(se.contains(&"sessions") && se.contains(&"serve"), "/se → sessions, serve");
        assert!(!se.contains(&"model"), "/se excludes non-matches");
        assert!(slash_matches(&v("/model foo")).is_empty(), "once an arg is typed the palette hides");
        assert_eq!(slash_matches(&v("/se")).first().map(|(n, _)| *n), Some("sessions"), "top match earliest-listed");
        assert!(slash_matches(&v("/xyz")).is_empty(), "no match → nothing to complete");
    }

    #[test]
    fn submission_variants_roundtrip() {
        // The REPL classifies on these — guard the shape.
        let s = Submission::Chat("hi".into(), vec!["data:...".into()]);
        assert_eq!(s, Submission::Chat("hi".into(), vec!["data:...".into()]));
        assert_ne!(Submission::Quit, Submission::Slash("help".into()));
    }
}
