//! What every OTHER thread is allowed to do to the screen: start/stop the render thread, and send
//! it one command.
//!
//! Nothing here draws. Each function packages a `Command` and returns immediately, so a caller can
//! never block on the terminal, and the render thread stays the single owner of the alternate
//! screen (see the module doc on the parent).

use super::*;

struct RuntimeHandle {
    pub(crate) tx: Sender<Command>,
    pub(crate) join: JoinHandle<()>,
}

fn runtime_slot() -> &'static Mutex<Option<RuntimeHandle>> {
    static RUNTIME: OnceLock<Mutex<Option<RuntimeHandle>>> = OnceLock::new();
    RUNTIME.get_or_init(|| Mutex::new(None))
}

pub(crate) struct TerminalSession {
    pub(crate) terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl TerminalSession {
    pub(crate) fn enter() -> io::Result<Self> {
        let mut stdout = io::stdout();
        // EnableMouseCapture: the terminal reports wheel/click/drag as crossterm mouse events
        // instead of leaking the wheel through as ↑/↓ keys (Windows Terminal "alternateScroll").
        // EnableBracketedPaste: terminals that support it deliver a paste as one Event::Paste
        // instead of a burst of key events — one insert, one repaint, no coalesce lag.
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            Hide
        )?;
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
            DisableBracketedPaste,
            DisableMouseCapture,
            Show,
            LeaveAlternateScreen
        );
    }
}

pub(crate) fn preferred() -> bool {
    // Retained is the ONLY interactive UI, so this is simply "can we take the terminal at all". When
    // it says no (piped output, CI, dumb terminals, or an explicit `NO_STICKY`) the caller runs the
    // plain line-REPL — there is no second renderer to pick, and nothing in the config selects one.
    io::stdout().is_terminal() && !crate::core::cli_config::branded_flag("NO_STICKY")
}

pub(crate) fn is_active() -> bool {
    ACTIVE.load(Ordering::Relaxed)
}

/// Panic/Ctrl-C fail-safe: put the terminal back to a sane state WITHOUT locking the runtime or the
/// render thread. Writes the raw escape sequences directly (show cursor, leave alternate screen) so
/// it stays safe to call from a panic hook, where the render thread may hold poisoned locks or be the
/// thread that panicked. Idempotent: doing it twice just re-emits harmless sequences. Deliberately
/// does NOT `join` the render thread — a panic hook must never block.
pub(crate) fn emergency_restore() {
    ACTIVE.store(false, Ordering::Relaxed);
    // `\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l` disable mouse reporting (all modes
    // EnableMouseCapture turns on) · `\x1b[?2004l` disable bracketed paste · `\x1b[?25h` show
    // cursor · `\x1b[?1049l` leave alternate screen. Written unconditionally — a terminal not in a
    // given mode just ignores the corresponding reset.
    let _ = write!(
        io::stdout(),
        "\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[?25h\x1b[?1049l"
    );
    let _ = io::stdout().flush();
}

pub(crate) fn is_running() -> bool {
    runtime_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_some()
}

pub(crate) fn size() -> (u16, u16) {
    (ROWS.load(Ordering::Relaxed), COLS.load(Ordering::Relaxed))
}

pub(crate) fn start(intro: &str, status: &str) -> bool {
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

pub(crate) fn stop() {
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

pub(crate) fn suspend() {
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

pub(crate) fn resume(status: &str) -> bool {
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
pub(super) fn terminal_size_changed() -> bool {
    match crossterm::terminal::size() {
        Ok((cols, rows)) => {
            let cols = cols.max(20);
            let rows = rows.max(8);
            cols != COLS.load(Ordering::Relaxed) || rows != ROWS.load(Ordering::Relaxed)
        }
        Err(_) => false,
    }
}

/// Refresh the published size from the real terminal without a ratatui frame.
///
/// The normal update rides on a paint (`autoresize` → `COLS`/`ROWS`), which cannot happen while the
/// renderer is suspended for a dialoguer menu: `session` is `None`, so the idle-tick probe is skipped
/// and the atomics keep their pre-menu values. Anything that lays itself out from `tui::width()`
/// during the menu — `print_config`'s rule and right-aligned path — then draws at the OLD width. This
/// is called on the way INTO a suspend (the size is still readable then) and again on the way out.
pub(super) fn publish_terminal_size() {
    if let Ok((cols, rows)) = crossterm::terminal::size() {
        COLS.store(cols.max(20), Ordering::Relaxed);
        ROWS.store(rows.max(8), Ordering::Relaxed);
    }
}

pub(super) fn send(cmd: Command) {
    let tx = runtime_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|h| h.tx.clone());
    if let Some(tx) = tx {
        let _ = tx.send(cmd);
    }
}

pub(crate) fn emit(s: &str) {
    send(Command::Emit(s.to_string()));
}

pub(crate) fn assistant_delta(s: &str) {
    send(Command::AssistantDelta(s.to_string()));
}

pub(crate) fn assistant_finish(interrupted: bool) {
    send(Command::AssistantFinish { interrupted });
}

pub(crate) fn update_input(input: InputSnapshot) {
    send(Command::Input(input));
}

/// Throw away ratatui's idea of what's on screen and repaint every cell from `AppState` (Ctrl-L).
///
/// The manual recovery hatch for the one failure this renderer cannot detect: something wrote to the
/// terminal behind its back (a stray `println!` from a subsystem, a child process that printed to the
/// inherited stdout). ratatui only repaints cells its diff believes changed, so foreign text stays
/// wedged inside later frames. Clearing first makes the next draw unconditional. Blocks live in
/// `AppState`, so this is a repaint, not a replay — no transcript is lost.
pub(crate) fn redraw() {
    send(Command::Redraw);
}

pub(crate) fn set_working(working: bool) {
    send(Command::Working(working));
}

pub(crate) fn set_status(status: &str) {
    send(Command::Status(status.to_string()));
}

/// Recolour the input box to gold (ultimate ON) or back to moonlight. Pushed once when the mode
/// toggles — never polled in the draw path (`cli_config::load()` reads the filesystem and the footer
/// repaints at ~9fps).
pub(crate) fn set_ultimate(on: bool) {
    send(Command::Ultimate(on));
}

/// Set the working caption target — a running tool's action ("Reading retained.rs") or the whimsical
/// verb between steps. The typewriter reveal replays only when the text actually changes.
pub(crate) fn set_work_caption(text: &str) {
    send(Command::WorkCaption(text.to_string()));
}

pub(crate) fn set_context(permille: u16) {
    send(Command::Context(permille.min(1000)));
}

pub(crate) fn set_health(kind: HealthKind) {
    send(Command::Health(kind));
}

pub(crate) fn set_selection(sel: SelectionRange) {
    // Mirror BEFORE queueing: the mirror is read by the input thread (right-click), and the command
    // queue is drained asynchronously by the render thread. Writing it here makes "what is selected"
    // true the instant the selection changes rather than one frame later.
    *live_selection_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(sel);
    send(Command::SetSelection(sel));
}

pub(crate) fn clear_selection() {
    *live_selection_slot()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = None;
    send(Command::ClearSelection);
}

/// Jump the transcript viewport so absolute wrapped-line `start` is at the top (scrollbar drag).
pub(crate) fn scroll_to(start: usize) {
    send(Command::ScrollTo(start));
}

/// Push (state `Running`) or update-in-place (state `Ok`/`Err`, matched by `seq`) a tool-call line.
pub(crate) fn tool_event(ev: ToolEvent) {
    send(Command::Tool(ev));
}

/// Replace the in-place plan checklist box with a fresh snapshot (empty → removes the box).
pub(crate) fn plan_update(rows: Vec<PlanRow>) {
    send(Command::Plan(rows));
}

/// Push a boxed diff preview under the most recent edit.
pub(crate) fn diff_box(d: DiffPayload) {
    send(Command::Diff(d));
}

/// Push a green verify-gate success line.
pub(crate) fn verify_line(v: VerifyPayload) {
    send(Command::Verify(v));
}

pub(crate) fn tick() {
    send(Command::Tick);
}

pub(crate) fn open_overlay(overlay: OverlaySnapshot) {
    send(Command::OpenOverlay(overlay));
}

/// Refresh an already-open overlay's body in place, keeping the reader's scroll offset.
pub(crate) fn update_overlay(lines: Vec<String>) {
    send(Command::UpdateOverlay(lines));
}

pub(crate) fn close_overlay() {
    send(Command::CloseOverlay);
}

pub(crate) fn scroll(delta: i32) {
    send(Command::Scroll(delta));
}

pub(crate) fn scroll_end() {
    send(Command::ScrollEnd);
}

/// Raise (`Some(idx)`) or clear (`None`) the idle screensaver. The render thread cover-encodes card
/// `idx` to its own pixel size and blits it fullscreen, restoring the transcript on clear.
pub(crate) fn screensaver(card: Option<usize>) {
    send(Command::Screensaver(card));
}

#[allow(dead_code)]
fn set_focus(focused: bool) {
    send(Command::Focus(focused));
}
