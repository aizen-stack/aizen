//! Retained full-frame terminal backend.
//!
//! The legacy sticky implementation in the parent module remains the rollback path. This backend
//! owns the alternate screen from one render thread; every other thread only sends commands.

use crossterm::cursor::{Hide, Show};
use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
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

const FOOTER_ROWS: u16 = 4;
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
    /// Right-click "Copy" menu, when one is up. Opened only while a selection exists, so the menu
    /// can never offer to copy nothing.
    context_menu: Option<ContextMenu>,
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

/// Where the right-click "Copy" menu was asked for, in screen cells. The menu is clamped into the
/// transcript area at draw time, so this is a request rather than a final position — a right-click
/// in the bottom-right corner still gets a fully visible box.
///
/// It deliberately does NOT carry the selection it will copy. The input thread owns that (it is the
/// thread that reads the clipboard action), and duplicating it here would let the two drift apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ContextMenu {
    pub col: u16,
    pub row: u16,
}

/// Geometry of the last transcript viewport — input thread maps mouse coords → (line, col).
#[derive(Clone, Debug, Default)]
struct TranscriptGeom {
    start: usize,
    visible: usize,
    total: usize,
    area: Rect,
    /// Plain (SGR-stripped) wrapped rows of the full transcript at last draw — used to extract the
    /// selected text on mouse-up without re-rendering on the input thread.
    plain_rows: Vec<String>,
    /// Screen rect of the floating "jump to bottom" button, present only while the transcript is
    /// scrolled up off the tail. `None` when at the tail (button hidden). The input thread hit-tests
    /// a left-click against this before anything else so the button lands the viewport back on tail.
    jump_button: Option<Rect>,
}

fn transcript_geom_slot() -> &'static Mutex<TranscriptGeom> {
    static SLOT: OnceLock<Mutex<TranscriptGeom>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(TranscriptGeom::default()))
}

/// Snapshot of the last transcript geometry for mouse hit-testing (selection / scrollbar drag).
pub(super) fn last_transcript_geom() -> (usize, usize, usize, Rect) {
    let g = transcript_geom_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    (g.start, g.visible, g.total, g.area)
}

/// Screen rect of the floating "jump to bottom" button at last draw, or `None` if the transcript is
/// already at the tail (button hidden). The input thread hit-tests a left-click against this.
pub(super) fn jump_button_rect() -> Option<Rect> {
    let g = transcript_geom_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    g.jump_button
}

/// Screen rect of the right-click Copy menu at last draw, or `None` when no menu is up. The input
/// thread hit-tests a left-click against this, exactly as it does for [`jump_button_rect`].
fn context_menu_rect_slot() -> &'static Mutex<Option<Rect>> {
    static SLOT: OnceLock<Mutex<Option<Rect>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub(super) fn context_menu_rect() -> Option<Rect> {
    *context_menu_rect_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// The selection currently PAINTED, mirrored out of the render thread's `AppState` so the input
/// thread can act on it without owning it.
///
/// The input thread's own `selecting` can NOT serve this purpose: it is `take()`n on mouse-up while
/// the highlight deliberately stays on screen, so between releasing the drag and the next click —
/// precisely when someone reaches for the right button — it is `None` even though text is visibly
/// selected. This mirror is written only by [`set_selection`] / [`clear_selection`], the same two
/// calls that change the highlight, so it cannot disagree with what is on screen.
fn live_selection_slot() -> &'static Mutex<Option<SelectionRange>> {
    static SLOT: OnceLock<Mutex<Option<SelectionRange>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

pub(super) fn live_selection() -> Option<SelectionRange> {
    *live_selection_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// Extract plain text covering `sel` from the last drawn plain rows. Empty if nothing is selected
/// or geometry is stale.
pub(super) fn extract_selection_text(sel: SelectionRange) -> String {
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
            context_menu: None,
            metrics: metrics::FrameMetrics::default(),
            cache: RenderCache::default(),
            screensaver: None,
            screensaver_cache: None,
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
    /// Show the right-click "Copy" menu at these screen cells (clamped into the transcript at draw).
    OpenContextMenu(ContextMenu),
    /// Dismiss the right-click menu.
    CloseContextMenu,
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

struct RuntimeHandle {
    tx: Sender<Command>,
    join: JoinHandle<()>,
}

fn runtime_slot() -> &'static Mutex<Option<RuntimeHandle>> {
    static RUNTIME: OnceLock<Mutex<Option<RuntimeHandle>>> = OnceLock::new();
    RUNTIME.get_or_init(|| Mutex::new(None))
}

struct TerminalSession {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    fn enter() -> io::Result<Self> {
        let mut stdout = io::stdout();
        // EnableMouseCapture: the terminal reports wheel/click/drag as crossterm mouse events
        // instead of leaking the wheel through as ↑/↓ keys (Windows Terminal "alternateScroll").
        execute!(stdout, EnterAlternateScreen, EnableMouseCapture, Hide)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = execute!(
            io::stdout(),
            DisableMouseCapture,
            Show,
            LeaveAlternateScreen
        );
    }
}

pub(super) fn preferred() -> bool {
    // Retained is the ONLY interactive UI, so this is simply "can we take the terminal at all". When
    // it says no (piped output, CI, dumb terminals, or an explicit `NO_STICKY`) the caller runs the
    // plain line-REPL — there is no second renderer to pick, and nothing in the config selects one.
    io::stdout().is_terminal() && !crate::core::cli_config::branded_flag("NO_STICKY")
}

pub(super) fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Panic/Ctrl-C fail-safe: put the terminal back to a sane state WITHOUT locking the runtime or the
/// render thread. Writes the raw escape sequences directly (show cursor, leave alternate screen) so
/// it stays safe to call from a panic hook, where the render thread may hold poisoned locks or be the
/// thread that panicked. Idempotent: doing it twice just re-emits harmless sequences. Deliberately
/// does NOT `join` the render thread — a panic hook must never block.
pub(super) fn emergency_restore() {
    ACTIVE.store(false, Ordering::Relaxed);
    // `\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l` disable mouse reporting (all modes
    // EnableMouseCapture turns on) · `\x1b[?25h` show cursor · `\x1b[?1049l` leave alternate screen.
    // Written unconditionally — a terminal not in a given mode just ignores the corresponding reset.
    let _ = write!(
        io::stdout(),
        "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?25h\x1b[?1049l"
    );
    let _ = io::stdout().flush();
}

pub(super) fn is_running() -> bool {
    runtime_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

pub(super) fn size() -> (u16, u16) {
    (ROWS.load(Ordering::Relaxed), COLS.load(Ordering::Relaxed))
}

pub(super) fn start(intro: &str, status: &str) -> bool {
    if !preferred() {
        return false;
    }
    let mut slot = runtime_slot().lock().unwrap_or_else(|e| e.into_inner());
    if slot.is_some() {
        ACTIVE.store(true, Ordering::Relaxed);
        return true;
    }
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::channel();
    let intro = intro.to_string();
    let status = status.to_string();
    let join = std::thread::spawn(move || render_loop(rx, ready_tx, intro, status));
    let ready = ready_rx
        .recv_timeout(Duration::from_secs(2))
        .unwrap_or(false);
    if !ready {
        let _ = join.join();
        return false;
    }
    *slot = Some(RuntimeHandle { tx, join });
    ACTIVE.store(true, Ordering::Relaxed);
    true
}

pub(super) fn stop() {
    ACTIVE.store(false, Ordering::Relaxed);
    let handle = runtime_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take();
    if let Some(handle) = handle {
        let (ack_tx, ack_rx) = mpsc::channel();
        let _ = handle.tx.send(Command::Shutdown(ack_tx));
        let _ = ack_rx.recv_timeout(Duration::from_secs(2));
        let _ = handle.join.join();
    }
}

pub(super) fn suspend() {
    if !is_running() {
        return;
    }
    ACTIVE.store(false, Ordering::Relaxed);
    let tx = runtime_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|h| h.tx.clone());
    if let Some(tx) = tx {
        let (ack_tx, ack_rx) = mpsc::channel();
        let _ = tx.send(Command::Suspend(ack_tx));
        let _ = ack_rx.recv_timeout(Duration::from_secs(2));
    }
}

pub(super) fn resume(status: &str) -> bool {
    let tx = runtime_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|h| h.tx.clone());
    let Some(tx) = tx else { return false };
    let (ack_tx, ack_rx) = mpsc::channel();
    let _ = tx.send(Command::Resume {
        status: status.to_string(),
        ack: ack_tx,
    });
    let ok = ack_rx.recv_timeout(Duration::from_secs(2)).unwrap_or(false);
    ACTIVE.store(ok, Ordering::Relaxed);
    ok
}

/// Whether the real terminal size differs from what we last painted at. Cheap (a `TIOCGWINSZ`-class
/// query via crossterm), called once per idle tick so an idle resize repaints without a second input
/// reader. Returns false when the size can't be read (keep the last known good size).
fn terminal_size_changed() -> bool {
    match crossterm::terminal::size() {
        Ok((cols, rows)) => {
            let cols = cols.max(20);
            let rows = rows.max(8);
            cols != COLS.load(Ordering::Relaxed) || rows != ROWS.load(Ordering::Relaxed)
        }
        Err(_) => false,
    }
}

fn send(cmd: Command) {
    let tx = runtime_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|h| h.tx.clone());
    if let Some(tx) = tx {
        let _ = tx.send(cmd);
    }
}

pub(super) fn emit(s: &str) {
    send(Command::Emit(s.to_string()));
}

pub(super) fn assistant_delta(s: &str) {
    send(Command::AssistantDelta(s.to_string()));
}

pub(super) fn assistant_finish(interrupted: bool) {
    send(Command::AssistantFinish { interrupted });
}

pub(super) fn update_input(input: InputSnapshot) {
    send(Command::Input(input));
}

/// Throw away ratatui's idea of what's on screen and repaint every cell from `AppState` (Ctrl-L).
///
/// The manual recovery hatch for the one failure this renderer cannot detect: something wrote to the
/// terminal behind its back (a stray `println!` from a subsystem, a child process that printed to the
/// inherited stdout). ratatui only repaints cells its diff believes changed, so foreign text stays
/// wedged inside later frames. Clearing first makes the next draw unconditional. Blocks live in
/// `AppState`, so this is a repaint, not a replay — no transcript is lost.
pub(super) fn redraw() {
    send(Command::Redraw);
}

pub(super) fn set_working(working: bool) {
    send(Command::Working(working));
}

pub(super) fn set_status(status: &str) {
    send(Command::Status(status.to_string()));
}

pub(super) fn set_context(permille: u16) {
    send(Command::Context(permille.min(1000)));
}

pub(super) fn set_health(kind: HealthKind) {
    send(Command::Health(kind));
}

pub(super) fn set_selection(sel: SelectionRange) {
    // Mirror BEFORE queueing: the mirror is read by the input thread (right-click), and the command
    // queue is drained asynchronously by the render thread. Writing it here makes "what is selected"
    // true the instant the selection changes rather than one frame later.
    *live_selection_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(sel);
    send(Command::SetSelection(sel));
}

pub(super) fn clear_selection() {
    *live_selection_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    // A menu with nothing to copy is dead weight, so `ClearSelection` also closes it in the render
    // thread — drop the hit-test rect here for the same reason `close_context_menu` does, or an Esc
    // that clears a selection would leave a rect that keeps swallowing the next click.
    *context_menu_rect_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    send(Command::ClearSelection);
}

/// Open the right-click Copy menu at `col`/`row` (screen cells). Callers open it only when a
/// selection exists — see `handle_retained_mouse`.
pub(super) fn open_context_menu(col: u16, row: u16) {
    send(Command::OpenContextMenu(ContextMenu { col, row }));
}

pub(super) fn close_context_menu() {
    // Clear the hit-test rect here too, not just in the render thread: a click that dismisses the
    // menu is immediately followed by more clicks, and a stale rect would keep swallowing them until
    // the next frame lands.
    *context_menu_rect_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    send(Command::CloseContextMenu);
}

/// Jump the transcript viewport so absolute wrapped-line `start` is at the top (scrollbar drag).
pub(super) fn scroll_to(start: usize) {
    send(Command::ScrollTo(start));
}

/// Push (state `Running`) or update-in-place (state `Ok`/`Err`, matched by `seq`) a tool-call line.
pub(super) fn tool_event(ev: ToolEvent) {
    send(Command::Tool(ev));
}

/// Replace the in-place plan checklist box with a fresh snapshot (empty → removes the box).
pub(super) fn plan_update(rows: Vec<PlanRow>) {
    send(Command::Plan(rows));
}

/// Push a boxed diff preview under the most recent edit.
pub(super) fn diff_box(d: DiffPayload) {
    send(Command::Diff(d));
}

/// Push a green verify-gate success line.
pub(super) fn verify_line(v: VerifyPayload) {
    send(Command::Verify(v));
}

pub(super) fn tick() {
    send(Command::Tick);
}

pub(super) fn open_overlay(overlay: OverlaySnapshot) {
    send(Command::OpenOverlay(overlay));
}

/// Refresh an already-open overlay's body in place, keeping the reader's scroll offset.
pub(super) fn update_overlay(lines: Vec<String>) {
    send(Command::UpdateOverlay(lines));
}

pub(super) fn close_overlay() {
    send(Command::CloseOverlay);
}

pub(super) fn scroll(delta: i32) {
    send(Command::Scroll(delta));
}

pub(super) fn scroll_end() {
    send(Command::ScrollEnd);
}

/// Raise (`Some(idx)`) or clear (`None`) the idle screensaver. The render thread cover-encodes card
/// `idx` to its own pixel size and blits it fullscreen, restoring the transcript on clear.
pub(super) fn screensaver(card: Option<usize>) {
    send(Command::Screensaver(card));
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

#[allow(dead_code)]
pub(super) fn set_focus(focused: bool) {
    send(Command::Focus(focused));
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
        // Turn just ended (working true→false): a tool may have spawned a child that reset our
        // console input mode, dropping mouse capture. Re-assert it (and raw mode + cursor hide) so the
        // wheel keeps arriving as mouse events and the transcript stays scrollable. Idempotent — a
        // terminal already in these modes ignores the repeat. Only on the edge, so it costs nothing
        // during a turn or while idle.
        if was_working && !state.working && session.is_some() {
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
            if !working {
                state.frame = 0;
            }
        }
        Command::Status(status) => state.input.status = status,
        Command::Context(v) => state.ctx_permille = v,
        Command::Health(h) => state.health = h,
        Command::Tick => state.frame = state.frame.wrapping_add(1),
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
        // Dropping the highlight closes the menu with it: a "Copy" button whose selection is gone
        // would copy nothing, and leaving it on screen after the highlight vanished reads as a stuck
        // frame. Every path that clears a selection therefore dismisses the menu for free.
        Command::ClearSelection => {
            state.selection = None;
            state.context_menu = None;
        }
        Command::OpenContextMenu(m) => state.context_menu = Some(m),
        Command::CloseContextMenu => state.context_menu = None,
        Command::Focus(v) => state.focused = v,
        // Lifecycle + `Redraw` carry no AppState change: they are handled by `render_loop`, which owns
        // the `TerminalSession` (`Redraw` sets `force_clear` so the next draw is unconditional).
        // Listed explicitly rather than via `_` so a new command can't silently become a no-op here.
        Command::Suspend(_) | Command::Resume { .. } | Command::Shutdown(_) | Command::Redraw => {}
    }
}

fn draw(frame: &mut Frame<'_>, state: &mut AppState) {
    let area = frame.area();
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(FOOTER_ROWS)])
        .split(area);
    draw_transcript(frame, chunks[0], state);
    draw_footer(frame, chunks[1], state);
    if let Some(overlay) = state.input.overlay.clone() {
        // draw_overlay clamps the requested scroll against the overlay's own visible height and
        // returns the value actually used, so a stored offset past the end snaps back next frame.
        state.overlay_scroll = draw_overlay(frame, area, &overlay, state.overlay_scroll);
    }
    // The Copy menu paints LAST so it sits above the transcript and the overlay both — it is the
    // surface the pointer is currently on, and a box the user just summoned must not appear behind
    // something else. Its hit-test rect is published every frame (including `None`), so a menu that
    // stopped being drawn stops swallowing clicks on the very frame it disappears.
    let menu_rect = state
        .context_menu
        .and_then(|m| draw_context_menu(frame, chunks[0], m));
    if let Ok(mut slot) = context_menu_rect_slot().lock() {
        *slot = menu_rect;
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
fn resolve_transcript_scroll(
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

fn draw_transcript(frame: &mut Frame<'_>, area: Rect, state: &mut AppState) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    // Leave 1 cell on the right for the scrollbar track when content overflows — content already
    // reserves `width-2` so the thumb never paints over text.
    let content_width = area.width.saturating_sub(2).max(8);
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut plain_rows: Vec<String> = Vec::new();
    for block in &state.blocks {
        let rows = state.cache.get_or_render(block, content_width);
        for row in rows {
            plain_rows.push(console::strip_ansi_codes(&row).into_owned());
            lines.push(styled_row(block.kind, row));
        }
    }
    // Apply selection reverse highlight before scrolling into the viewport.
    if let Some(sel) = state.selection {
        apply_selection_highlight(&mut lines, sel);
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

fn draw_footer(frame: &mut Frame<'_>, area: Rect, state: &AppState) {
    if area.height < FOOTER_ROWS || area.width == 0 {
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
    let elapsed = state
        .working_since
        .map(|t| t.elapsed().as_secs())
        .unwrap_or(0);
    // Right of the HUD row: working pill while the agent runs, else coloured health chip + a
    // compact context meter `⟦▓▓░░…⟧ N%`. Green = OK, yellow =
    // slow/transient, red = permanent unavailability, muted = still checking.
    //
    // Built as spans (not a single pre-coloured string) so the bar can take its own fill colour
    // independent of the health dot.
    let right_spans: Vec<Span<'static>> = if state.working {
        const STAR: [&str; 6] = ["✶", "✷", "✸", "✹", "✺", "✻"];
        vec![Span::styled(
            format!(
                "{} working · {}s · Esc to stop",
                STAR[state.frame % STAR.len()],
                elapsed
            ),
            Style::default().fg(Color::Indexed(crate::ui::theme::ACCENT)),
        )]
    } else {
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
            Style::default().fg(Color::Indexed(crate::ui::theme::ACCENT_DIM)),
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
    let (shown, cursor_off) = input_line(state, type_budget);
    let shown_w = console::measure_text_width(&shown);
    // Pad between the typed text and the right-aligned hint. `3` = `❯ ` (2) + one right margin.
    let hint_gap = width
        .saturating_sub(3 + imgtag_w + shown_w + hint_w.saturating_sub(1))
        .max(if hint.is_empty() { 0 } else { 1 });
    let mut prompt_spans = vec![Span::styled(
        "❯ ",
        Style::default()
            .fg(Color::Indexed(crate::ui::theme::ACCENT))
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
    frame.render_widget(Paragraph::new(Line::from(prompt_spans)), rows[2]);
    frame.render_widget(
        Paragraph::new(Line::styled(
            rule,
            Style::default().fg(Color::Indexed(crate::ui::theme::ACCENT_DIM)),
        )),
        rows[3],
    );
    let cursor_x = rows[2]
        .x
        .saturating_add(2)
        .saturating_add(imgtag_w as u16)
        .saturating_add(cursor_off as u16);
    frame.set_cursor_position((cursor_x.min(rows[2].right().saturating_sub(1)), rows[2].y));
}

fn input_line(state: &AppState, budget: usize) -> (String, usize) {
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
        return (console::truncate_str(&ph, budget, "…").into_owned(), 0);
    }
    // Only collapse to a "N lines pasted" chip for a genuinely large paste — match the classic
    // renderer's `>= 5 lines` threshold (tui.rs). A short multi-line draft (e.g. one char then
    // Shift+Enter) stays inline: turning a two-line note into a misleading "2 lines pasted" chip
    // is exactly the bug we're fixing.
    let nlines = state.input.draft.iter().filter(|&&c| c == '\n').count() + 1;
    if nlines >= 5 {
        let text: String = state.input.draft.iter().collect();
        let n = text.lines().count().max(1);
        let chip = format!("↵ {n} lines pasted");
        let w = console::measure_text_width(&chip);
        return (
            console::truncate_str(&chip, budget, "…").into_owned(),
            w.min(budget),
        );
    }
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
    let mut start = cursor;
    let mut caret = 0usize;
    while start > 0 {
        let cw = cellw(state.input.draft[start - 1]);
        if caret + cw > budget.saturating_sub(1) {
            break;
        }
        start -= 1;
        caret += cw;
    }
    let mut shown = String::new();
    let mut used = 0usize;
    for &c in &state.input.draft[start..] {
        let cw = cellw(c);
        if used + cw > budget {
            break;
        }
        shown.push(disp(c));
        used += cw;
    }
    (shown, caret)
}

/// Draw the informational overlay, applying `scroll` (rows hidden above the top) clamped so the last
/// page is the furthest you can go. Returns the CLAMPED scroll so the caller can write it back — a
/// PageDown past the end then reads as "at the bottom" rather than drifting into empty space.
fn draw_overlay(
    frame: &mut Frame<'_>,
    area: Rect,
    overlay: &OverlaySnapshot,
    scroll: usize,
) -> usize {
    let width = area.width.saturating_sub(4).min(84).max(20);
    let height = (overlay.lines.len() as u16 + 4)
        .min(area.height.saturating_sub(2))
        .max(5);
    let rect = centered(area, width, height);
    frame.render_widget(Clear, rect);
    let block = FrameBlock::default()
        .borders(Borders::ALL)
        .title(overlay.title.clone())
        .border_style(Style::default().fg(Color::Indexed(crate::ui::theme::ACCENT_DIM)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    let mut lines: Vec<Line<'static>> = overlay
        .lines
        .iter()
        .enumerate()
        .map(|(i, line)| {
            let selected = overlay.selected == Some(i);
            Line::styled(
                if selected {
                    format!("› {line}")
                } else {
                    format!("  {line}")
                },
                if selected {
                    Style::default()
                        .fg(Color::Indexed(crate::ui::theme::ACCENT))
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Gray)
                },
            )
        })
        .collect();
    if !overlay.hint.is_empty() {
        lines.push(Line::styled(
            overlay.hint.clone(),
            Style::default().fg(Color::DarkGray),
        ));
    }
    // Clamp scroll so the final page is the furthest reachable position (never scroll past the end).
    let visible = inner.height as usize;
    let max_scroll = lines.len().saturating_sub(visible);
    let clamped = scroll.min(max_scroll);
    frame.render_widget(
        Paragraph::new(lines)
            .wrap(Wrap { trim: false })
            .scroll((clamped.min(u16::MAX as usize) as u16, 0)),
        inner,
    );
    clamped
}

/// Label on the right-click menu's only button. Padded so the highlight reads as a button rather
/// than as stray reversed text.
const COPY_LABEL: &str = " Copy ";

/// Where the Copy menu box goes for a right-click at `menu`, or `None` when `area` is too small to
/// hold one. Pure so the clamping is testable without a terminal backend.
///
/// The box is offered one cell right-and-below the pointer so it doesn't sit under the cursor, then
/// pulled back inside `area` when that would overflow. Without the clamp a right-click near the
/// bottom-right corner puts most of the box off-screen, leaving a Copy button that cannot be clicked
/// — the one position where a context menu is most likely to be used.
fn context_menu_layout(area: Rect, menu: ContextMenu) -> Option<Rect> {
    let width = console::measure_text_width(COPY_LABEL) as u16 + 2; // + borders
    let height = 3; // top border, label row, bottom border
    if area.width < width || area.height < height {
        return None;
    }
    // Both maxima are ≥ the area origin because of the size check above.
    let max_x = area.x + area.width - width;
    let max_y = area.y + area.height - height;
    let x = menu.col.saturating_add(1).clamp(area.x, max_x);
    let y = menu.row.saturating_add(1).clamp(area.y, max_y);
    Some(Rect::new(x, y, width, height))
}

/// Paint the right-click Copy menu, returning the rect actually drawn so the input thread can
/// hit-test a click against it.
fn draw_context_menu(frame: &mut Frame<'_>, area: Rect, menu: ContextMenu) -> Option<Rect> {
    let rect = context_menu_layout(area, menu)?;
    // `Clear` first: the menu floats over live transcript text, and without wiping the cells the
    // box borders blend into whatever was underneath.
    frame.render_widget(Clear, rect);
    let block = FrameBlock::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Indexed(crate::ui::theme::ACCENT_DIM)));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            COPY_LABEL,
            Style::default()
                .fg(Color::Black)
                .bg(Color::Indexed(crate::ui::theme::ACCENT))
                .add_modifier(Modifier::BOLD),
        ))),
        inner,
    );
    Some(rect)
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width: width.min(area.width),
        height: height.min(area.height),
    }
}

fn render_assistant_rows(raw: &str, width: usize) -> Vec<String> {
    // Keep SGR: the markdown renderer emits the moonlight gutter, code-box borders, and syntax
    // highlight as colour codes — `sanitize_keep_sgr` preserves them (dropping only cursor moves)
    // and `ansi_spans` turns them into styled spans at draw time.
    sanitize_keep_sgr(&crate::ui::markdown::render_retained(raw, width.max(24)))
        .split('\n')
        .map(str::to_string)
        .collect()
}

/// Format a run time for the result line: `· 940ms` under a second, `· 1.2s` under a minute, then
/// `· 7m03s` / `· 2h05m`. Sub-second times keep millisecond resolution; the minute/hour tiers exist
/// because a sub-agent dispatch is a single tool call that can run for hours, and `· 7203.4s` makes
/// the reader do the division. Empty for `None` (unknown — restored transcripts / eager-adopted).
fn fmt_elapsed(ms: Option<u64>) -> String {
    match ms {
        None => String::new(),
        Some(ms) if ms < 1000 => format!(" · {ms}ms"),
        Some(ms) if ms < 60_000 => format!(" · {:.1}s", ms as f64 / 1000.0),
        Some(ms) => format!(" · {}", crate::agent::orchestration::fmt_secs(ms / 1000)),
    }
}

/// Lay out one tool-call line under the transcript, mockup-style but result-below: the call
/// `<icon> <name>   <target>` on its own line (icon + name in moonlight accent, target dim silver),
/// then an indented `└ <digest> · <time>` line beneath it — the result digest tinted by state
/// (running = faint, ok = green, err = salmon) and carrying the wall-clock run time. A still-running
/// call (empty digest) is just the call line; the result line is added when the digest lands.
pub(super) fn render_tool_row(t: &ToolEvent, width: usize) -> String {
    use crate::ui::theme;
    let _ = width; // stacked layout no longer needs the frame width to right-align
    let icon = if t.icon.is_empty() {
        String::new()
    } else {
        format!("{} ", t.icon)
    };
    let name_styled = theme::accent(&t.name).to_string();
    let call_line = if t.target.is_empty() {
        format!("{}{}", theme::accent(&icon), name_styled)
    } else {
        format!(
            "{}{}   {}",
            theme::accent(&icon),
            name_styled,
            theme::accent_dim(&t.target)
        )
    };
    if t.digest.is_empty() {
        return call_line;
    }
    // Result on the line below: `└ <digest> · <time>`, digest tinted by outcome, time dimmed.
    let digest_styled = match t.state {
        ToolState::Running => theme::faint(&t.digest).to_string(),
        ToolState::Ok => theme::ok(&t.digest).to_string(),
        ToolState::Err => theme::err(&t.digest).to_string(),
    };
    let time = fmt_elapsed(t.elapsed_ms);
    let time_styled = if time.is_empty() {
        String::new()
    } else {
        theme::faint(&time).to_string()
    };
    format!(
        "{call_line}\n{} {digest_styled}{time_styled}",
        theme::faint("└")
    )
}

/// Render the in-place plan panel as a boxed checklist: a `☑ done/total · plan` header row, then one
/// `✓ / ▸ / ○` row per item, framed with the same rounded box the markdown renderer uses. Done rows
/// are green + dim-struck, the in-progress row is bright moonlight, pending rows are faint.
pub(super) fn render_plan_box(rows: &[PlanRow], width: usize) -> Vec<String> {
    use crate::ui::theme;
    let done = rows.iter().filter(|r| r.status == 2).count();
    let header = format!("☑ {done}/{} · plan", rows.len());
    // Inner width: cap so the box doesn't sprawl on a very wide pane; leave room for `│ ` + ` │`.
    let inner = width.saturating_sub(2).min(72).max(12);
    let bar = "─".repeat(inner);
    let mut out = Vec::new();
    out.push(
        theme::accent_dim(format!(
            "╭─ {} ─╮",
            pad_to(&header, inner.saturating_sub(4))
        ))
        .to_string(),
    );
    for r in rows {
        let glyph = match r.status {
            2 => "✓",
            1 => "▸",
            _ => "○",
        };
        let g = match r.status {
            2 => theme::ok(glyph).to_string(),
            1 => theme::accent(glyph).bold().to_string(),
            _ => theme::faint(glyph).to_string(),
        };
        // Body row `│ <glyph> <text><pad> │`: the span BETWEEN the two rules must be exactly `inner`
        // cells so the right `│` lines up with the border corners. That span is
        // ` `+glyph(1)+` `+text+pad+` ` = 4 + text + pad, so pad = inner − 4 − text.
        let text_budget = inner.saturating_sub(4);
        let clipped = clip_to(&r.text, text_budget);
        let styled = match r.status {
            2 => theme::muted(&clipped).to_string(),
            1 => theme::accent(&clipped).bold().to_string(),
            _ => theme::faint(&clipped).to_string(),
        };
        let pad = inner.saturating_sub(4 + console::measure_text_width(&clipped));
        out.push(format!(
            "{} {} {}{} {}",
            theme::accent_dim("│"),
            g,
            styled,
            " ".repeat(pad),
            theme::accent_dim("│")
        ));
    }
    out.push(theme::accent_dim(format!("╰{bar}╯")).to_string());
    out
}

/// Render a boxed diff preview: a `diff · <path>  +A −D` header, then the `+`/`−` lines (green /
/// salmon) inside the same rounded frame. Lines are clipped to the inner width.
pub(super) fn render_diff_box(d: &DiffPayload, width: usize) -> Vec<String> {
    use crate::ui::theme;
    let inner = width.saturating_sub(2).min(84).max(12);
    let bar = "─".repeat(inner);
    let header = format!("diff · {}   +{} −{}", d.path, d.adds, d.dels);
    let mut out = Vec::new();
    out.push(
        theme::accent_dim(format!(
            "╭─ {} ─╮",
            pad_to(&header, inner.saturating_sub(4))
        ))
        .to_string(),
    );
    for (is_add, content) in &d.lines {
        let budget = inner.saturating_sub(4);
        let clipped = clip_to(content, budget.saturating_sub(2));
        let body = if *is_add {
            format!("+ {clipped}")
        } else {
            format!("− {clipped}")
        };
        let pad = inner.saturating_sub(2 + console::measure_text_width(&body));
        let styled = if *is_add {
            theme::ok(&body).to_string()
        } else {
            theme::err(&body).to_string()
        };
        out.push(format!(
            "{} {}{} {}",
            theme::accent_dim("│"),
            styled,
            " ".repeat(pad),
            theme::accent_dim("│")
        ));
    }
    out.push(theme::accent_dim(format!("╰{bar}╯")).to_string());
    out
}

/// Render the verify-gate success line: a green `✓ <cmd> — <detail>`, clipped to width.
pub(super) fn render_verify_line(v: &VerifyPayload, width: usize) -> String {
    use crate::ui::theme;
    let text = if v.detail.is_empty() {
        format!("✓ {}", v.cmd)
    } else {
        format!("✓ {} — {}", v.cmd, v.detail)
    };
    theme::ok(clip_to(&text, width)).to_string()
}

/// Clip a plain string to `max` display columns (ellipsis when it would overflow). Width-aware so a
/// wide glyph never half-lands past the frame.
fn clip_to(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if console::measure_text_width(s) <= max {
        return s.to_string();
    }
    let budget = max.saturating_sub(1);
    let mut out = String::new();
    let mut w = 0usize;
    for ch in s.chars() {
        let cw = console::measure_text_width(&ch.to_string()).max(1);
        if w + cw > budget {
            break;
        }
        out.push(ch);
        w += cw;
    }
    out.push('…');
    out
}

/// Clip then right-pad a plain string to exactly `width` display columns (for a box header).
fn pad_to(s: &str, width: usize) -> String {
    let clipped = clip_to(s, width);
    let w = console::measure_text_width(&clipped);
    format!("{clipped}{}", " ".repeat(width.saturating_sub(w)))
}

/// Strip ALL terminal control sequences INCLUDING colour/SGR, leaving printable text only. Used for
/// the intro block (rendered as one flat dim line) and where a plain-text guarantee is wanted.
fn sanitize_text(input: &str) -> String {
    console::strip_ansi_codes(&sanitize_keep_sgr(input)).into_owned()
}

/// Strip terminal control sequences that would corrupt the retained frame (cursor moves, screen
/// erase, save/restore, carriage returns) but PRESERVE colour/SGR (`\x1b[…m`) so [`ansi_spans`] can
/// turn them into styled ratatui spans. This is what keeps the `❯` user echo in accent, the edit
/// diff green/salmon, and the `◆` tool anchor tinted by state — instead of one flat grey.
fn sanitize_keep_sgr(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.peek() {
                Some('[') => {
                    chars.next();
                    let mut seq = String::from("\x1b[");
                    let mut final_byte = None;
                    while let Some(&pc) = chars.peek() {
                        chars.next();
                        seq.push(pc);
                        // A CSI sequence ends at its final byte (0x40–0x7E). We break on the first
                        // ASCII letter, which covers every sequence we actually emit: `m` (SGR),
                        // `J`/`K` (erase), `s`/`u` (save/restore cursor), `H`/`A`…`G` (moves).
                        if pc.is_ascii_alphabetic() {
                            final_byte = Some(pc);
                            break;
                        }
                    }
                    if final_byte == Some('m') {
                        out.push_str(&seq); // keep colour / bold / dim — drop everything else
                    }
                }
                // A non-CSI escape (e.g. `\x1b7` save cursor): drop the ESC and its one payload byte.
                _ => {
                    chars.next();
                }
            }
            continue;
        }
        match c {
            '\n' => out.push('\n'),
            '\t' => out.push_str("    "),
            '\r' => {}
            c if !c.is_control() => out.push(c),
            _ => {}
        }
    }
    out
}

/// Map a basic ANSI colour index (0–15, the `30`–`37` / `90`–`97` SGR families) to a ratatui colour.
fn basic_color(n: u16) -> Color {
    match n {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        _ => Color::White,
    }
}

/// Fold one SGR parameter list (the `;`-separated numbers before an `m`) into the running style,
/// resetting to `base` on `0`/`39`. Supports the codes the app actually emits: reset, bold, dim,
/// italic, underline, the 8+8 basic-colour families, and 256-colour (`38;5;n`) / truecolor (`38;2`).
fn apply_sgr(base: Style, cur: Style, params: &str) -> Style {
    let codes: Vec<u16> = params
        .split(';')
        .filter(|s| !s.is_empty())
        .filter_map(|s| s.parse::<u16>().ok())
        .collect();
    if codes.is_empty() {
        return base; // a bare `\x1b[m` is `\x1b[0m` — a full reset
    }
    let mut style = cur;
    let mut i = 0;
    while i < codes.len() {
        match codes[i] {
            0 => style = base,
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            22 => {
                style = style
                    .remove_modifier(Modifier::BOLD)
                    .remove_modifier(Modifier::DIM)
            }
            30..=37 => style = style.fg(basic_color(codes[i] - 30)),
            39 => style = style.fg(base.fg.unwrap_or(Color::Gray)),
            90..=97 => style = style.fg(basic_color(codes[i] - 90 + 8)),
            38 => match codes.get(i + 1) {
                Some(5) => {
                    if let Some(&n) = codes.get(i + 2) {
                        style = style.fg(Color::Indexed(n as u8));
                    }
                    i += 2;
                }
                Some(2) => {
                    if let (Some(&r), Some(&g), Some(&b)) =
                        (codes.get(i + 2), codes.get(i + 3), codes.get(i + 4))
                    {
                        style = style.fg(Color::Rgb(r as u8, g as u8, b as u8));
                    }
                    i += 4;
                }
                _ => {}
            },
            48 => match codes.get(i + 1) {
                Some(5) => i += 2, // background colour — measured/skipped, foreground is what reads
                Some(2) => i += 4,
                _ => {}
            },
            _ => {}
        }
        i += 1;
    }
    style
}

/// Parse a single already-sanitised row (SGR only, no cursor moves) into ratatui spans over `base`.
/// A row with no colour codes collapses to one span in the base style, so uncoloured output looks
/// exactly as before.
fn ansi_spans(row: &str, base: Style) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = base;
    let mut buf = String::new();
    let mut chars = row.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            let mut params = String::new();
            let mut final_byte = None;
            while let Some(&pc) = chars.peek() {
                chars.next();
                if pc.is_ascii_digit() || pc == ';' {
                    params.push(pc);
                } else {
                    final_byte = Some(pc);
                    break;
                }
            }
            if final_byte == Some('m') {
                if !buf.is_empty() {
                    spans.push(Span::styled(std::mem::take(&mut buf), cur));
                }
                cur = apply_sgr(base, cur, &params);
            }
            continue;
        }
        buf.push(c);
    }
    if !buf.is_empty() {
        spans.push(Span::styled(buf, cur));
    }
    if spans.is_empty() {
        spans.push(Span::styled(String::new(), base));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn short_multiline_draft_stays_inline() {
        let mut state = AppState::new("intro", "status");
        state.input.draft = "x\n".chars().collect();
        state.input.cursor = state.input.draft.len();

        let (shown, caret) = input_line(&state, 40);

        assert_eq!(shown, "x↵", "a short Shift+Enter draft is shown inline");
        assert_eq!(caret, 2, "caret advances over the visible newline glyph");
        assert!(
            !shown.contains("lines pasted"),
            "short text must not look like a paste chip"
        );
    }

    #[test]
    fn large_multiline_draft_uses_paste_chip() {
        let mut state = AppState::new("intro", "status");
        state.input.draft = "a\nb\nc\nd\ne".chars().collect();
        state.input.cursor = state.input.draft.len();

        let (shown, _) = input_line(&state, 40);

        assert_eq!(shown, "↵ 5 lines pasted");
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

    /// A right-click in the bottom-right corner is exactly where a naive `x+1, y+1` placement puts
    /// most of the box off-screen — leaving a Copy button that cannot be clicked at the position a
    /// context menu is most likely to be used from.
    #[test]
    fn copy_menu_is_clamped_fully_inside_the_transcript_area() {
        let area = Rect::new(0, 0, 40, 20);
        let fits = |r: Rect| {
            r.x >= area.x
                && r.y >= area.y
                && r.x + r.width <= area.x + area.width
                && r.y + r.height <= area.y + area.height
        };

        // Middle of the area: offered one cell down-right of the pointer so the box isn't under it.
        let mid = context_menu_layout(area, ContextMenu { col: 10, row: 5 }).unwrap();
        assert_eq!((mid.x, mid.y), (11, 6));
        assert!(fits(mid));

        // Bottom-right corner: pulled back inside instead of overflowing.
        let corner = context_menu_layout(area, ContextMenu { col: 39, row: 19 }).unwrap();
        assert!(fits(corner), "corner menu escaped the area: {corner:?}");
        assert_eq!(corner.x + corner.width, area.width, "flush to the right edge");
        assert_eq!(corner.y + corner.height, area.height, "flush to the bottom");

        // A non-zero origin (the transcript never starts at 0,0 once a footer exists) is respected.
        let offset_area = Rect::new(4, 3, 40, 20);
        let off = context_menu_layout(offset_area, ContextMenu { col: 4, row: 3 }).unwrap();
        assert!(off.x >= 4 && off.y >= 3, "clamped to the area origin: {off:?}");

        // A viewport too small to hold the box draws nothing rather than a clipped, unclickable one.
        assert!(context_menu_layout(Rect::new(0, 0, 3, 20), ContextMenu { col: 0, row: 0 }).is_none());
        assert!(context_menu_layout(Rect::new(0, 0, 40, 2), ContextMenu { col: 0, row: 0 }).is_none());
    }

    /// Losing the highlight must take the menu with it. A "Copy" button whose selection is gone would
    /// copy nothing, so every path that clears a selection (Esc, click-away, the jump button) has to
    /// dismiss the menu — which is why `ClearSelection` does it rather than each caller remembering.
    #[test]
    fn clearing_the_selection_also_closes_the_copy_menu() {
        let mut state = AppState::new("intro", "status");
        let sel = SelectionRange {
            anchor_line: 1,
            anchor_col: 0,
            cursor_line: 1,
            cursor_col: 4,
        };
        apply_command(&mut state, Command::SetSelection(sel));
        apply_command(
            &mut state,
            Command::OpenContextMenu(ContextMenu { col: 5, row: 5 }),
        );
        assert!(state.context_menu.is_some());

        apply_command(&mut state, Command::ClearSelection);
        assert!(state.selection.is_none());
        assert!(
            state.context_menu.is_none(),
            "a menu offering to copy nothing must not survive the highlight"
        );

        // Closing the menu on its own leaves the highlight alone — copying should not deselect.
        apply_command(&mut state, Command::SetSelection(sel));
        apply_command(
            &mut state,
            Command::OpenContextMenu(ContextMenu { col: 5, row: 5 }),
        );
        apply_command(&mut state, Command::CloseContextMenu);
        assert!(state.context_menu.is_none());
        assert_eq!(
            state.selection,
            Some(sel),
            "dismissing the menu keeps the selection"
        );
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
}
