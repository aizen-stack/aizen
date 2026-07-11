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
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
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
    ("timeline", "checkpoint timeline (glance) · pick to restore"),
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

/// Wakes async waiters ([`cancelled`]) the instant a cancel is requested — the flag alone only
/// serves pollers; racing a long network tool needs an awaitable signal.
static CANCEL_NOTIFY: OnceLock<tokio::sync::Notify> = OnceLock::new();

fn cancel_notify() -> &'static tokio::sync::Notify {
    CANCEL_NOTIFY.get_or_init(tokio::sync::Notify::new)
}

/// Request cancellation of the in-flight turn (called by the input thread on Esc while working).
pub fn request_cancel() {
    CANCEL_REQUESTED.store(true, Ordering::Relaxed);
    cancel_notify().notify_waiters();
}

/// Resolves when a cancel is requested. Missed-wakeup-safe: registers with the Notify FIRST, then
/// re-checks the flag, so a request landing between check and await is still seen. Never resolves
/// outside the sticky REPL (nothing else sets the flag) — racing against it is a no-op there.
pub async fn cancelled() {
    loop {
        if cancel_requested() {
            return;
        }
        let notified = cancel_notify().notified();
        if cancel_requested() {
            return;
        }
        notified.await;
    }
}
/// Whether a cancel has been requested — polled by the synchronous tool path + the agent loop.
pub fn cancel_requested() -> bool {
    CANCEL_REQUESTED.load(Ordering::Relaxed)
}
/// Clear the cancel flag (called by the REPL at each turn boundary so a stale Esc can't kill the next).
pub fn clear_cancel() {
    CANCEL_REQUESTED.store(false, Ordering::Relaxed);
}

/// Star frames for the animated working indicator (a lone background thread advances this while
/// `WORKING`, so it pulses even when no token is streaming — e.g. before the first byte or during a
/// long tool call). Moonlight silver, drawn by [`paint_box`]. Every frame is exactly one cell.
const STAR: [&str; 6] = ["✶", "✷", "✸", "✹", "✺", "✻"];
/// Whimsical present-tense verbs cycled (slowly, every ~3s) in the working pill — the "still
/// thinking" flavour, Claude-Code style. Purely cosmetic: the elapsed clock + the `↑N tok` counter
/// are the real liveness signal.
const VERBS: &[&str] = &[
    "Pondering",
    "Contemplating",
    "Weaving words",
    "Honing",
    "Rummaging",
    "Threading ideas",
    "Distilling",
    "Incubating",
    "Refining",
    "Envisioning",
    "Racking my brain",
    "Toiling",
    "Figuring it out",
    "Calculating",
    "Linking ideas",
    "Wrapping up",
];
/// Rotating one-line tips shown under each submitted message (Claude-Code style) — a quiet
/// discoverability nudge for a feature the user may not know. Each turn advances by one (seeded off
/// `TIP_SEED`), so a session slowly surfaces the whole set instead of repeating one. Kept short so
/// they fit one line; silenced with `AIZEN_NO_TIPS`.
const TIPS: &[&str] = &[
    "type `/` to browse commands, or `@` to attach a file",
    "press Esc to cancel the current turn without quitting",
    "`#remember <fact>` teaches the memory brain a durable fact",
    "start a line with `!` to run a shell command inline",
    "`/model` switches models mid-session; `/config` opens setup",
    "`/persona` role-plays a character with its own evolving memory",
    "`/compact` summarizes old turns to free up context",
    "`/time` saves & restores code checkpoints (git-backed)",
    "`/skills` loads reusable step-by-step procedures on demand",
    "delegate a sub-task with the `task` tool for parallel work",
    "`/cost` and `/tokens` show this session's usage",
    "set a Tavily key (`/config`) to unlock `web_search`",
    "`/apps` connects GitHub, Notion, Slack & more via MCP",
    "`/yolo` auto-approves tool calls; `/smart` approves read-only",
];
/// Per-session tip cursor — advanced once per submitted turn so tips rotate rather than repeat.
static TIP_SEED: AtomicUsize = AtomicUsize::new(0);

/// The next rotating tip line (`""` when tips are off via `AIZEN_NO_TIPS`, or on a pipe/CI). Advances
/// the cursor each call, so successive turns show successive tips.
pub fn next_tip() -> &'static str {
    if crate::core::cli_config::branded_flag("NO_TIPS") || !std::io::stdout().is_terminal() {
        return "";
    }
    let i = TIP_SEED.fetch_add(1, Ordering::Relaxed);
    TIPS[i % TIPS.len()]
}

/// Rotating cursor for the per-turn working verb (advanced once per turn, so each run opens on a
/// fresh word). Distinct from the footer's old in-place cycling: the verb now prints ONCE into the
/// transcript at turn start (a quiet "here we go" line) AND drives the animated thinking line in the
/// footer breather row (see [`thinking_line`]).
static VERB_CURSOR: AtomicUsize = AtomicUsize::new(0);
/// Index (into `VERBS`) of the verb chosen for the CURRENT turn — set by [`next_work_verb`] at turn
/// start and read every frame by [`paint_box`] so the footer shimmer keeps showing the same word for
/// the whole turn (rather than re-rolling on each ~9×/s repaint).
static CURRENT_VERB: AtomicUsize = AtomicUsize::new(0);

/// The next working verb (e.g. "Pondering"), advancing the rotation AND pinning it as the current
/// turn's verb (so the animated footer line shows the same word). Emitted once per turn into the
/// scrolling transcript by the REPL — see the turn-start line in `run_menu_sticky`.
pub fn next_work_verb() -> &'static str {
    let i = VERB_CURSOR.fetch_add(1, Ordering::Relaxed) % VERBS.len();
    CURRENT_VERB.store(i, Ordering::Relaxed);
    VERBS[i]
}

/// The verb pinned for the current turn (the one [`next_work_verb`] last returned) — read by the
/// footer's animated thinking line so it stays stable across repaints.
fn current_verb() -> &'static str {
    VERBS[CURRENT_VERB.load(Ordering::Relaxed) % VERBS.len()]
}

/// Context-window fill, in per-mille (0..=1000), for the HUD meter bar. Set by `status_text` each
/// time the status is refreshed; read by `paint_box` to draw the coloured bar. Per-mille (not
/// percent) so the bar has sub-1% resolution without a float in the hot paint path.
static CTX_PERMILLE: AtomicU16 = AtomicU16::new(0);

/// Update the context-meter fill (per-mille, clamped 0..=1000). Called from `status_text` alongside
/// each status refresh; harmless when the TUI is inactive.
pub fn set_ctx_permille(v: u16) {
    CTX_PERMILLE.store(v.min(1000), Ordering::Relaxed);
}

/// Current spinner frame index (advanced by the ticker thread; read by `paint_box`).
static WORK_FRAME: AtomicUsize = AtomicUsize::new(0);
/// Rough count of streamed OUTPUT characters this turn (÷4 ≈ tokens) — drives the live "↑N tok"
/// counter in the working pill. Bumped by the streaming client via [`add_stream_chars`]; zeroed at
/// each turn start (`set_working(true)`).
static STREAM_CHARS: AtomicU64 = AtomicU64::new(0);
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

/// Bump the streamed-output character counter (≈ tokens ÷ 4) — called by the streaming client per
/// content delta so the working pill shows live progress. A cheap relaxed add; harmless off-TTY.
pub fn add_stream_chars(n: u64) {
    STREAM_CHARS.fetch_add(n, Ordering::Relaxed);
}

/// Estimated streamed OUTPUT tokens this turn (chars ÷ 4) for the working pill's `↑N tok`.
fn stream_tokens() -> u64 {
    STREAM_CHARS.load(Ordering::Relaxed) / 4
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
    // The HUD is `model  ·  🎭 Persona  ·  ✦ mode  ·  todos` — chips are separated by "  ·  ".
    // Colour each SEGMENT on its own so persona + mode can both pop at once (the old single-split
    // version only lit the first chip and left the rest muted). A segment's leading glyph picks its
    // colour; everything else stays neutral moonlight-grey.
    const SEP: &str = "  ·  ";
    s.split(SEP)
        .map(|seg| {
            if seg.starts_with('⚡') {
                theme::warn(seg).to_string() // yolo → gold (runs hot)
            } else if seg.starts_with('✦') {
                theme::warn(seg).bold().to_string() // ultimate → gold bold (runs hottest)
            } else if seg.starts_with('◆') || seg.starts_with('🎭') {
                // smart mode + persona chip → calm moonlight (careful mode / which character is live)
                theme::accent(seg).to_string()
            } else {
                theme::muted(seg).to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(&theme::faint(SEP).to_string())
}

/// Number of filled cells the context meter uses (excludes the enclosing brackets). Kept small so
/// the whole `⟦▓▓▓░░░⟧ 42%` widget is a compact right-hand gauge, not a full-width progress bar.
const CTX_BAR_CELLS: usize = 10;

/// Render the context-window meter: a bracketed 10-cell bar that fills as the session grows, plus a
/// trailing `NN%`. The fill colour tracks headroom — calm moonlight while there's plenty of room,
/// warm gold as it tightens, salmon when nearly full — so a glance reads "how much context is left"
/// without parsing a number. `permille` is 0..=1000 (see [`set_ctx_permille`]). Returns a styled
/// string sized to exactly `CTX_BAR_CELLS + 2` bracket cells + " NN%" (≤ 4) so callers can budget it.
fn ctx_meter(permille: u16) -> String {
    let pm = permille.min(1000);
    let filled = (pm as usize * CTX_BAR_CELLS).div_ceil(1000).min(CTX_BAR_CELLS);
    // A partial cell for the boundary so low fills still show a sliver of progress (▏..█ eighths).
    let bar_color = if pm >= 900 {
        theme::ERR // nearly full — reclaim room soon (/compact)
    } else if pm >= 700 {
        theme::WARN // getting tight
    } else {
        theme::ACCENT_DIM // plenty of headroom — quiet moonlight
    };
    let mut bar = String::new();
    for i in 0..CTX_BAR_CELLS {
        bar.push(if i < filled { '▓' } else { '░' });
    }
    let pct = (pm as f64 / 10.0).round() as u16; // per-mille → percent
    format!(
        "{}{}{} {}",
        theme::faint("⟦"),
        style(bar).color256(bar_color),
        theme::faint("⟧"),
        theme::muted(format!("{pct}%")),
    )
}

/// Render the animated "thinking" line for the footer breather row while the agent works: the
/// current turn's verb (e.g. `Pondering`) with a bright moonlight band that sweeps left→right across
/// its letters (a shimmer), a leading star frame, and trailing dots that grow `.`→`..`→`···`. Driven
/// entirely by `WORK_FRAME` (advanced ~9×/s by the ticker) so it animates smoothly even when no token
/// is streaming. Returns `""` when the verb is empty so the caller can leave the row blank when idle.
///
/// The shimmer is a moving 3-cell-wide window: letters inside it render bright `ACCENT` (bold at the
/// crest), the rest a quiet `MUTED`, so a soft highlight glides across the word like moonlight on
/// water — matching the palette's "holds the moon" identity without adding a new colour.
fn thinking_line(frame: usize) -> String {
    let verb = current_verb();
    let chars: Vec<char> = verb.chars().collect();
    let n = chars.len();
    if n == 0 {
        return String::new();
    }
    // The shimmer crest sweeps across [0, n + tail) so it enters, crosses, and exits the word before
    // wrapping — a slow, continuous glide (one cell every ~2 frames ≈ 5 cells/s).
    let span = n + 6;
    let crest = (frame / 2) % span;
    let mut out = String::from("  "); // indent so it sits under the transcript column, not the rule
    for (i, &c) in chars.iter().enumerate() {
        // Distance from the crest, wrapping so the band re-enters cleanly at the left edge.
        let d = (i as isize - crest as isize).unsigned_abs();
        let styled = if d == 0 {
            style(c.to_string()).color256(ACCENT).bold() // crest — brightest
        } else if d == 1 {
            style(c.to_string()).color256(theme::ACCENT_DIM) // shoulder — soft glow
        } else {
            style(c.to_string()).color256(theme::MUTED) // trough — quiet
        };
        out.push_str(&styled.to_string());
    }
    // Trailing dots grow 0→3 then repeat, on a slower clock than the shimmer so they read as a pulse.
    let dots = (frame / 4) % 4;
    out.push_str(&theme::faint(".".repeat(dots)).to_string());
    out
}

/// Visible (de-styled) width of the context meter for a given fill — brackets + bar + " NN%".
/// `paint_box` uses this to reserve the meter's right-hand slot before truncating the status text.
fn ctx_meter_width(permille: u16) -> usize {
    let pct = (permille.min(1000) as f64 / 10.0).round() as u16;
    // ⟦ + bar + ⟧ + space + digits + %
    1 + CTX_BAR_CELLS + 1 + 1 + pct.to_string().len() + 1
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

    // row 2: HUD — muted "model · ✦ mode" on the LEFT; a compact state pill + the always-on context
    // meter on the RIGHT. The verb ("Pondering…") no longer animates here — it prints once into the
    // transcript at turn start — so this row stays quiet: state, liveness numbers, and the ctx gauge.
    goto(buf, top_row + 1, 1);
    clear_line(buf);
    let pm = CTX_PERMILLE.load(Ordering::Relaxed);
    let meter = ctx_meter(pm);
    let meter_w = ctx_meter_width(pm);
    let state = if WORKING.load(Ordering::Relaxed) {
        let frame = STAR[WORK_FRAME.load(Ordering::Relaxed) % STAR.len()];
        let secs = work_elapsed_secs();
        let tok = stream_tokens();
        let toktail = if tok >= 1000 {
            format!(" · ↑{:.1}K tok", tok as f64 / 1000.0)
        } else if tok > 0 {
            format!(" · ↑{tok} tok")
        } else {
            String::new()
        };
        format!(
            "{} {}",
            style(frame).color256(ACCENT).bold(),
            theme::faint(format!("{secs}s{toktail} · Esc"))
        )
    } else {
        format!("{} {}", theme::ok("●"), theme::faint("ready"))
    };
    // Right cluster = state pill + a gap + the context meter, right-aligned to the window edge.
    let right = format!("{state}   {meter}");
    let right_w = measure_text_width(&state) + 3 + meter_w;
    // Bound the status so the right cluster always fits and the line can never wrap onto a second
    // row (a wrapped HUD is how the footer visually "doubles").
    let avail = w.saturating_sub(right_w + 3);
    let status = truncate_to_width(&r.status, avail);
    let status_styled = style_hud(&status);
    // Pad between the (plain-width) status and the right cluster so the meter hugs the right edge.
    let pad = w
        .saturating_sub(measure_text_width(&status) + right_w)
        .max(1);
    buf.push_str(&format!("{status_styled}{}{right}", " ".repeat(pad)));

    // row 3: while the agent works this is the animated "thinking" line — the turn's verb with a
    // moonlight shimmer sweeping across its letters (driven by WORK_FRAME, repainted ~9×/s by the
    // ticker). Idle → a blank breather between the HUD and the prompt (the airy mockup spacing).
    goto(buf, top_row + 2, 1);
    clear_line(buf);
    if WORKING.load(Ordering::Relaxed) {
        let line = thinking_line(WORK_FRAME.load(Ordering::Relaxed));
        buf.push_str(&line);
    }

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
        STREAM_CHARS.store(0, Ordering::Relaxed); // fresh token counter for this turn
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

// ── the animated `/effort` slider ─────────────────────────────────────────────
// A keyboard-dragged horizontal slider for the per-turn reasoning-effort tier. Four discrete stops
// (`auto` · `low` · `medium` · `high`) sit on a rail; a moonlit knob slides between them with an
// ease-out glide (the "kéo"/drag feel), and a small pulse plays on commit. Colour keys the mood —
// auto is calm moonlight, low goes green (light & cheap), medium dim-silver, high burns the reserved
// gold (runs hot). Runs while the sticky box is SUSPENDED (or in the plain REPL), so it owns stdin;
// degrades to a no-op (returns `None`) off-TTY. The caller maps the returned index to config writes.

/// Rail inner width in cells (index range `0..=RAIL`).
const RAIL: usize = 39;
/// Cell position of each tier's notch on the rail — evenly spaced across `0..=RAIL`.
const NOTCHES: [usize; 6] = [0, 8, 16, 23, 31, 39];
/// Stop labels, left→right. Index is the value returned by [`effort_slider`].
const E_TIERS: [&str; 6] = ["auto", "low", "medium", "high", "xhigh", "max"];
/// One-line gist shown under the focused stop.
const E_DESCS: [&str; 6] = [
    "detect per-turn from your wording — keyword + complexity",
    "minimal reasoning — fastest & cheapest",
    "balanced reasoning — the middle ground",
    "deep reasoning — the everyday ceiling",
    "deeper exploration — always thinks deeply",
    "no limit on thinking depth — slowest & most thorough",
];
/// Rows the slider block occupies (title · blank · rail · labels · blank · desc · hint).
const SLIDER_ROWS: usize = 7;

/// The moonlight-palette colour for a tier: auto = accent, low = green (ok), medium = dim silver,
/// high/xhigh = the reserved warm gold (matches the `⚡ yolo` "runs hot" cue), max = salmon (the
/// hottest rung). high and xhigh share the gold; the label text distinguishes them.
fn e_color(i: usize) -> u8 {
    match i {
        1 => theme::OK,
        2 => theme::ACCENT_DIM,
        3 => theme::WARN,
        4 => theme::WARN,
        5 => theme::ERR,
        _ => theme::ACCENT,
    }
}

/// Build the labels row: each stop centred on its notch, the focused one bold-tinted, the rest faint.
/// Contiguous cells of the same owner are grouped into one styled span (so the plain names survive as
/// substrings and the escape count stays small).
fn labels_line(sel: usize) -> String {
    let mut owner = [usize::MAX; RAIL + 1];
    let mut chars = [' '; RAIL + 1];
    for (li, name) in E_TIERS.iter().enumerate() {
        let w = name.chars().count();
        let mut start = NOTCHES[li].saturating_sub(w / 2);
        if start + w > RAIL + 1 {
            start = RAIL + 1 - w; // clamp the rightmost label so it can't overflow the rail
        }
        for (k, ch) in name.chars().enumerate() {
            chars[start + k] = ch;
            owner[start + k] = li;
        }
    }
    let mut out = String::new();
    let mut c = 0;
    while c <= RAIL {
        let o = owner[c];
        let mut seg = String::new();
        while c <= RAIL && owner[c] == o {
            seg.push(chars[c]);
            c += 1;
        }
        if o == usize::MAX {
            out.push_str(&seg);
        } else if o == sel {
            out.push_str(&style(seg).color256(e_color(sel)).bold().to_string());
        } else {
            out.push_str(&theme::faint(seg).to_string());
        }
    }
    out
}

/// Render one frame of the slider: `sel` = focused tier (colours the fill + labels + desc), `knob` =
/// the knob's current rail cell (may sit *between* notches mid-glide), `glyph` = the knob character
/// (swapped during the commit pulse). Produces exactly `SLIDER_ROWS` lines joined by `\n` (no trailing
/// newline); every line begins with a clear-to-EOL so an in-place redraw leaves no residue.
fn slider_frame(sel: usize, knob: usize, glyph: &str) -> String {
    let col = e_color(sel);
    let mut out = String::new();
    // 1) title
    out.push_str("\x1b[2K");
    out.push_str(&theme::muted("  reasoning effort").to_string());
    out.push('\n');
    // 2) blank
    out.push_str("\x1b[2K\n");
    // 3) rail — filled (tinted) up to the knob, faint beyond it
    out.push_str("\x1b[2K  ");
    for c in 0..=RAIL {
        if c == knob {
            out.push_str(&style(glyph).color256(col).bold().to_string());
        } else if c < knob {
            out.push_str(&style("━").color256(col).to_string());
        } else {
            out.push_str(&theme::faint("─").to_string());
        }
    }
    out.push('\n');
    // 4) labels
    out.push_str("\x1b[2K  ");
    out.push_str(&labels_line(sel));
    out.push('\n');
    // 5) blank
    out.push_str("\x1b[2K\n");
    // 6) description of the focused stop
    out.push_str("\x1b[2K  ");
    out.push_str(&style(format!("› {}", E_DESCS[sel])).color256(col).to_string());
    out.push('\n');
    // 7) key hints
    out.push_str("\x1b[2K  ");
    out.push_str(&theme::faint("← → drag · Enter set · Esc cancel").to_string());
    out
}

/// Reprint the block in place: jump the cursor up to the block's top row, then repaint every line
/// (each clears itself) and drop back below it.
fn slider_redraw(frame: &str) {
    println!("\x1b[{SLIDER_ROWS}A{frame}");
    let _ = std::io::stdout().flush();
}

/// Glide the knob from one notch to another with an ease-out cubic (fast start, gentle settle) — the
/// dragging animation. `to` is the destination tier, so the fill/labels recolour to it as it moves.
fn slider_glide(from: usize, to: usize) {
    const FRAMES: usize = 7;
    let (a, b) = (NOTCHES[from] as f32, NOTCHES[to] as f32);
    for f in 1..=FRAMES {
        let t = f as f32 / FRAMES as f32;
        let e = 1.0 - (1.0 - t).powi(3); // ease-out
        let cell = (a + (b - a) * e).round() as usize;
        slider_redraw(&slider_frame(to, cell, "●"));
        std::thread::sleep(Duration::from_millis(16));
    }
}

/// A short pulse on the knob when the choice is committed (a little "click" of feedback).
fn slider_commit_pulse(sel: usize) {
    for g in ["●", "◉", "●", "◉", "●"] {
        slider_redraw(&slider_frame(sel, NOTCHES[sel], g));
        std::thread::sleep(Duration::from_millis(45));
    }
}

/// Run the interactive effort slider, starting focused on `start` (0=auto … 3=high). Returns the
/// chosen index, or `None` if the user cancelled (Esc) or it isn't a TTY. Drives stdin directly, so
/// the caller must have SUSPENDED the sticky box first (the plain REPL can call it as-is).
pub fn effort_slider(start: usize) -> Option<usize> {
    if !std::io::stdout().is_terminal() {
        return None;
    }
    let term = Term::stdout();
    let _ = term.hide_cursor();
    let mut sel = start.min(E_TIERS.len() - 1);
    println!("{}", slider_frame(sel, NOTCHES[sel], "●"));
    let _ = std::io::stdout().flush();
    let choice = loop {
        let key = match term.read_key() {
            Ok(k) => k,
            Err(_) => break None,
        };
        match key {
            Key::ArrowRight | Key::Char('l') | Key::Char('L') if sel < E_TIERS.len() - 1 => {
                slider_glide(sel, sel + 1);
                sel += 1;
            }
            Key::ArrowLeft | Key::Char('h') | Key::Char('H') if sel > 0 => {
                slider_glide(sel, sel - 1);
                sel -= 1;
            }
            Key::Enter => {
                slider_commit_pulse(sel);
                break Some(sel);
            }
            Key::Escape | Key::Char('\u{3}') | Key::Char('\u{4}') => break None,
            _ => {}
        }
    };
    let _ = term.show_cursor();
    println!();
    choice
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
        // every star frame must measure as one cell so the right-edge pill stays aligned
        for f in STAR {
            assert_eq!(measure_text_width(f), 1, "{f:?} must be a single cell");
        }
    }

    #[test]
    fn tips_are_nonempty_one_line_and_rotate() {
        // Every tip must be a single non-empty line (they render on one dim row under the message).
        assert!(!TIPS.is_empty(), "there must be at least one tip");
        for t in TIPS {
            assert!(!t.trim().is_empty(), "a tip must not be blank");
            assert!(!t.contains('\n'), "a tip must be a single line: {t:?}");
        }
        // The rotation cursor advances by one per pull, wrapping the set — consecutive pulls index
        // consecutive tips (modulo the seed's current value, which sibling tests may have bumped).
        let base = TIP_SEED.load(Ordering::Relaxed);
        let a = TIPS[base % TIPS.len()];
        let b = TIPS[(base + 1) % TIPS.len()];
        assert_eq!(TIPS[TIP_SEED.fetch_add(1, Ordering::Relaxed) % TIPS.len()], a);
        assert_eq!(TIPS[TIP_SEED.fetch_add(1, Ordering::Relaxed) % TIPS.len()], b);
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

    #[test]
    fn ctx_meter_fills_proportionally_and_reports_percent() {
        // Empty session → no filled cells; the percent reads 0.
        let empty = ctx_meter(0);
        assert_eq!(empty.matches('▓').count(), 0, "0‰ → no filled cells");
        assert_eq!(empty.matches('░').count(), CTX_BAR_CELLS, "0‰ → all empty cells");
        assert!(empty.contains("0%"));
        // Half full → about half the cells filled, and "50%".
        let half = ctx_meter(500);
        assert_eq!(half.matches('▓').count(), CTX_BAR_CELLS / 2, "500‰ → half filled");
        assert!(half.contains("50%"));
        // Full → every cell filled, "100%".
        let full = ctx_meter(1000);
        assert_eq!(full.matches('▓').count(), CTX_BAR_CELLS, "1000‰ → all filled");
        assert!(full.contains("100%"));
        // A non-zero-but-tiny fill still lights at least one cell (div_ceil), so progress is visible.
        assert_eq!(ctx_meter(1).matches('▓').count(), 1, "any non-zero fill shows ≥1 cell");
    }

    #[test]
    fn ctx_meter_width_matches_rendered_visible_width() {
        // The reserved width must equal the de-styled visible width for every fill, so paint_box's
        // right-alignment math is exact (an off-by-one here wraps the HUD → the footer "doubles").
        for pm in [0u16, 1, 99, 100, 500, 999, 1000] {
            let rendered = ctx_meter(pm);
            // Strip ANSI: measure_text_width already ignores escapes, so compare directly.
            assert_eq!(
                measure_text_width(&rendered),
                ctx_meter_width(pm),
                "meter width mismatch at {pm}‰"
            );
        }
    }

    #[test]
    fn work_verb_rotation_advances_and_wraps() {
        // Successive pulls walk the VERBS list (modulo the shared cursor other tests may have bumped).
        let base = VERB_CURSOR.load(Ordering::Relaxed);
        let a = VERBS[base % VERBS.len()];
        let b = VERBS[(base + 1) % VERBS.len()];
        assert_eq!(next_work_verb(), a);
        assert_eq!(next_work_verb(), b);
    }

    #[test]
    fn thinking_line_animates_and_keeps_the_verb_intact() {
        // Pin a known verb, then render several frames. The word's letters must all survive every
        // frame (the shimmer only re-colours them, never drops any), and successive frames must
        // differ (the crest sweeps + the dots pulse) so the line actually animates.
        next_work_verb();
        let verb = current_verb();
        let stripped = |f: usize| console::strip_ansi_codes(&thinking_line(f)).to_string();
        // Every rendered frame contains the whole verb (letters may be individually styled).
        for f in 0..12 {
            let vis = stripped(f);
            assert!(
                vis.contains(verb),
                "frame {f} dropped the verb: {vis:?} (want {verb:?})"
            );
        }
        // The shimmer/dots move: at least two of the first several frames must render differently.
        let frames: Vec<String> = (0..8).map(|f| thinking_line(f)).collect();
        assert!(
            frames.iter().any(|f| f != &frames[0]),
            "thinking line never changed across frames — no animation"
        );
    }

    #[test]
    fn style_hud_preserves_every_chip_and_separator() {
        // The HUD text must survive styling verbatim (colours may be stripped under the test harness,
        // but the glyphs/labels always remain) — persona + mode chips coexist without one clobbering
        // the other, and the "  ·  " separators are kept so the row reads the same shape.
        let hud = "gpt-model  ·  🎭 Sherlock  ·  ✦ ultimate";
        let out = style_hud(hud);
        assert!(out.contains("gpt-model"), "model label kept");
        assert!(out.contains("🎭 Sherlock"), "persona chip kept");
        assert!(out.contains("✦ ultimate"), "mode chip kept alongside persona");
        assert_eq!(out.matches('·').count(), 2, "both separators kept");
        // A plain status (no chips) is passed through unchanged in content.
        assert!(style_hud("just-a-model").contains("just-a-model"));
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
    fn slider_frame_has_all_stops_and_bounded_rows() {
        // A frame must name every tier, describe the focused one, carry the knob glyph, and be
        // exactly SLIDER_ROWS lines (the redraw jumps up by that count — a mismatch smears the UI).
        let frame = slider_frame(2, NOTCHES[2], "●");
        for t in E_TIERS {
            assert!(frame.contains(t), "frame must show the '{t}' label");
        }
        assert!(frame.contains(E_DESCS[2]), "frame shows the focused tier's description");
        assert!(frame.contains('●'), "frame carries the knob glyph");
        assert_eq!(frame.lines().count(), SLIDER_ROWS, "frame must be exactly SLIDER_ROWS lines");
    }

    #[test]
    fn slider_notches_span_the_rail_in_order() {
        // The notches must be sorted, start at 0, end at RAIL, and match the tier count — otherwise
        // the knob would jump off the rail or land between labels.
        assert_eq!(NOTCHES.len(), E_TIERS.len(), "one notch per tier");
        assert_eq!(NOTCHES[0], 0, "first stop sits at the rail start");
        assert_eq!(*NOTCHES.last().unwrap(), RAIL, "last stop sits at the rail end");
        assert!(NOTCHES.windows(2).all(|w| w[0] < w[1]), "notches strictly ascend");
    }

    #[test]
    fn labels_line_contains_every_tier_name() {
        // Every stop's name must survive as a plain substring regardless of which is focused, so the
        // label row always reads correctly (the styling groups spans but never splits a name).
        for sel in 0..E_TIERS.len() {
            let line = labels_line(sel);
            for t in E_TIERS {
                assert!(line.contains(t), "labels row (sel={sel}) must contain '{t}'");
            }
        }
    }

    #[test]
    fn e_color_maps_each_tier_to_a_palette_role() {
        // auto→accent, low→ok(green), medium→dim, high→warn(gold). Guards the "high runs hot" cue.
        assert_eq!(e_color(0), theme::ACCENT);
        assert_eq!(e_color(1), theme::OK);
        assert_eq!(e_color(2), theme::ACCENT_DIM);
        assert_eq!(e_color(3), theme::WARN);
    }

    #[test]
    fn submission_variants_roundtrip() {
        // The REPL classifies on these — guard the shape.
        let s = Submission::Chat("hi".into(), vec!["data:...".into()]);
        assert_eq!(s, Submission::Chat("hi".into(), vec!["data:...".into()]));
        assert_ne!(Submission::Quit, Submission::Slash("help".into()));
    }
}
