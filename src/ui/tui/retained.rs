//! Retained full-frame terminal backend — the only chat surface aizen has. (The classic sticky
//! renderer this once fell back to was removed; there is no rollback path to keep working.)
//!
//! ONE render thread owns the alternate screen. Every other thread only sends [`Command`]s, which is
//! what makes the rest of the program free to print from anywhere without tearing a frame.
//!
//! This file holds the state ([`AppState`]), the command enum, and the loop that applies one to the
//! other. The rest is split by concern into the submodules below.

use crossterm::cursor::{Hide, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{
    Block as FrameBlock, Borders, Clear, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState, Wrap,
};
use ratatui::{Frame, Terminal};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{self, IsTerminal, Stdout, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use super::HealthKind;

mod metrics;

// The backend split by concern; `retained.rs` keeps the state, the command enum and the render
// loop that ties them together. Re-exported at this level because callers outside (`ui::tui`) have
// always addressed these as `retained::<name>`, and a file split must not change that.
mod ansi;
mod api;
mod geom;
mod paint;
mod widgets;

pub(super) use self::ansi::*;
pub(super) use self::api::*;
pub(super) use self::geom::*;
use self::paint::*; // all internal to the backend — nothing here is addressed from `ui::tui`
pub(super) use self::widgets::*;

#[cfg(test)]
mod tests;

/// Smallest footer that can be painted at all: HUD + rule + one text row + rule. Below this the
/// composer is skipped entirely rather than drawn half-formed.
const FOOTER_ROWS: u16 = 4;
/// The footer's fixed furniture — the HUD strip and the two framing rules. Everything on top of
/// this is composer text, which is why the footer's height is decided per frame rather than fixed.
const FOOTER_CHROME_ROWS: u16 = 3;
/// Ceiling on how tall the composer may grow. A long prompt wraps DOWN instead of scrolling
/// sideways out of sight, but past this many rows a pasted wall of text would become the screen.
const MAX_INPUT_TEXT_ROWS: u16 = 10;
const CACHE_LIMIT: usize = 512;
const BLOCK_LIMIT: usize = 2048;

static ACTIVE: AtomicBool = AtomicBool::new(false);
static COLS: AtomicU16 = AtomicU16::new(80);
static ROWS: AtomicU16 = AtomicU16::new(24);

#[derive(Clone, Default)]
pub(super) struct OverlaySnapshot {
    pub title: String,
    pub lines: Vec<String>,
    pub selected: Option<usize>,
    pub hint: String,
}

#[derive(Clone, Default)]
pub(super) struct InputSnapshot {
    pub draft: Vec<char>,
    pub cursor: usize,
    pub images: usize,
    pub status: String,
    pub queued_count: usize,
    pub overlay: Option<OverlaySnapshot>,
    /// Mouse highlight inside the input box as `(anchor, cursor)` draft char indices — the drag's
    /// start and where it currently is, in either order. Painted REVERSED; normalised through
    /// [`super::normalized_draft_sel`] so a bare click (anchor == cursor) is no selection at all.
    pub sel: Option<(usize, usize)>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockKind {
    Intro,
    Generic,
    Assistant,
    /// A single tool-call line: `⚙ <name>   <target>            <digest>` — the digest is
    /// right-aligned by the render width and tinted by [`ToolState`]. Created at call start
    /// (state `Running`, empty digest) and updated in place when the result lands.
    Tool,
    /// The in-place task checklist box (`☑ done/total · plan` + ✓/▸/○ rows). One stable block per
    /// session, replaced (not appended) on each `todo_write`.
    Plan,
    /// A boxed unified-diff preview (`diff · <path>` + `+`/`−` rows).
    Diff,
    /// A green verify-gate success line (`✓ <cmd> — <detail>`).
    Verify,
}

/// Outcome state for a [`ToolEvent`] — drives the digest colour (running = dim, ok = green,
/// err = salmon).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum ToolState {
    Running,
    Ok,
    Err,
}

/// A structured tool-call line. Rendered as `<icon> <name>   <target>` on the call line, with the
/// result `digest` on an indented `└` line below it (tinted by [`ToolState`] and carrying the run
/// time). `seq` lets the result update the same block in place (parallel batches update by seq).
#[derive(Clone)]
pub(super) struct ToolEvent {
    pub seq: u64,
    pub icon: String,
    pub name: String,
    pub target: String,
    pub digest: String,
    pub state: ToolState,
    /// Wall-clock run time of the tool call in milliseconds, appended to the result line (`· 1.2s`).
    /// `None` when unknown (restored transcripts, parallel eager-adoption) → no time is shown.
    pub elapsed_ms: Option<u64>,
}

/// One checklist row for the plan panel. `status`: 0 = pending (○), 1 = in-progress (▸), 2 = done (✓).
#[derive(Clone)]
pub(super) struct PlanRow {
    pub status: u8,
    pub text: String,
}

/// A boxed diff preview: the edited path plus the `+`/`−` lines (`is_add`, content, already clipped
/// upstream). `adds`/`dels` are the full counts for the header even when `lines` is truncated.
#[derive(Clone)]
pub(super) struct DiffPayload {
    pub path: String,
    pub adds: usize,
    pub dels: usize,
    pub lines: Vec<(bool, String)>,
}

/// A verify-gate success line: `✓ <cmd> — <detail>` (e.g. `cargo check`, `0 errors · verify gate passed`).
#[derive(Clone)]
pub(super) struct VerifyPayload {
    pub cmd: String,
    pub detail: String,
}

/// The body of a transcript block. Text kinds (intro/generic/assistant) keep a raw string that is
/// re-wrapped per width; the structured kinds carry typed data the renderer lays out by width.
#[derive(Clone)]
enum Payload {
    Text(String),
    Tool(ToolEvent),
    Plan(Vec<PlanRow>),
    Diff(DiffPayload),
    Verify(VerifyPayload),
}

impl Payload {
    /// A stable hash of the payload for the render cache + frame metrics.
    fn content_hash(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        match self {
            Payload::Text(s) => s.hash(&mut h),
            Payload::Tool(t) => {
                t.name.hash(&mut h);
                t.target.hash(&mut h);
                t.digest.hash(&mut h);
                (t.state as u8).hash(&mut h);
                t.elapsed_ms.hash(&mut h);
            }
            Payload::Plan(rows) => {
                for r in rows {
                    r.status.hash(&mut h);
                    r.text.hash(&mut h);
                }
            }
            Payload::Diff(d) => {
                d.path.hash(&mut h);
                d.adds.hash(&mut h);
                d.dels.hash(&mut h);
                for (add, line) in &d.lines {
                    add.hash(&mut h);
                    line.hash(&mut h);
                }
            }
            Payload::Verify(v) => {
                v.cmd.hash(&mut h);
                v.detail.hash(&mut h);
            }
        }
        h.finish()
    }
}

#[derive(Clone)]
struct UiBlock {
    id: u64,
    kind: BlockKind,
    payload: Payload,
    complete: bool,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
struct CacheKey {
    id: u64,
    width: u16,
    hash: u64,
    complete: bool,
}

#[derive(Default)]
struct RenderCache {
    rows: HashMap<CacheKey, Vec<String>>,
    hits: u64,
    misses: u64,
}

impl RenderCache {
    fn get_or_render(&mut self, block: &UiBlock, width: u16) -> Vec<String> {
        let key = CacheKey {
            id: block.id,
            width,
            hash: block.payload.content_hash(),
            complete: block.complete,
        };
        if let Some(rows) = self.rows.get(&key) {
            self.hits += 1;
            return rows.clone();
        }
        self.misses += 1;
        let w = width as usize;
        let rows = match &block.payload {
            Payload::Text(s) => match block.kind {
                BlockKind::Assistant => render_assistant_rows(s, w),
                _ => sanitize_keep_sgr(s)
                    .split('\n')
                    .map(str::to_string)
                    .collect(),
            },
            Payload::Tool(t) => render_tool_row(t, w)
                .split('\n')
                .map(str::to_string)
                .collect(),
            Payload::Plan(rows) => render_plan_box(rows, w),
            Payload::Diff(d) => render_diff_box(d, w),
            Payload::Verify(v) => vec![render_verify_line(v, w)],
        };
        if self.rows.len() >= CACHE_LIMIT {
            self.rows.clear();
        }
        self.rows.insert(key, rows.clone());
        rows
    }
}

struct AppState {
    blocks: Vec<UiBlock>,
    next_id: u64,
    active_assistant: Option<u64>,
    input: InputSnapshot,
    working: bool,
    working_since: Option<Instant>,
    frame: usize,
    ctx_permille: u16,
    /// Live model health for the idle `● ready` chip (fed by `Command::Health`).
    health: HealthKind,
    /// Transcript scroll offset measured in wrapped lines UP from the bottom. `0` means "follow the
    /// tail" — the newest output stays pinned to the bottom as it streams. Any positive value means
    /// the user scrolled up to read; while scrolled up, new content arriving at the bottom must NOT
    /// drag the viewport (see `last_total` and the anchoring in `draw_transcript`).
    scroll_from_tail: usize,
    /// Wrapped-line count of the transcript at the last render. Used to keep the viewport anchored on
    /// the same content while the user is scrolled up and new lines stream in at the bottom: the
    /// offset is bumped by however many lines were appended, so what you're reading doesn't move.
    last_total: usize,
    /// Scroll offset (in lines from the top) for the informational overlay, when one is open. Kept
    /// separate from `scroll_from_tail` so PageUp/PageDown over an overlay never disturb the
    /// transcript behind it; reset on open/close and clamped to the overlay's own content height.
    overlay_scroll: usize,
    focused: bool,
    /// Block id of the in-place plan panel, if one has been shown this session. `todo_write` replaces
    /// that block's payload instead of appending a new one, so the checklist updates in place rather
    /// than stacking a fresh copy on every call. Cleared when the list is emptied.
    plan_id: Option<u64>,
    /// Absolute (line, col) selection in the flat wrapped-line space. Drawn reversed; cleared on a
    /// plain click outside the range / Esc.
    selection: Option<SelectionRange>,
    metrics: metrics::FrameMetrics,
    cache: RenderCache,
    /// When `Some(idx)`, the idle screensaver is up: the render thread cover-encodes card `idx` to its
    /// current pixel size, blits that sixel over the whole alt-screen, and skips the normal ratatui
    /// frame until it's cleared (`None`) — at which point the transcript is force-repainted. Set/cleared
    /// by `Command::Screensaver`.
    screensaver: Option<usize>,
    /// Cache of the last cover-encoded screensaver sixel, keyed by `(card_idx, px_w, px_h)`. The encode
    /// is ~tens of ms, so we reuse it across idle repaints and only re-encode when the card or the
    /// terminal's pixel geometry changes (e.g. a window resize while the screensaver is up).
    screensaver_cache: Option<(usize, u32, u32, String)>,
    /// Ultimate mode ON → the input box (prompt arrow + framing rules) recolours to the reserved gold,
    /// tying it to the `✦ ultimate` chip. Pushed via `Command::Ultimate` (never read from disk in the
    /// draw path — `cli_config::load()` hits the filesystem and this runs at ~9fps).
    ultimate: bool,
    /// The whimsical fallback verb ("Pondering") shown in the working caption BETWEEN tool steps — the
    /// "idle" half of the hybrid caption. Refreshed from `tui::next_work_verb()` when a turn starts and
    /// each time the last running tool completes, so successive quiet stretches surface fresh words.
    work_verb: String,
    /// The caption currently being typed out beside the working spinner. Either a running tool's action
    /// ("Reading retained.rs") or `work_verb` when nothing is running. Reset (`work_reveal = 0`) whenever
    /// this target changes so the typewriter re-runs on the new text.
    work_caption: String,
    /// How many chars of `work_caption` are revealed so far — advanced one per animation tick for the
    /// typewriter effect, clamped to the caption length.
    work_reveal: usize,
    /// First WRAPPED ROW of the draft visible in the composer at the last draw.
    ///
    /// The box grows downward with the draft, so this is only non-zero once the draft is taller than
    /// the box will go (`MAX_INPUT_TEXT_ROWS`, or what the terminal leaves after the transcript's
    /// floor). The window is STICKY: it slides only when the caret would otherwise fall off an edge.
    /// Re-deriving it from the caret every frame would pin the caret to the bottom row, so scrolling
    /// back up through a long paste would snap away on the next repaint.
    input_row_scroll: usize,
}

/// Absolute character selection over the flat list of wrapped transcript rows.
/// `line` is the absolute wrapped-line index; `col` is a display-cell offset within that row.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct SelectionRange {
    pub anchor_line: usize,
    pub anchor_col: usize,
    pub cursor_line: usize,
    pub cursor_col: usize,
}

impl AppState {
    fn new(intro: &str, status: &str) -> Self {
        Self {
            blocks: vec![UiBlock {
                id: 1,
                kind: BlockKind::Intro,
                payload: Payload::Text(sanitize_text(intro)),
                complete: true,
            }],
            next_id: 2,
            active_assistant: None,
            input: InputSnapshot {
                status: status.to_string(),
                ..InputSnapshot::default()
            },
            working: false,
            working_since: None,
            frame: 0,
            ctx_permille: 0,
            health: HealthKind::Unknown,
            scroll_from_tail: 0,
            last_total: 0,
            overlay_scroll: 0,
            focused: true,
            plan_id: None,
            selection: None,
            metrics: metrics::FrameMetrics::default(),
            cache: RenderCache::default(),
            screensaver: None,
            screensaver_cache: None,
            ultimate: false,
            work_verb: String::new(),
            work_caption: String::new(),
            work_reveal: 0,
            input_row_scroll: 0,
        }
    }

    /// Point the working caption at `text` (a tool action or the whimsical verb). Resets the reveal
    /// counter only when the text actually changes, so the typewriter replays on a new caption but a
    /// re-assert of the same one doesn't stutter back to the first character.
    fn set_work_caption(&mut self, text: String) {
        if self.work_caption != text {
            self.work_caption = text;
            self.work_reveal = 0;
        }
    }

    fn push_block(&mut self, kind: BlockKind, payload: Payload, complete: bool) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.blocks.push(UiBlock {
            id,
            kind,
            payload,
            complete,
        });
        if self.blocks.len() > BLOCK_LIMIT {
            let excess = self.blocks.len() - BLOCK_LIMIT;
            self.blocks.drain(0..excess);
        }
        // No scroll reset here: when the user is at the bottom (`scroll_from_tail == 0`) the tail is
        // followed automatically; when they've scrolled up to read, `draw_transcript` anchors on the
        // same content so newly-appended blocks don't yank the viewport down mid-read.
        id
    }

    /// A plain-text block (generic emit / intro). Convenience over [`push_block`] + [`Payload::Text`].
    fn push_text(&mut self, kind: BlockKind, content: String, complete: bool) -> u64 {
        self.push_block(kind, Payload::Text(content), complete)
    }

    fn push_assistant(&mut self, delta: &str) {
        let id = match self.active_assistant {
            Some(id) => id,
            None => {
                let id = self.push_block(BlockKind::Assistant, Payload::Text(String::new()), false);
                self.active_assistant = Some(id);
                id
            }
        };
        if let Some(block) = self.blocks.iter_mut().find(|b| b.id == id) {
            if let Payload::Text(s) = &mut block.payload {
                s.push_str(delta);
            }
            block.complete = false;
        }
        // Deliberately no `scroll_from_tail = 0`: a streaming token must not fight a user who has
        // scrolled up to read. Follow-at-bottom vs pinned-while-scrolled-up is handled at draw time.
    }

    fn finish_assistant(&mut self, interrupted: bool) {
        let Some(id) = self.active_assistant.take() else {
            return;
        };
        if let Some(block) = self.blocks.iter_mut().find(|b| b.id == id) {
            block.complete = true;
            if interrupted {
                if let Payload::Text(s) = &mut block.payload {
                    if !s.ends_with('\n') {
                        s.push('\n');
                    }
                }
            }
        }
    }

    /// Apply a tool-call event: a `Running` event with a fresh `seq` pushes a new tool block; an
    /// `Ok`/`Err` event with a `seq` we've seen updates that block in place (so the result lands on
    /// the same line instead of appending a second row). A result for an unknown seq (e.g. after
    /// pruning) just pushes a fresh completed row.
    fn apply_tool_event(&mut self, ev: ToolEvent) {
        if let Some(block) = self.blocks.iter_mut().find(|b| {
            b.kind == BlockKind::Tool && matches!(&b.payload, Payload::Tool(t) if t.seq == ev.seq)
        }) {
            block.complete = ev.state != ToolState::Running;
            block.payload = Payload::Tool(ev);
            return;
        }
        let complete = ev.state != ToolState::Running;
        self.push_block(BlockKind::Tool, Payload::Tool(ev), complete);
    }

    /// Replace the single in-place plan panel with the current checklist. The panel keeps a stable
    /// block id: the first `todo_write` pushes it, later calls update THAT block's payload in place
    /// (so the box refreshes where it first appeared instead of stacking a fresh copy on every call).
    /// An empty list removes the panel and forgets the id (a cleared plan leaves no empty box behind).
    fn apply_plan(&mut self, rows: Vec<PlanRow>) {
        if rows.is_empty() {
            if let Some(id) = self.plan_id.take() {
                self.blocks.retain(|b| b.id != id);
            }
            return;
        }
        if let Some(id) = self.plan_id {
            if let Some(block) = self.blocks.iter_mut().find(|b| b.id == id) {
                block.payload = Payload::Plan(rows);
                block.complete = true;
                return;
            }
            // The id was pruned out of the ring — fall through and push a fresh panel.
        }
        self.plan_id = Some(self.push_block(BlockKind::Plan, Payload::Plan(rows), true));
    }
}

enum Command {
    Emit(String),
    AssistantDelta(String),
    AssistantFinish {
        interrupted: bool,
    },
    /// A tool-call event: push a new line at call start (state `Running`), or update the existing
    /// line in place when the result lands (matched by `seq`).
    Tool(ToolEvent),
    /// Replace the in-place plan checklist box with a fresh snapshot (`todo_write`).
    Plan(Vec<PlanRow>),
    /// Push a boxed diff preview under the most recent edit.
    Diff(DiffPayload),
    /// Push a green verify-gate success line.
    Verify(VerifyPayload),
    Input(InputSnapshot),
    Working(bool),
    Status(String),
    /// Set the working caption target (a running tool's action, or a whimsical verb between steps).
    /// The typewriter reveal restarts whenever the text changes; a re-assert of the same text is a
    /// no-op so it doesn't stutter back to the first character. Passing an empty string clears it back
    /// to the whimsical `work_verb`.
    WorkCaption(String),
    /// Ultimate mode toggled — recolour the input box (gold ON, moonlight OFF). Pushed from the
    /// `/ultimate` handler and once at activation, never read from disk in the draw path.
    Ultimate(bool),
    Context(u16),
    /// Idle `● ready` chip colour/label — green/yellow/red based on the last `/models` probe.
    Health(HealthKind),
    Tick,
    OpenOverlay(OverlaySnapshot),
    /// Replace the OPEN overlay's body while preserving the reader's scroll position. Distinct from
    /// `OpenOverlay` on purpose: a live panel (`/workflows`) re-publishes itself about once a second,
    /// and re-opening would yank the view back to the top on every refresh — unreadable while you are
    /// paging through the history section. Ignored when no overlay is up, so a refresh that races the
    /// Esc that closed the panel can't resurrect it.
    UpdateOverlay(Vec<String>),
    CloseOverlay,
    Scroll(i32),
    /// Jump the transcript so absolute wrapped-line `start` is at the top of the viewport
    /// (used by scrollbar thumb drag).
    ScrollTo(usize),
    ScrollEnd,
    /// Idle screensaver / startup card: `Some(idx)` fills the alt-screen with feature card `idx`
    /// (encoded to the terminal's exact pixel size at draw time, cover-cropped — a raw DCS can't ride
    /// through the ratatui widget model, so it's blitted directly); `None` clears it and forces a full
    /// transcript repaint. Driven by the input thread (startup + 15s idle timer).
    Screensaver(Option<usize>),
    /// Set or replace the live mouse selection (rendered reversed).
    SetSelection(SelectionRange),
    /// Drop the current selection highlight.
    ClearSelection,
    Focus(bool),
    /// Throw away ratatui's belief about what is on screen and repaint every cell from the block
    /// buffer (Ctrl-L). The recovery hatch for the one failure ratatui's cell diff cannot see: text
    /// written to the terminal by something other than the render thread (a stray `println!`, a child
    /// process, a terminal glitch). The diff compares against its own last frame, so foreign text is
    /// never overwritten and survives inside later frames. `terminal.clear()` resets that belief.
    /// Not handled in `apply_command` — clearing needs the `TerminalSession`, which only the render
    /// loop holds.
    Redraw,
    Suspend(Sender<()>),
    Resume {
        status: String,
        ack: Sender<bool>,
    },
    Shutdown(Sender<()>),
}

/// The terminal's pixel dimensions, for encoding a card that fills the whole screen. Prefers the
/// terminal's reported pixel size; when a terminal doesn't report pixels (`width`/`height` = 0) it
/// falls back to the cell grid times a typical ~8×18 px monospace cell, so the fill is still full-size
/// (just at an assumed cell aspect) rather than degenerate.
fn terminal_pixels() -> (u32, u32) {
    match crossterm::terminal::window_size() {
        Ok(ws) if ws.width > 0 && ws.height > 0 => (ws.width as u32, ws.height as u32),
        _ => {
            let cols = COLS.load(Ordering::Relaxed) as u32;
            let rows = ROWS.load(Ordering::Relaxed) as u32;
            (cols * 8, rows * 18)
        }
    }
}

/// Cover-encode card `idx` to the terminal's pixel size (reusing the cached sixel when the card and
/// geometry are unchanged) and blit it fullscreen over the alt-screen, bypassing ratatui. Clears the
/// screen first so the transcript underneath is gone, homes the cursor, writes the DCS, then flushes.
/// Called only from the render thread (the sole owner of the alt-screen `Stdout`), so there is no
/// contention with `terminal.draw`. Best-effort: a degenerate size or an encode failure just leaves
/// the cleared screen (the next keystroke restores the transcript).
fn blit_screensaver(session: &mut TerminalSession, state: &mut AppState, idx: usize) {
    use crossterm::cursor::MoveTo;
    use crossterm::terminal::{Clear, ClearType};
    let (pw, ph) = terminal_pixels();
    let cols = COLS.load(Ordering::Relaxed).max(1);
    let rows = ROWS.load(Ordering::Relaxed).max(1);
    // Pixels-per-cell (integer). We encode to a WHOLE number of cells and place the sixel at a cell
    // origin, so it lands exactly where the terminal expects a character — no sub-cell drift.
    let cell_w = (pw / cols as u32).max(1);
    let cell_h = (ph / rows as u32).max(1);
    // Reserve a symmetric margin: 2 columns and 2 rows total (1 cell all around). This (a) centers the
    // image so any pixel-reporting slop splits evenly instead of piling up bottom-right, and (b) keeps
    // the sixel off the last row, which would otherwise scroll the alt-screen. The card is cover-cropped
    // so shrinking by one cell each edge is imperceptible.
    let grid_cols = (cols.saturating_sub(2)).max(1);
    let grid_rows = (rows.saturating_sub(2)).max(1);
    let enc_w = grid_cols as u32 * cell_w;
    let enc_h = grid_rows as u32 * cell_h;
    let col_off = (cols - grid_cols) / 2;
    let row_off = (rows - grid_rows) / 2;
    // Reuse the cached encode unless the card or the encoded geometry changed (resize while idle).
    let hit = matches!(&state.screensaver_cache, Some((ci, cw, ch, _)) if *ci == idx && *cw == enc_w && *ch == enc_h);
    if !hit {
        if let Some(sixel) = crate::ui::cards::render_cover_sixel(idx, enc_w, enc_h) {
            state.screensaver_cache = Some((idx, enc_w, enc_h, sixel));
        } else {
            state.screensaver_cache = None;
        }
    }
    let Some((_, _, _, sixel)) = &state.screensaver_cache else {
        // Encode failed — clear so we don't leave a stale transcript half-under a missing image.
        let out = session.terminal.backend_mut();
        let _ = execute!(out, Clear(ClearType::All), Hide);
        let _ = out.flush();
        return;
    };
    // Reuse the backend's own writer so we share its flushed state (no second Stdout handle racing).
    let out = session.terminal.backend_mut();
    // Clear removes the transcript; `Hide` re-hides the cursor (the last `terminal.draw` before the
    // screensaver came up positioned + showed it for the input line, so it would blink over the image);
    // `MoveTo` homes to the centred cell origin. The next keystroke force-redraws and restores both.
    let _ = execute!(out, Clear(ClearType::All), Hide, MoveTo(col_off, row_off));
    let _ = out.write_all(sixel.as_bytes());
    // Caption: the feature's name, dim + centred on the reserved bottom margin row (the sixel stops
    // one row short of it, so this never overlaps the image or scrolls the alt-screen). Skipped when
    // the title can't fit the width. Purely decorative — a failed write just leaves the bare image.
    if let Some(title) = crate::ui::cards::card_title(idx) {
        let len = title.chars().count() as u16;
        if len < cols {
            let cap_col = (cols - len) / 2;
            let cap_row = rows.saturating_sub(1);
            let _ = execute!(out, MoveTo(cap_col, cap_row));
            // Dim (SGR 2) so the caption reads as a subtle label, not a headline; reset after.
            let _ = out.write_all(b"\x1b[2m");
            let _ = out.write_all(title.as_bytes());
            let _ = out.write_all(b"\x1b[0m");
        }
    }
    let _ = out.flush();
}

fn render_loop(rx: Receiver<Command>, ready: Sender<bool>, intro: String, status: String) {
    let mut session = match TerminalSession::enter() {
        Ok(s) => Some(s),
        Err(_) => {
            let _ = ready.send(false);
            return;
        }
    };
    let mut state = AppState::new(&intro, &status);
    let _ = ready.send(true);
    let mut shutdown_ack: Option<Sender<()>> = None;
    let mut dirty = true;
    // Tracks whether the screensaver sixel is currently painted over the alt-screen. When it flips
    // off we must force ratatui to redraw every cell (the DCS blit wrote outside the frame model, so
    // ratatui's diff still believes the old transcript is on screen and would paint nothing).
    let mut screensaver_shown = false;
    // Tracks the working flag across ticks so we can re-assert terminal modes the instant a turn ends.
    // A tool that spawned a child (shell / MCP / LSP server) may have let it reset our console input
    // mode on Windows — clearing ENABLE_MOUSE_INPUT, which silently kills mouse capture so the wheel
    // leaks back through as ↑/↓ and the transcript stops scrolling. Re-emitting the mode setters is
    // idempotent and cheap; doing it on the true→false edge restores scroll without a per-frame cost.
    let mut was_working = false;
    // Set by `Command::Redraw`: clear the terminal before the next draw so ratatui's cell diff starts
    // from a blank slate instead of its stale belief about the screen.
    let mut force_clear = false;
    loop {
        let wait = if state.working && state.focused {
            Duration::from_millis(110)
        } else {
            Duration::from_millis(250)
        };
        match rx.recv_timeout(wait) {
            Ok(Command::Shutdown(ack)) => {
                shutdown_ack = Some(ack);
                break;
            }
            Ok(Command::Suspend(ack)) => {
                // Publish the size BEFORE giving up the screen: while suspended there is no frame to
                // ride an `autoresize` on, and whatever draws inside the menu reads these atomics.
                publish_terminal_size();
                session.take();
                let _ = ack.send(());
                dirty = false;
            }
            Ok(Command::Resume { status, ack }) => {
                state.input.status = status;
                let ok = match TerminalSession::enter() {
                    Ok(s) => {
                        session = Some(s);
                        true
                    }
                    Err(_) => false,
                };
                let _ = ack.send(ok);
                // Force a full clear, exactly as `Redraw` does. `enter()`'s `terminal.clear()` resets
                // ratatui's own diff baseline, but the SCREEN may hold bytes no baseline knows about:
                // a menu's leftover lines, or a raw print / spinner tick that landed during the
                // suspend window. Without this those cells are never overwritten and survive into
                // later frames — the "/config corrupted the layout" report.
                force_clear = ok;
                dirty = ok;
            }
            Ok(Command::Redraw) => {
                force_clear = true;
                dirty = true;
            }
            Ok(cmd) => {
                apply_command(&mut state, cmd);
                dirty = true;
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if state.working && state.focused {
                    state.frame = state.frame.wrapping_add(1);
                    // Advance the typewriter on idle ticks too, so the caption keeps typing during a
                    // quiet stretch (a long tool call) when no command arrives to drive it.
                    let len = state.work_caption.chars().count();
                    if state.work_reveal < len {
                        state.work_reveal += 1;
                    }
                    dirty = true;
                }
                // Idle resize: the render below only runs when `dirty`, so a terminal resized while
                // nothing is happening (no command) would otherwise never be repainted until the next
                // keystroke. Probe the real terminal size each tick and force a redraw when it moved —
                // no second stdin reader, just a cheap ioctl on this render thread.
                if session.is_some() && terminal_size_changed() {
                    dirty = true;
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                Command::Shutdown(ack) => {
                    shutdown_ack = Some(ack);
                    session.take();
                    if let Some(ack) = shutdown_ack.take() {
                        let _ = ack.send(());
                    }
                    return;
                }
                Command::Suspend(ack) => {
                    publish_terminal_size(); // see the blocking-recv arm above
                    session.take();
                    let _ = ack.send(());
                    dirty = false;
                }
                Command::Resume { status, ack } => {
                    state.input.status = status;
                    let ok = match TerminalSession::enter() {
                        Ok(s) => {
                            session = Some(s);
                            true
                        }
                        Err(_) => false,
                    };
                    let _ = ack.send(ok);
                    force_clear = ok; // see the blocking-recv arm above
                    dirty = ok;
                }
                Command::Redraw => {
                    force_clear = true;
                    dirty = true;
                }
                other => {
                    apply_command(&mut state, other);
                    dirty = true;
                }
            }
        }
        // Keep mouse capture pinned for the WHOLE working window, not just its trailing edge. A tool
        // can spawn a child process that resets our console input mode on Windows, silently dropping
        // mouse capture mid-turn; from then on the terminal's "alternateScroll" leaks the wheel through
        // as ↑/↓ keys, so scrolling walks input history instead of the transcript — the "sometimes the
        // wheel scrolls inside the chat box" bug. The reset happens at an arbitrary point during the
        // turn (whenever the child spawns), so an edge-only re-assert left the rest of the turn leaking.
        // Children only ever run while WORKING, so re-assert raw mode + mouse capture + cursor-hide on
        // every working tick (~110ms) and once more on the working→idle edge to catch the last child.
        // All three are idempotent — a terminal already in these modes ignores the repeat, and the
        // escapes are invisible mode-sets (no flicker, ~270 B/s while working). Idle ticks skip it
        // entirely: nothing resets the mode while the user is just reading, so there's no cost there.
        if session.is_some() && (state.working || was_working) {
            let _ = crossterm::terminal::enable_raw_mode();
            let _ = execute!(io::stdout(), EnableMouseCapture, Hide);
        }
        was_working = state.working;
        if dirty {
            if let Some(s) = session.as_mut() {
                let before = s.terminal.size().ok();
                let _ = s.terminal.autoresize();
                let after = s.terminal.size().ok();
                if let Some(area) = after {
                    COLS.store(area.width.max(20), Ordering::Relaxed);
                    ROWS.store(area.height.max(8), Ordering::Relaxed);
                }
                if let Some(idx) = state.screensaver {
                    // Screensaver up: cover-encode the card to the terminal's pixel size and blit it
                    // fullscreen, bypassing the ratatui frame (a DCS can't ride through the widget
                    // model). Clear first so the transcript underneath is gone, then blit.
                    blit_screensaver(s, &mut state, idx);
                    screensaver_shown = true;
                } else {
                    // Leaving the screensaver: the DCS wrote outside ratatui's cell buffer, so its
                    // diff still thinks the old transcript is on screen. Force a full redraw so the
                    // transcript is restored from scratch (no replay — blocks live in AppState).
                    if screensaver_shown {
                        let _ = s.terminal.clear();
                        screensaver_shown = false;
                    }
                    // Ctrl-L asked for a hard redraw: something wrote to the terminal behind our back
                    // (a stray raw print, a child process, a terminal-side glitch), so ratatui's cell
                    // diff is comparing against a screen that no longer exists and would repaint only
                    // the cells IT changed — leaving the foreign text embedded in later frames. Clear
                    // wipes the real screen and drops the diff baseline, so the next `draw` repaints
                    // every cell from `state.blocks`. Nothing is replayed: the transcript lives in
                    // AppState, not in the terminal.
                    if force_clear {
                        let _ = s.terminal.clear();
                        force_clear = false;
                    }
                    let started = Instant::now();
                    let _ = s.terminal.draw(|frame| draw(frame, &mut state));
                    // Inject OSC 8 hyperlinks AFTER terminal.draw() — post-draw so ratatui's cell
                    // diff never sees the escape sequences and can't overwrite them next frame.
                    // Pattern is identical to the screensaver sixel blitter (`blit_screensaver`).
                    {
                        // Snapshot occluders + caret BEFORE taking the geometry lock, so all four
                        // pieces describe the frame that was just drawn.
                        let occluders = last_occluders();
                        let caret = last_caret();
                        let g = transcript_geom_slot()
                            .lock()
                            .unwrap_or_else(|e| e.into_inner());
                        let ctx = crate::ui::links::InjectCtx {
                            start: g.start,
                            visible: g.visible,
                            area: g.area,
                            occluders,
                            caret,
                        };
                        let out = s.terminal.backend_mut();
                        crate::ui::links::inject_hyperlinks(out, &g.sgr_rows, &g.plain_rows, &ctx);
                    }
                    let rows = state
                        .blocks
                        .iter()
                        .map(|b| format!("{}:{:x}:{}", b.id, b.payload.content_hash(), b.complete))
                        .collect::<Vec<_>>();
                    state.metrics.record(
                        started.elapsed(),
                        metrics::hash_rows(&rows),
                        before != after,
                    );
                }
            }
            dirty = false;
        }
    }
    session.take();
    if let Some(ack) = shutdown_ack {
        let _ = ack.send(());
    }
}

fn apply_command(state: &mut AppState, cmd: Command) {
    match cmd {
        Command::Emit(s) => {
            // Keep SGR (colour/bold/dim) so the `❯` user echo, the `◆` tool anchor tinted by state,
            // and the green/salmon edit diff survive into styled spans — only cursor moves / erases
            // are stripped. `ansi_spans` turns the kept codes into ratatui spans at draw time.
            let clean = sanitize_keep_sgr(&s);
            if !clean.is_empty() {
                state.push_text(BlockKind::Generic, clean, true);
            }
        }
        Command::AssistantDelta(s) => state.push_assistant(&s),
        Command::AssistantFinish { interrupted } => state.finish_assistant(interrupted),
        Command::Tool(ev) => state.apply_tool_event(ev),
        Command::Plan(rows) => state.apply_plan(rows),
        Command::Diff(d) => {
            state.push_block(BlockKind::Diff, Payload::Diff(d), true);
        }
        Command::Verify(v) => {
            state.push_block(BlockKind::Verify, Payload::Verify(v), true);
        }
        Command::Input(input) => {
            // Preserve a non-empty HUD status if the snapshot arrives with an empty one
            // (classic shared Render can lag a frame behind `Command::Status` after activate).
            let keep_status = if input.status.is_empty() && !state.input.status.is_empty() {
                Some(state.input.status.clone())
            } else {
                None
            };
            state.input = input;
            if let Some(s) = keep_status {
                state.input.status = s;
            }
        }
        Command::Working(working) => {
            state.working = working;
            state.working_since = working.then(Instant::now);
            if working {
                // A fresh turn opens on a new whimsical verb, and the caption starts on it (typed from
                // scratch) until the first tool call renames it to a concrete action.
                state.work_verb = crate::ui::tui::next_work_verb().to_string();
                state.set_work_caption(state.work_verb.clone());
            } else {
                state.frame = 0;
                state.work_caption.clear();
                state.work_reveal = 0;
            }
        }
        Command::Status(status) => state.input.status = status,
        Command::WorkCaption(text) => {
            // Empty ⇒ fall back to the whimsical verb (a tool finished, nothing else running yet).
            let target = if text.is_empty() {
                state.work_verb.clone()
            } else {
                text
            };
            state.set_work_caption(target);
        }
        Command::Ultimate(on) => state.ultimate = on,
        Command::Context(v) => state.ctx_permille = v,
        Command::Health(h) => state.health = h,
        Command::Tick => {
            state.frame = state.frame.wrapping_add(1);
            // Type one more character of the working caption per tick (the typewriter). Clamped to the
            // caption length so a fully-revealed caption just holds with its blinking caret.
            let len = state.work_caption.chars().count();
            if state.work_reveal < len {
                state.work_reveal += 1;
            }
        }
        Command::OpenOverlay(overlay) => {
            state.input.overlay = Some(overlay);
            state.overlay_scroll = 0; // a fresh overlay starts at the top
        }
        Command::UpdateOverlay(lines) => {
            if let Some(o) = state.input.overlay.as_mut() {
                o.lines = lines;
                // `overlay_scroll` is deliberately untouched — `draw_overlay` re-clamps it against the
                // new content height, so a panel that shrank snaps to its last page instead of leaving
                // the viewport parked past the end.
            }
        }
        Command::CloseOverlay => {
            state.input.overlay = None;
            state.overlay_scroll = 0;
        }
        Command::Scroll(delta) => {
            // An open informational overlay owns scroll (PageUp/Down/Home/End move ITS content, not
            // the transcript sitting behind it). Clamp happens at draw time against the visible height.
            if state.input.overlay.is_some() {
                if delta < 0 {
                    state.overlay_scroll = state
                        .overlay_scroll
                        .saturating_add(delta.unsigned_abs() as usize);
                } else {
                    state.overlay_scroll = state.overlay_scroll.saturating_sub(delta as usize);
                }
            } else if delta < 0 {
                state.scroll_from_tail = state
                    .scroll_from_tail
                    .saturating_add(delta.unsigned_abs() as usize);
            } else {
                state.scroll_from_tail = state.scroll_from_tail.saturating_sub(delta as usize);
            }
        }
        Command::ScrollTo(start) => {
            // Invert resolve_transcript_scroll: start = tail_start - scroll_from_tail →
            // scroll_from_tail = total.saturating_sub(visible).saturating_sub(start).
            // `last_total` is used as a stand-in for total until the next draw re-clamps.
            let total = state.last_total;
            let visible = state
                .input
                .overlay
                .as_ref()
                .map(|_| 0)
                .unwrap_or(ROWS.load(Ordering::Relaxed).saturating_sub(FOOTER_ROWS) as usize);
            // Prefer the last stashed geometry when available (more accurate than ROWS estimate).
            let (geom_start, geom_visible, geom_total, _) = last_transcript_geom();
            let (total, visible) = if geom_total > 0 {
                (geom_total, geom_visible.max(1))
            } else {
                (total, visible.max(1))
            };
            let _ = geom_start;
            let tail_start = total.saturating_sub(visible);
            state.scroll_from_tail = tail_start.saturating_sub(start.min(tail_start));
        }
        Command::ScrollEnd => {
            // End/Home resets whichever surface is active: the open overlay, else the transcript.
            if state.input.overlay.is_some() {
                state.overlay_scroll = 0;
            } else {
                state.scroll_from_tail = 0;
            }
        }
        Command::Screensaver(card) => state.screensaver = card,
        Command::SetSelection(sel) => state.selection = Some(sel),
        Command::ClearSelection => {
            state.selection = None;
        }
        Command::Focus(v) => state.focused = v,
        // Lifecycle + `Redraw` carry no AppState change: they are handled by `render_loop`, which owns
        // the `TerminalSession` (`Redraw` sets `force_clear` so the next draw is unconditional).
        // Listed explicitly rather than via `_` so a new command can't silently become a no-op here.
        Command::Suspend(_) | Command::Resume { .. } | Command::Shutdown(_) | Command::Redraw => {}
    }
}
