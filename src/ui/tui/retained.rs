//! Retained full-frame terminal backend.
//!
//! The legacy sticky implementation in the parent module remains the rollback path. This backend
//! owns the alternate screen from one render thread; every other thread only sends commands.

use crossterm::cursor::{Hide, Show};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block as FrameBlock, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{self, IsTerminal, Stdout, Write};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

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
}

#[derive(Clone)]
struct UiBlock {
    id: u64,
    kind: BlockKind,
    content: String,
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
            hash: hash_text(&block.content),
            complete: block.complete,
        };
        if let Some(rows) = self.rows.get(&key) {
            self.hits += 1;
            return rows.clone();
        }
        self.misses += 1;
        let rows = match block.kind {
            BlockKind::Assistant => render_assistant_rows(&block.content, width as usize),
            _ => sanitize_keep_sgr(&block.content).split('\n').map(str::to_string).collect(),
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
    metrics: metrics::FrameMetrics,
    cache: RenderCache,
}

impl AppState {
    fn new(intro: &str, status: &str) -> Self {
        Self {
            blocks: vec![UiBlock {
                id: 1,
                kind: BlockKind::Intro,
                content: sanitize_text(intro),
                complete: true,
            }],
            next_id: 2,
            active_assistant: None,
            input: InputSnapshot { status: status.to_string(), ..InputSnapshot::default() },
            working: false,
            working_since: None,
            frame: 0,
            ctx_permille: 0,
            scroll_from_tail: 0,
            last_total: 0,
            overlay_scroll: 0,
            focused: true,
            metrics: metrics::FrameMetrics::default(),
            cache: RenderCache::default(),
        }
    }

    fn push_block(&mut self, kind: BlockKind, content: String, complete: bool) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        self.blocks.push(UiBlock { id, kind, content, complete });
        if self.blocks.len() > BLOCK_LIMIT {
            let excess = self.blocks.len() - BLOCK_LIMIT;
            self.blocks.drain(0..excess);
        }
        // No scroll reset here: when the user is at the bottom (`scroll_from_tail == 0`) the tail is
        // followed automatically; when they've scrolled up to read, `draw_transcript` anchors on the
        // same content so newly-appended blocks don't yank the viewport down mid-read.
        id
    }

    fn push_assistant(&mut self, delta: &str) {
        let id = match self.active_assistant {
            Some(id) => id,
            None => {
                let id = self.push_block(BlockKind::Assistant, String::new(), false);
                self.active_assistant = Some(id);
                id
            }
        };
        if let Some(block) = self.blocks.iter_mut().find(|b| b.id == id) {
            block.content.push_str(delta);
            block.complete = false;
        }
        // Deliberately no `scroll_from_tail = 0`: a streaming token must not fight a user who has
        // scrolled up to read. Follow-at-bottom vs pinned-while-scrolled-up is handled at draw time.
    }

    fn finish_assistant(&mut self, interrupted: bool) {
        let Some(id) = self.active_assistant.take() else { return };
        if let Some(block) = self.blocks.iter_mut().find(|b| b.id == id) {
            block.complete = true;
            if interrupted && !block.content.ends_with('\n') {
                block.content.push('\n');
            }
        }
    }
}

enum Command {
    Emit(String),
    AssistantDelta(String),
    AssistantFinish { interrupted: bool },
    Input(InputSnapshot),
    Working(bool),
    Status(String),
    Context(u16),
    Tick,
    OpenOverlay(OverlaySnapshot),
    CloseOverlay,
    Scroll(i32),
    ScrollEnd,
    Focus(bool),
    Suspend(Sender<()>),
    Resume { status: String, ack: Sender<bool> },
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
        execute!(stdout, EnterAlternateScreen, Hide)?;
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend)?;
        terminal.clear()?;
        Ok(Self { terminal })
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = self.terminal.show_cursor();
        let _ = execute!(io::stdout(), Show, LeaveAlternateScreen);
    }
}

pub(super) fn preferred() -> bool {
    if !io::stdout().is_terminal() || crate::core::cli_config::branded_flag("NO_STICKY") {
        return false;
    }
    !matches!(crate::core::cli_config::load().tui_mode(), crate::core::cli_config::TuiMode::Classic)
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
    // `\x1b[?25h` show cursor · `\x1b[?1049l` leave alternate screen (crossterm's EnterAlternateScreen
    // uses the same private mode). Written unconditionally — if we never entered the alt screen the
    // terminal ignores the leave.
    let _ = write!(io::stdout(), "\x1b[?25h\x1b[?1049l");
    let _ = io::stdout().flush();
}

pub(super) fn is_running() -> bool {
    runtime_slot().lock().unwrap_or_else(|e| e.into_inner()).is_some()
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
    let ready = ready_rx.recv_timeout(Duration::from_secs(2)).unwrap_or(false);
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
    let handle = runtime_slot().lock().unwrap_or_else(|e| e.into_inner()).take();
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
    let _ = tx.send(Command::Resume { status: status.to_string(), ack: ack_tx });
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

pub(super) fn set_working(working: bool) {
    send(Command::Working(working));
}

pub(super) fn set_status(status: &str) {
    send(Command::Status(status.to_string()));
}

pub(super) fn set_context(permille: u16) {
    send(Command::Context(permille.min(1000)));
}

pub(super) fn tick() {
    send(Command::Tick);
}

pub(super) fn open_overlay(overlay: OverlaySnapshot) {
    send(Command::OpenOverlay(overlay));
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
                other => {
                    apply_command(&mut state, other);
                    dirty = true;
                }
            }
        }
        if dirty {
            if let Some(s) = session.as_mut() {
                let before = s.terminal.size().ok();
                let _ = s.terminal.autoresize();
                let after = s.terminal.size().ok();
                if let Some(area) = after {
                    COLS.store(area.width.max(20), Ordering::Relaxed);
                    ROWS.store(area.height.max(8), Ordering::Relaxed);
                }
                let started = Instant::now();
                let _ = s.terminal.draw(|frame| draw(frame, &mut state));
                let rows = state
                    .blocks
                    .iter()
                    .map(|b| format!("{}:{}:{}", b.id, b.content.len(), b.complete))
                    .collect::<Vec<_>>();
                state.metrics.record(started.elapsed(), metrics::hash_rows(&rows), before != after);
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
                state.push_block(BlockKind::Generic, clean, true);
            }
        }
        Command::AssistantDelta(s) => state.push_assistant(&s),
        Command::AssistantFinish { interrupted } => state.finish_assistant(interrupted),
        Command::Input(input) => {
            state.input = input;
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
        Command::Tick => state.frame = state.frame.wrapping_add(1),
        Command::OpenOverlay(overlay) => {
            state.input.overlay = Some(overlay);
            state.overlay_scroll = 0; // a fresh overlay starts at the top
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
                    state.overlay_scroll = state.overlay_scroll.saturating_add(delta.unsigned_abs() as usize);
                } else {
                    state.overlay_scroll = state.overlay_scroll.saturating_sub(delta as usize);
                }
            } else if delta < 0 {
                state.scroll_from_tail = state.scroll_from_tail.saturating_add(delta.unsigned_abs() as usize);
            } else {
                state.scroll_from_tail = state.scroll_from_tail.saturating_sub(delta as usize);
            }
        }
        Command::ScrollEnd => {
            // End/Home resets whichever surface is active: the open overlay, else the transcript.
            if state.input.overlay.is_some() {
                state.overlay_scroll = 0;
            } else {
                state.scroll_from_tail = 0;
            }
        }
        Command::Focus(v) => state.focused = v,
        Command::Suspend(_) | Command::Resume { .. } | Command::Shutdown(_) => {}
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
    let content_width = area.width.saturating_sub(2).max(8);
    let mut lines: Vec<Line<'static>> = Vec::new();
    for block in &state.blocks {
        let rows = state.cache.get_or_render(block, content_width);
        for row in rows {
            lines.push(styled_row(block.kind, row));
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
}

fn styled_row(kind: BlockKind, row: String) -> Line<'static> {
    match kind {
        // Intro is sanitised plain (no SGR) → one flat dim line, as before.
        BlockKind::Intro => Line::styled(row, Style::default().fg(Color::DarkGray)),
        // Assistant + Generic carry SGR now: parse it into coloured spans over a grey base. The
        // moonlight `▌` gutter and the state-tinted `◆` tool anchor keep their own colour because
        // it rode through as SGR; uncoloured text collapses to one grey span (unchanged look).
        BlockKind::Assistant | BlockKind::Generic => {
            Line::from(ansi_spans(&row, Style::default().fg(Color::Gray)))
        }
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
    let elapsed = state.working_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
    let indicator = if state.working {
        const STAR: [&str; 6] = ["✶", "✷", "✸", "✹", "✺", "✻"];
        format!("{} {}s", STAR[state.frame % STAR.len()], elapsed)
    } else {
        "● ready".to_string()
    };
    let pct = state.ctx_permille / 10;
    let right = format!("{indicator}  {pct}%");
    let right_w = console::measure_text_width(&right);
    let left_budget = width.saturating_sub(right_w + 1);
    let left = console::truncate_str(&state.input.status, left_budget, "…").into_owned();
    let gap = width.saturating_sub(console::measure_text_width(&left) + right_w);
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(left, Style::default().fg(Color::DarkGray)),
            Span::raw(" ".repeat(gap)),
            Span::styled(
                right,
                Style::default().fg(if state.working {
                    Color::Indexed(crate::ui::theme::ACCENT)
                } else {
                    Color::Indexed(crate::ui::theme::OK)
                }),
            ),
        ])),
        rows[0],
    );
    let rule = "─".repeat(width);
    frame.render_widget(
        Paragraph::new(Line::styled(
            rule.clone(),
            Style::default().fg(Color::Indexed(crate::ui::theme::ACCENT_DIM)),
        )),
        rows[1],
    );

    let (shown, cursor_off) = input_line(state, width.saturating_sub(3));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                "❯ ",
                Style::default()
                    .fg(Color::Indexed(crate::ui::theme::ACCENT))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(shown, Style::default().fg(Color::White)),
        ])),
        rows[2],
    );
    frame.render_widget(
        Paragraph::new(Line::styled(
            rule,
            Style::default().fg(Color::Indexed(crate::ui::theme::ACCENT_DIM)),
        )),
        rows[3],
    );
    let cursor_x = rows[2].x.saturating_add(2).saturating_add(cursor_off as u16);
    frame.set_cursor_position((cursor_x.min(rows[2].right().saturating_sub(1)), rows[2].y));
}

fn input_line(state: &AppState, budget: usize) -> (String, usize) {
    if state.input.draft.is_empty() {
        let q = if state.input.queued_count > 0 {
            format!(" · {} queued", state.input.queued_count)
        } else {
            String::new()
        };
        let ph = if state.working { format!("Queue a message · Esc stops{q}") } else { format!("Type a message · / commands{q}") };
        return (console::truncate_str(&ph, budget, "…").into_owned(), 0);
    }
    if state.input.draft.contains(&'\n') {
        let text: String = state.input.draft.iter().collect();
        let n = text.lines().count().max(1);
        let chip = format!("↵ {n} lines pasted");
        let w = console::measure_text_width(&chip);
        return (console::truncate_str(&chip, budget, "…").into_owned(), w.min(budget));
    }
    let cursor = state.input.cursor.min(state.input.draft.len());
    let cellw = |c: char| console::measure_text_width(&c.to_string()).max(1);
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
        shown.push(c);
        used += cw;
    }
    (shown, caret)
}

/// Draw the informational overlay, applying `scroll` (rows hidden above the top) clamped so the last
/// page is the furthest you can go. Returns the CLAMPED scroll so the caller can write it back — a
/// PageDown past the end then reads as "at the bottom" rather than drifting into empty space.
fn draw_overlay(frame: &mut Frame<'_>, area: Rect, overlay: &OverlaySnapshot, scroll: usize) -> usize {
    let width = area.width.saturating_sub(4).min(84).max(20);
    let height = (overlay.lines.len() as u16 + 4).min(area.height.saturating_sub(2)).max(5);
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
                if selected { format!("› {line}") } else { format!("  {line}") },
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
        lines.push(Line::styled(overlay.hint.clone(), Style::default().fg(Color::DarkGray)));
    }
    // Clamp scroll so the final page is the furthest reachable position (never scroll past the end).
    let visible = inner.height as usize;
    let max_scroll = lines.len().saturating_sub(visible);
    let clamped = scroll.min(max_scroll);
    frame.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).scroll((clamped.min(u16::MAX as usize) as u16, 0)),
        inner,
    );
    clamped
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
    let codes: Vec<u16> =
        params.split(';').filter(|s| !s.is_empty()).filter_map(|s| s.parse::<u16>().ok()).collect();
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
            22 => style = style.remove_modifier(Modifier::BOLD).remove_modifier(Modifier::DIM),
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

fn hash_text(s: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
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
        assert_eq!(spans[1].style.fg, Some(Color::Red), "the middle span is red");
        assert_eq!(spans[2].style.fg, Some(Color::Gray), "reset returns to base");
    }

    #[test]
    fn ansi_spans_reads_256_colour() {
        // The app emits 256-colour SGR (`\x1b[38;5;Nm`) for its accent/ok/err palette — parse it.
        let spans = ansi_spans("\x1b[38;5;71mgreen\x1b[0m", Style::default().fg(Color::Gray));
        assert_eq!(spans[0].style.fg, Some(Color::Indexed(71)));
    }

    #[test]
    fn assistant_cache_is_width_and_content_keyed() {
        let mut cache = RenderCache::default();
        let mut block = UiBlock { id: 7, kind: BlockKind::Assistant, content: "hello world".into(), complete: false };
        let a = cache.get_or_render(&block, 40);
        let b = cache.get_or_render(&block, 40);
        assert_eq!(a, b);
        assert_eq!(cache.hits, 1);
        block.content.push('!');
        let _ = cache.get_or_render(&block, 40);
        let _ = cache.get_or_render(&block, 20);
        assert_eq!(cache.misses, 3);
    }

    #[test]
    fn pruning_keeps_whole_blocks() {
        let mut state = AppState::new("intro", "status");
        for i in 0..BLOCK_LIMIT + 20 {
            state.push_block(BlockKind::Generic, format!("line-{i}"), true);
        }
        assert_eq!(state.blocks.len(), BLOCK_LIMIT);
        assert!(state.blocks.iter().all(|b| !b.content.is_empty()));
    }

    #[test]
    fn scroll_routes_to_open_overlay_not_the_transcript() {
        let mut state = AppState::new("intro", "status");
        // No overlay → scroll moves the transcript.
        apply_command(&mut state, Command::Scroll(-3));
        assert_eq!(state.scroll_from_tail, 3);
        assert_eq!(state.overlay_scroll, 0);

        // Open an overlay → scroll now moves ITS content, leaving the transcript offset untouched.
        apply_command(&mut state, Command::OpenOverlay(OverlaySnapshot {
            title: "info".into(),
            lines: (0..40).map(|i| format!("row {i}")).collect(),
            selected: None,
            hint: String::new(),
        }));
        assert_eq!(state.overlay_scroll, 0, "a fresh overlay starts at the top");
        apply_command(&mut state, Command::Scroll(-5));
        assert_eq!(state.overlay_scroll, 5, "overlay scrolled");
        assert_eq!(state.scroll_from_tail, 3, "transcript offset is left alone while the overlay is up");

        // Home/End resets the overlay while it's open.
        apply_command(&mut state, Command::ScrollEnd);
        assert_eq!(state.overlay_scroll, 0);
        assert_eq!(state.scroll_from_tail, 3);

        // Closing the overlay hands scroll back to the transcript.
        apply_command(&mut state, Command::CloseOverlay);
        apply_command(&mut state, Command::Scroll(-2));
        assert_eq!(state.scroll_from_tail, 5);
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
        assert_eq!(offset, 40, "clamped to tail_start (50 - 10), not left at 9999");
        assert_eq!(start, 0, "pinned at the very top");

        // Fewer lines than the viewport: nothing to scroll, offset collapses to 0.
        let (start, offset, _) = resolve_transcript_scroll(3, 4, 4, 10);
        assert_eq!(offset, 0);
        assert_eq!(start, 0);
    }
}
