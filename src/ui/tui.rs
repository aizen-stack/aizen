//! Sticky-footer interactive TUI for the bare-`ng` REPL: a chat input box **pinned to the bottom**
//! of the terminal that stays visible even while the agent is working, with three properties the
//! plain line-REPL can't give:
//!
//! 1. **Pinned sandwich prompt** — an ANSI scroll region (`ESC[{top};{bot}r`) reserves the bottom
//!    rows for a sandwich-style footer (HUD above · top rule · the moonlit `❯` prompt · bottom rule —
//!    horizontal borders only around the input row); all agent output scrolls in the region *above* it,
//!    so the prompt never scrolls away and never stacks up.
//! 2. **Continuous chat** — a background thread owns the keyboard and pushes each submitted line onto
//!    an unbounded queue. You can keep typing (and queue messages) while the agent runs; the REPL
//!    drains the queue and auto-fires the next one when the current turn finishes.
//! 3. **Cancel** — Esc / Ctrl-C while the agent is working sends a cancel signal; the REPL drops the
//!    in-flight turn (aborting the streaming HTTP request) and returns you to the prompt.
//!
//! Output coordination: a single render `Mutex` serialises every terminal write. The agent's
//! streaming output and tool traces go through [`emit`]/[`emit_line`] (which restore the saved output
//! cursor, print, re-save, then repaint the box); the input thread repaints the box on each keypress.
//! When the TUI isn't active (the one-shot `aizen chat`/`agent` subcommands, pipes, CI) every entry
//! point degrades to a plain `print!` so nothing changes for non-interactive use.

use crate::ui::theme;
use console::{style, Key, Term};
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc as stdmpsc;
use std::sync::{Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::sync::oneshot;

mod retained;

/// A key whose `read_key()` returned within this many ms was ALREADY waiting in the OS input buffer
/// → it arrived as part of a burst (a paste), not a deliberate human keystroke. Used so a newline
/// *inside* a paste becomes a literal newline in the draft instead of submitting the line — the fix
/// for a multi-line paste firing one message per line. Comfortably above buffered-read scheduling
/// jitter (a few ms) yet far below the gap before a human reaches the Enter key (≥ ~100 ms).
const PASTE_COALESCE_MS: u64 = 50;

/// Idle seconds before the screensaver card is raised (retained backend only). Reset by any key or
/// mouse event; gated on !working and no open menu/overlay so it never fires mid-task or over a menu.
const IDLE_SCREENSAVER_SECS: u64 = 15;

/// Shared list + selection while the overlay is open (owned by the menu input thread).
static MODEL_MENU: OnceLock<Mutex<ModelMenuState>> = OnceLock::new();

#[derive(Clone)]
struct ModelMenuRow {
    id: String,
    label: String,
}

#[derive(Default)]
struct ModelMenuState {
    active: bool,
    sel: usize,
    rows: Vec<ModelMenuRow>,
    done_tx: Option<oneshot::Sender<Option<String>>>,
}

fn model_menu_slot() -> &'static Mutex<ModelMenuState> {
    MODEL_MENU.get_or_init(|| Mutex::new(ModelMenuState::default()))
}

/// Shared list + selection while the `/sessions` overlay is open (owned by the menu input thread).
static SESSIONS_MENU: OnceLock<Mutex<SessionsMenuState>> = OnceLock::new();

#[derive(Clone)]
struct SessionsMenuRow {
    /// Left-aligned primary label (pretty session name, or an action like "+ Save current…").
    title: String,
    /// Faint trailing detail ("12 msgs · 2 hr ago"), empty for action rows.
    subtitle: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionsMenuChoice {
    Pick(usize),
    Delete(usize),
}

#[derive(Default)]
struct SessionsMenuState {
    active: bool,
    sel: usize,
    rows: Vec<SessionsMenuRow>,
    /// Only the leading session rows are deletable; trailing Save/Back/confirmation actions are not.
    deletable_rows: usize,
    /// Resolves with Enter/Delete on a row, or `None` on Esc/cancel.
    done_tx: Option<oneshot::Sender<Option<SessionsMenuChoice>>>,
}

fn sessions_menu_slot() -> &'static Mutex<SessionsMenuState> {
    SESSIONS_MENU.get_or_init(|| Mutex::new(SessionsMenuState::default()))
}

/// Captured pure-print output while the temporary text overlay is open.
static TEXT_OVERLAY: OnceLock<Mutex<TextOverlayState>> = OnceLock::new();

#[derive(Default)]
struct TextOverlayState {
    active: bool,
    scroll: usize,
    title: String,
    lines: Vec<String>,
    done_tx: Option<oneshot::Sender<()>>,
}

fn text_overlay_slot() -> &'static Mutex<TextOverlayState> {
    TEXT_OVERLAY.get_or_init(|| Mutex::new(TextOverlayState::default()))
}

/// Slash commands matching the current draft. Empty unless the draft is a bare `/<prefix>` with no
/// space yet (once you type an argument the palette gets out of the way). Drawn from the shared
/// [`crate::features::slash`] catalog so the live palette, the bare-`/` picker, and `/help` never
/// drift apart — every executable command (built-in or custom) shows up here.
fn slash_matches(draft: &[char]) -> Vec<crate::features::slash::SlashCommand> {
    if draft.first() != Some(&'/') {
        return Vec::new();
    }
    let rest: String = draft[1..].iter().collect();
    if rest.chars().any(|c| c.is_whitespace()) {
        return Vec::new(); // argument phase → hide the palette
    }
    let typed = rest.to_lowercase();
    crate::features::slash::list()
        .into_iter()
        .filter(|c| c.name.starts_with(&typed))
        .collect()
}

/// File completions for the `@` picker. Fires when the draft contains `@<prefix>` at the cursor
/// (word boundary, not inside an email). Returns at most 12 matching paths relative to cwd,
/// sorted: exact-prefix matches first, then fuzzy. Empty when no `@` token is at the cursor.
fn at_matches(draft: &[char]) -> Vec<String> {
    // Find the last `@` that is at a word boundary (preceded by whitespace or start-of-draft).
    // We search backward from the cursor end so typing more chars narrows the list in real time.
    let s: String = draft.iter().collect();
    // Locate the last `@` preceded by start or whitespace.
    let at_pos = s
        .char_indices()
        .rev()
        .find(|&(i, c)| {
            c == '@' && (i == 0 || s[..i].chars().last().map_or(true, |p| p.is_whitespace()))
        })
        .map(|(i, _)| i);
    let at_pos = match at_pos {
        Some(p) => p,
        None => return Vec::new(),
    };
    // The prefix is everything after the `@` up to the end of draft (cursor always at end for this).
    let prefix: String = s[at_pos + 1..]
        .chars()
        .take_while(|c| !c.is_whitespace())
        .collect();
    // Avoid triggering on obvious non-path patterns like `@everyone`.
    // We return results even for an empty prefix (show recent/top files) but cap at 12.
    const LIMIT: usize = 12;
    let root = match std::env::current_dir() {
        Ok(d) => d,
        Err(_) => return Vec::new(),
    };
    // Walk up to ~2000 entries from cwd, collect relative paths that match the prefix.
    let lower_prefix = prefix.to_lowercase();
    // Use WalkDir-equivalent via std::fs recursive helper — no new dep.
    fn collect_files(
        dir: &std::path::Path,
        root: &std::path::Path,
        depth: u8,
        out: &mut Vec<String>,
    ) {
        if depth == 0 {
            return;
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            let ft = match entry.file_type() {
                Ok(t) => t,
                Err(_) => continue,
            };
            // Skip hidden dirs and known noise dirs.
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with('.')
                || matches!(name_str.as_ref(), "target" | "node_modules" | "__pycache__")
            {
                continue;
            }
            if ft.is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    out.push(rel.to_string_lossy().replace('\\', "/"));
                }
            } else if ft.is_dir() {
                collect_files(&path, root, depth - 1, out);
            }
            if out.len() >= 2000 {
                return;
            }
        }
    }
    let mut all_files: Vec<String> = Vec::new();
    collect_files(&root, &root, 4, &mut all_files);

    if lower_prefix.is_empty() {
        // No prefix yet — show the most recently-modified files (up to LIMIT).
        // Simple heuristic: just take the first LIMIT from the walk (already breadth-first-ish).
        all_files.into_iter().take(LIMIT).collect()
    } else {
        // Exact prefix matches first, then substring matches.
        let exact: Vec<_> = all_files
            .iter()
            .filter(|p| p.to_lowercase().contains(&lower_prefix))
            .cloned()
            .collect();
        // Sort: paths whose filename starts with prefix first.
        let mut scored: Vec<(usize, &String)> = exact
            .iter()
            .map(|p| {
                let fname = p.rsplit('/').next().unwrap_or(p);
                let score = if fname.to_lowercase().starts_with(&lower_prefix) {
                    0
                } else {
                    1
                };
                (score, p)
            })
            .collect();
        scored.sort_by_key(|(s, _)| *s);
        scored
            .into_iter()
            .map(|(_, p)| p.clone())
            .take(LIMIT)
            .collect()
    }
}

/// Whether a direct retained informational overlay (`/workflows`, later panels) is open.
static RETAINED_INFO_OVERLAY: AtomicBool = AtomicBool::new(false);

/// Whether the sticky TUI currently owns the terminal (gates `emit`'s behaviour + spinner suppression).
static ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether the agent is mid-turn. Set by the REPL around a turn; read by the input thread (Esc =
/// cancel when working, quit when idle) and by the ticker (it only animates while a turn runs). The
/// footer's own working pill is driven by the render thread's `AppState`, fed via [`set_working`].
static WORKING: AtomicBool = AtomicBool::new(false);

/// Whether the REPL currently owns stdin for a `dialoguer` menu — set by [`suspend`], cleared by
/// [`resume`]. The input thread nurses this on every iteration and releases the keyboard (dropping
/// raw mode) for as long as it is set.
///
/// This flag replaced a park decision the input thread used to make on its own, by matching the
/// slash name against a table and then blocking on the resume channel. Two things went wrong with
/// that. The table was a *second* copy of `main.rs`'s interactive-command list and had drifted from
/// it, so a command the REPL never suspended for could still park the keyboard until some unrelated
/// resume signal arrived. And the decision read `WORKING` at the moment the key was pressed while
/// the REPL suspends at the moment it dequeues — type `/config` mid-turn and the two disagreed, so
/// the input thread kept reading keys (re-asserting raw mode every iteration) underneath the menu
/// that was trying to read them. Observing the real suspend/resume edges cannot drift and cannot
/// deadlock: nothing blocks forever waiting for a signal that no longer matches.
static KEYBOARD_PARKED: AtomicBool = AtomicBool::new(false);

/// Acknowledgement for the flag above: set by the input thread once it has actually left the read
/// path and dropped raw mode, cleared when it takes the keyboard back. [`suspend`] waits on this
/// (bounded) so a `dialoguer` menu never opens while the reader is still inside its 1s `event::poll`
/// and would consume one more key — or re-assert raw mode — underneath the menu.
static KEYBOARD_RELEASED: AtomicBool = AtomicBool::new(false);

/// Serializes tests that arm/cancel the process-global turn slot below.
///
/// `ACTIVE_TURN_CANCEL` is one slot for the whole process, and `request_cancel` cancels whatever
/// happens to be in it. Two tests exercising cancellation at once would therefore cancel each
/// other's token — a real race, not a theoretical one, since cargo runs tests in parallel threads.
#[cfg(test)]
pub(crate) static TEST_CANCEL_LOCK: Mutex<()> = Mutex::new(());

/// Turn-scoped cancellation handle currently armed by the interactive REPL.
///
/// Unlike the old process-global latch, this slot only points at the active logical turn. Children
/// inherit the same token through `AgentConfig`; unrelated turns/tests own different tokens. The slot
/// is disarmed by token identity, so a late completion cannot clear a newer turn.
static ACTIVE_TURN_CANCEL: OnceLock<Mutex<Option<crate::core::cancel::TurnCancel>>> =
    OnceLock::new();

fn active_turn_cancel() -> &'static Mutex<Option<crate::core::cancel::TurnCancel>> {
    ACTIVE_TURN_CANCEL.get_or_init(|| Mutex::new(None))
}

/// Arm cancellation for one interactive turn.
pub fn arm_cancel(token: crate::core::cancel::TurnCancel) {
    *active_turn_cancel()
        .lock()
        .unwrap_or_else(|e| e.into_inner()) = Some(token);
}

/// Disarm only when the slot still refers to this turn (a completed old turn cannot clear a new one).
pub fn disarm_cancel(token: &crate::core::cancel::TurnCancel) {
    let mut slot = active_turn_cancel()
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if slot.as_ref().is_some_and(|active| active.same_turn(token)) {
        *slot = None;
    }
}

/// Request cancellation of the in-flight interactive turn (called by the input thread on Esc).
pub fn request_cancel() {
    let token = active_turn_cancel()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Some(token) = token {
        token.cancel();
    }
}

/// Current interactive token, exposed to synchronous pollers outside a tool scope.
pub fn active_cancel_token() -> Option<crate::core::cancel::TurnCancel> {
    active_turn_cancel()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Is there cancellable work in flight? `true` when the working pill is up OR a cancel token is
/// armed — and Esc keys off THIS, never off `WORKING` alone.
///
/// The two are not the same window. Between dequeuing a submission and flipping `WORKING`, the REPL
/// does real, slow work: prompt-lane rebuild, codebase retrieval, a recovery checkpoint, LSP arming,
/// registry construction. `WORKING` is still false for all of it, so an Esc pressed there used to
/// fall through to the idle branch and merely clear the draft — the turn then started anyway. That
/// window is per-queued-message, which is why it bit hardest while a queue was draining. Arming the
/// token first and testing it here makes Esc live for the whole turn, prep included.
pub fn turn_in_flight() -> bool {
    WORKING.load(Ordering::Relaxed)
        || active_turn_cancel()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
}

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
    "`/approval smart` auto-runs read-only tools; `yolo` pre-authorizes the rest",
    "PgUp/PgDn scrolls back through the transcript, End returns to the live tail",
    "drag over transcript text to copy it; Ctrl-C copies the highlight, or the draft you typed",
    "click in the input box to move the caret; drag over it to select & copy what you typed",
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

/// Rotating cursor for the per-turn working verb, advanced once per turn so each run opens on a fresh
/// word. The verb is emitted into the transcript as a turn-start line; the footer's own shimmering
/// verb is picked independently by the render thread, so this cursor only orders the transcript ones.
static VERB_CURSOR: AtomicUsize = AtomicUsize::new(0);

/// The next working verb (e.g. "Pondering"). Emitted once per turn into the transcript by the REPL —
/// see the turn-start line in `run_menu_sticky`.
pub fn next_work_verb() -> &'static str {
    VERBS[VERB_CURSOR.fetch_add(1, Ordering::Relaxed) % VERBS.len()]
}

/// Provider reachability for the idle `●` chip. Green = answered fast; yellow = flaky/slow;
/// red = permanent unavailability (bad key/endpoint or missing config).
///
/// The live value lives in the render thread's `AppState` (fed by [`set_health`]) — there is no
/// second copy here, so the chip can never disagree with what was drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum HealthKind {
    /// `GET /models` succeeded within the slow threshold.
    Ok = 0,
    /// Transient error (429/5xx/timeout/transport) OR success slower than the slow threshold.
    Unstable = 1,
    /// Permanent failure: 400/401/403/404, missing config, or endpoint unreachable as a client error.
    Down = 2,
    /// No probe result yet (boot / first poll in flight).
    Unknown = 3,
}

impl HealthKind {
    /// Footer label. Narrow terminals get a short form so the HUD still fits.
    pub fn label(self, narrow: bool) -> &'static str {
        match (self, narrow) {
            (Self::Ok, true) => "ok",
            (Self::Ok, _) => "ready",
            (Self::Unstable, true) => "slow",
            (Self::Unstable, _) => "unstable",
            (Self::Down, true) => "down",
            (Self::Down, _) => "down",
            (Self::Unknown, true) => "…",
            (Self::Unknown, _) => "checking",
        }
    }

    /// 256-colour index for the `●` (and the retained right-hand chip).
    pub fn color_code(self) -> u8 {
        match self {
            Self::Ok => theme::OK,
            Self::Unstable => theme::WARN,
            Self::Down => theme::ERR,
            Self::Unknown => theme::MUTED,
        }
    }
}

/// Update the context-meter fill (per-mille, clamped 0..=1000). Called from `status_text` alongside
/// each status refresh; harmless when the TUI is inactive.
pub fn set_ctx_permille(v: u16) {
    if retained::is_running() {
        retained::set_context(v.min(1000));
    }
}

/// Push a new health reading into the idle footer chip. Harmless when the TUI is inactive.
pub fn set_health(kind: HealthKind) {
    if retained::is_running() {
        retained::set_health(kind);
    }
}

/// Guards the single ticker thread so it's spawned at most once per process.
static TICKER_STARTED: AtomicBool = AtomicBool::new(false);

/// Rough count of streamed OUTPUT characters this turn (÷4 ≈ tokens), zeroed at each turn start.
/// The retained HUD currently shows the elapsed clock rather than a token tally, so nothing reads
/// this yet; it is kept because the streaming client already feeds it per content delta and it is the
/// only per-turn output volume signal available to a future HUD chip.
static STREAM_CHARS: AtomicU64 = AtomicU64::new(0);

/// Bump the streamed-output character counter — called by the streaming client per content delta.
/// A cheap relaxed add; harmless off-TTY.
pub fn add_stream_chars(n: u64) {
    STREAM_CHARS.fetch_add(n, Ordering::Relaxed);
}

/// Feed raw assistant Markdown into the retained active-message block. Classic/one-shot callers keep
/// using [`emit`] with the existing streaming Markdown renderer.
pub fn assistant_stream_delta(s: &str) {
    if retained::is_active() {
        retained::assistant_delta(s);
    }
}

/// Close the retained active assistant block at a clean message boundary.
pub fn assistant_stream_finish(interrupted: bool) {
    if retained::is_running() {
        retained::assistant_finish(interrupted);
    }
}

/// Spawn the lone animation ticker (idempotent). While the agent is working it pokes the render
/// thread ~9×/s so the spinner animates and the elapsed counter ticks even when no output is
/// streaming. Idle (not working) → it just sleeps; on a pipe/CI it never spawns.
///
/// The frame counter itself lives in the render thread's `AppState` (advanced by [`retained::tick`]),
/// so this thread only supplies the heartbeat.
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
        if retained::is_active() {
            retained::tick();
        }
    });
}

/// Service a status-panel command on the INPUT THREAD while a turn is in flight. Returns true when the
/// command was fully handled here and must NOT be queued.
///
/// This is the fourth mid-turn entry point, for exactly the same reason `>` steer and `?` aside are
/// the second and third: the REPL's turn `select!` polls only the turn future and the cancel channel,
/// so an ordinary queued slash command is not dequeued until the turn ENDS. For this one command that
/// makes it useless twice over — a stop that lands after the run it targeted has already finished is
/// no stop at all, and a self-refreshing activity panel you can only open once the fan-out is over has
/// nothing left to show.
///
/// Servicing it here is safe precisely because it touches nothing the REPL owns: reading the
/// orchestration registry and raising a cancel flag are both process-global and lock-guarded, and the
/// overlay is already driven from this thread — it is the one that closes it on Esc. While IDLE the
/// command stays on the queue, where suspend/park semantics remain the REPL's business.
fn handle_status_command_inline(name: &str, arg: &str) -> bool {
    if !turn_in_flight() || !crate::agent::orchestration::is_status_command(&name.to_lowercase()) {
        return false;
    }
    if let Some(note) = crate::agent::orchestration::try_stop_command(arg) {
        note_line(&theme::muted(note).to_string());
        return true;
    }
    // A bare `/workflows`: open the live panel from here. A `false` return (no retained backend — a
    // pipe/CI, or the box is currently suspended for a menu) falls through to the queue rather than
    // printing over a surface this thread does not own.
    retained_overlay_open_live("Activity", crate::agent::orchestration::format_status)
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

/// Shared input state behind the global lock: the draft buffer plus which overlay is open. The input
/// thread owns editing semantics and mutates this; [`retained_input_snapshot`] translates it into one
/// `InputSnapshot` per frame for the render thread. Deliberately holds NO geometry — the retained
/// backend is the only thing that paints, so it is the only authority on terminal size (see
/// [`width`]); a second copy here could disagree with what was actually drawn.
struct Render {
    draft: Vec<char>,
    cursor: usize,
    /// Mouse highlight inside the input box as `(anchor, cursor)` draft char indices, or `None` when
    /// nothing is highlighted. Written by the drag handler and by every key that invalidates it; read
    /// by the renderer (to paint it REVERSED) and by Ctrl-C (to copy just that much).
    draft_sel: Option<(usize, usize)>,
    images: usize,
    status: String,
    /// Highlighted row in the live slash palette (index into the current matches; 0 = nearest box).
    palette_sel: usize,
    /// Highlighted row in the `@file` picker (index into current file matches; 0 = top item).
    at_sel: usize,
    /// `/model` overlay above the footer (replaces the slash palette while open).
    model_menu_active: bool,
    model_menu_sel: usize,
    model_menu_rows: Vec<ModelMenuRow>,
    /// `/sessions` overlay above the footer (same slot as the model menu; only one is open at a time).
    sessions_menu_active: bool,
    sessions_menu_sel: usize,
    sessions_menu_rows: Vec<SessionsMenuRow>,
    sessions_menu_deletable_rows: usize,
    /// Temporary scrollable output for pure-print slash commands; Esc restores the prior transcript.
    text_overlay_active: bool,
    text_overlay_scroll: usize,
    text_overlay_title: String,
    text_overlay_lines: Vec<String>,
    /// Chat/slash submissions waiting while a turn runs (shown in the prompt placeholder).
    queued_count: usize,
}

fn render() -> &'static Mutex<Render> {
    static R: OnceLock<Mutex<Render>> = OnceLock::new();
    R.get_or_init(|| {
        Mutex::new(Render {
            draft: Vec::new(),
            cursor: 0,
            draft_sel: None,
            images: 0,
            status: String::new(),
            palette_sel: 0,
            at_sel: 0,
            model_menu_active: false,
            model_menu_sel: 0,
            model_menu_rows: Vec::new(),
            sessions_menu_active: false,
            sessions_menu_sel: 0,
            sessions_menu_rows: Vec::new(),
            sessions_menu_deletable_rows: 0,
            text_overlay_active: false,
            text_overlay_scroll: 0,
            text_overlay_title: String::new(),
            text_overlay_lines: Vec::new(),
            queued_count: 0,
        })
    })
}

/// Normalise an input-box highlight into a half-open `[start, end)` range of draft chars, clamped to a
/// draft of `len` chars.
///
/// One implementation for both sides of the module boundary: the renderer paints this range and Ctrl-C
/// copies it, and a highlight that is drawn differently from what gets copied is worse than no
/// highlight at all. `None` when there is nothing selected — a bare click leaves `anchor == cursor`,
/// which must NOT read as a one-char selection, and a stale range against a shorter draft (history
/// recall, a `/clear`) collapses to nothing rather than pointing at whatever now sits at that index.
fn normalized_draft_sel(sel: Option<(usize, usize)>, len: usize) -> Option<(usize, usize)> {
    let (a, b) = sel?;
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    let (lo, hi) = (lo.min(len), hi.min(len));
    (hi > lo).then_some((lo, hi))
}

/// The input-box highlight as a half-open draft range, if there is one.
fn draft_selection() -> Option<(usize, usize)> {
    let r = render().lock().unwrap();
    normalized_draft_sel(r.draft_sel, r.draft.len())
}

/// Drop the highlight without touching the text, repainting only if there was one to drop.
fn clear_draft_selection() {
    let had = {
        let mut r = render().lock().unwrap();
        r.draft_sel.take().is_some()
    };
    if had {
        repaint();
    }
}

/// Remove the highlighted chars from the draft and collapse the caret onto where they started.
///
/// No repaint of its own: every caller either repaints or is about to insert the char that replaces the
/// selection, and repainting the intermediate state would flash the deletion on screen.
fn delete_draft_selection() {
    let mut r = render().lock().unwrap();
    if let Some((a, b)) = normalized_draft_sel(r.draft_sel, r.draft.len()) {
        r.draft.drain(a..b);
        r.cursor = a;
        r.palette_sel = 0; // matches changed → reset the palette highlight to the nearest
        r.at_sel = 0;
    }
    r.draft_sel = None;
}

/// Park the caret at draft index `idx` and set (or clear) the highlight in the same repaint.
fn set_draft_caret(idx: usize, sel: Option<(usize, usize)>) {
    {
        let mut r = render().lock().unwrap();
        r.cursor = idx.min(r.draft.len());
        r.draft_sel = sel;
    }
    repaint();
}

/// Submissions not yet consumed by the REPL (incremented on keyboard send, decremented on recv).
static SUBMISSION_DEPTH: AtomicUsize = AtomicUsize::new(0);

/// Call when the input thread enqueues a chat or slash submission.
pub fn note_submission_enqueued() {
    let d = SUBMISSION_DEPTH.fetch_add(1, Ordering::Relaxed) + 1;
    render().lock().unwrap().queued_count = if WORKING.load(Ordering::Relaxed) {
        d
    } else {
        0
    };
    if WORKING.load(Ordering::Relaxed) && active() {
        repaint_force();
    }
}

/// Call when the REPL receives the next submission from the channel.
pub fn note_submission_dequeued() {
    let prev = SUBMISSION_DEPTH.fetch_sub(1, Ordering::Relaxed);
    let d = prev.saturating_sub(1);
    let show = if WORKING.load(Ordering::Relaxed) {
        d
    } else {
        0
    };
    render().lock().unwrap().queued_count = show;
    if active() {
        repaint_force();
    }
}

/// Clear depth after Esc flushes the backlog.
pub fn clear_submission_depth() {
    SUBMISSION_DEPTH.store(0, Ordering::Relaxed);
    render().lock().unwrap().queued_count = 0;
    if active() {
        repaint_force();
    }
}

pub fn active() -> bool {
    retained::is_active() || ACTIVE.load(Ordering::Relaxed)
}

/// Whether the retained full-frame backend currently owns the terminal.
pub fn retained_active() -> bool {
    retained::is_active()
}

/// Whether the retained backend's render thread is alive (true even while SUSPENDED for an
/// interactive dialoguer menu). The render thread keeps folding `Command::Emit` into its block
/// buffer while suspended (it just doesn't paint), and `resume` redraws from that buffer — so text
/// emitted during a suspended menu must be sent to the render thread, NOT `print!`ed onto the
/// dialoguer's screen (where `resume`'s clear+redraw would wipe it). Emit paths therefore route by
/// `is_running()`, not `is_active()`, so a `/sessions` restore replayed mid-menu survives resume.
pub fn retained_running() -> bool {
    retained::is_running()
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
    // Point the idle screensaver's context card at "Safe autonomy": a risky action just raised this
    // gate, so if the user steps away right after, the card reflects the guardrail they saw.
    crate::ui::cards::note_approval();
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

/// The width (columns) the frame is drawn at — the canonical wrap width for streamed output so the
/// Markdown renderer wraps to exactly the transcript viewport, not to a separately-probed (possibly
/// larger) window edge. Off-TTY / before the render thread is up, falls back to a live probe.
///
/// The render thread is the single source of truth here: it calls `autoresize` and stores the result,
/// so this can never disagree with what was actually painted (the old second copy in `Render.cols`
/// needed its own 250 ms poller to stay in step, and drifted between polls).
///
/// EXCEPT while suspended for a dialoguer menu: there is no frame then, so the stored size is frozen
/// at whatever was last painted and a window resized during the menu would lay out at the old width
/// (the config panel's rule and right-aligned path). While the renderer holds no screen, nothing has
/// been "actually painted" to disagree with, so a live probe is strictly better.
pub fn width() -> usize {
    if retained::is_running() && retained::is_active() {
        retained::size().1 as usize
    } else {
        term_size().1 as usize
    }
}

fn term_size() -> (u16, u16) {
    // console returns (rows, cols); fall back to a sane default if it can't probe.
    let (r, c) = Term::stdout().size();
    (r.max(8), c.max(20))
}

/// Start the interactive TUI: hand the terminal to the retained full-frame backend.
///
/// Returns whether it came up. `false` means the caller must fall back to the plain line-REPL —
/// either stdout isn't a TTY, or entering the alternate screen failed. There is no second renderer
/// to degrade into: the retained backend is the only interactive surface, so a half-started UI is
/// never left on screen.
pub fn activate(intro: &str, status: &str) -> bool {
    if !std::io::stdout().is_terminal() {
        return false;
    }
    // Seed the shared input state's status BEFORE starting the render thread: every keystroke
    // snapshot reads it, so skipping this makes the first keypress send an empty `InputSnapshot.
    // status` and blank the HUD's left side (model · tokens · yolo).
    {
        let mut r = render().lock().unwrap_or_else(|e| e.into_inner());
        r.status = status.to_string();
    }
    if !retained::start(intro, status) {
        return false;
    }
    ACTIVE.store(true, Ordering::Relaxed);
    true
}

/// Leave the TUI: stop the render thread (its `TerminalSession::drop` shows the cursor, disables
/// mouse capture, and leaves the alternate screen) and put stdin back in cooked mode.
pub fn deactivate() {
    // The crossterm input loop leaves stdin in raw mode; return it to cooked so the `bye.` line and the
    // shell prompt after us echo normally. Idempotent and safe even if the loop never enabled raw.
    restore_stdin_cooked();
    // Idempotent, and must still run when Windows delivers CTRL_C_EVENT before the keyboard thread
    // observes it — in that race another path may already have cleared `ACTIVE`.
    ACTIVE.store(false, Ordering::Relaxed);
    retained::stop();
}

/// Idempotent, lock-free terminal restore for the two paths that must NEVER hang: a panic unwinding
/// through the render thread, and a hard Ctrl-C. It writes escape sequences straight to stdout —
/// show cursor, leave the retained alternate screen, reset the classic scroll region — and resets
/// Windows stdin to cooked mode. It does NOT lock the render state, the runtime slot, or any mutex a
/// poisoned/panicking thread might hold, so it is safe to call from a panic hook. Callable any number
/// of times; a terminal that was never in a given mode ignores the corresponding reset.
pub fn emergency_restore() {
    // Retained backend: leave the alternate screen + show cursor without touching its runtime mutex.
    retained::emergency_restore();
    ACTIVE.store(false, Ordering::Relaxed);
    // Belt-and-braces for a terminal a crashed/killed child may have left in an odd state: reset any
    // DECSTBM scroll region and force the cursor visible. Two escapes, no locks, safe from a panic
    // hook — a terminal that was never in those modes just ignores them.
    let mut out = std::io::stdout();
    let _ = out.write_all(b"\x1b[r\x1b[?25h");
    let _ = out.flush();
    restore_stdin_cooked();
}

/// Install a one-time panic hook that restores the terminal BEFORE the default hook prints the panic
/// message — otherwise a panic inside retained/sticky mode dumps the backtrace into the alternate
/// screen (lost on exit) or onto a frame with a restricted scroll region (mangled). Chains the
/// previous hook so the normal panic report still runs. Idempotent via a `OnceLock` latch.
pub fn install_panic_hook() {
    static INSTALLED: OnceLock<()> = OnceLock::new();
    if INSTALLED.set(()).is_err() {
        return;
    }
    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        emergency_restore();
        prev(info);
    }));
}

/// Temporarily yield the terminal so a `dialoguer` slash menu can use stdin/redraw normally. The
/// input thread is already parked (it parks itself right after sending a `Slash`).
///
/// The render thread drops the alternate screen and stops painting, but keeps folding `Command::Emit`
/// into its block buffer — so output produced *during* the menu survives and [`resume`] redraws it.
pub fn suspend() {
    // Park the keyboard FIRST: the input thread must stop re-asserting raw mode before
    // `prepare_dialoguer_session` puts stdin back into cooked mode, or the two race and the menu
    // reads nothing. Set unconditionally (even when retained isn't running) so the plain REPL's
    // dialoguer menus get the same protection.
    KEYBOARD_PARKED.store(true, Ordering::SeqCst);
    // Then WAIT for the acknowledgement. Setting the flag isn't enough on its own: the input thread
    // can be sitting inside a 1s `event::poll`, so it would still consume one more key — and worse,
    // re-assert raw mode — after the menu had already taken the terminal. `KEYBOARD_RELEASED` is set
    // by the input thread only once it has actually dropped out of the read path. The deadline is the
    // safety valve: a missing ack (no input thread at all — the plain REPL, a pipe, tests) must never
    // hang the menu, so we cap the wait and proceed.
    let deadline = Instant::now() + Duration::from_millis(300);
    while !KEYBOARD_RELEASED.load(Ordering::SeqCst) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    if retained::is_running() {
        retained::suspend();
        prepare_dialoguer_session();
    }
}

/// Whether the input thread should stand down because a `dialoguer` menu owns stdin.
///
/// Read by tests only — the input thread reads `KEYBOARD_PARKED` directly on its own hot path rather
/// than through this accessor. It stays as the flag's documented reader so the park/release
/// handshake keeps a test-visible surface.
#[allow(dead_code)]
pub fn keyboard_parked() -> bool {
    KEYBOARD_PARKED.load(Ordering::SeqCst)
}

/// Called by the input thread to report whether it currently holds the keyboard. [`suspend`] waits on
/// this so a menu never opens while the reader is still mid-`poll`.
///
/// The input thread now stores `KEYBOARD_RELEASED` inline at its park/unpark points, so this setter
/// has no callers. Kept as the writing half of the handshake [`suspend`] blocks on.
#[allow(dead_code)]
pub(crate) fn note_keyboard_released(released: bool) {
    KEYBOARD_RELEASED.store(released, Ordering::SeqCst);
}

/// Re-enter the retained frame after a slash menu. The render thread re-enters the alternate screen
/// and repaints from its own block buffer, so the menu's leftover lines are discarded with the old
/// screen and anything emitted while suspended appears in the transcript.
pub fn resume(status: &str) {
    {
        let mut r = render().lock().unwrap_or_else(|e| e.into_inner());
        r.status = status.to_string();
    }
    if retained::is_running() {
        let _ = retained::resume(status);
    }
    // Hand the keyboard back LAST — after the retained frame is painted again, so the first keystroke
    // can't be read against a screen that isn't up yet. This is the release half of the pairing with
    // `suspend`: while the flag is set the input thread holds no stdin at all, so forgetting to clear
    // it here would wedge input permanently (every key ignored, no way to type or quit).
    KEYBOARD_PARKED.store(false, Ordering::SeqCst);
}

/// Whether an `emit` capture session is in progress. When set, `emit`/`emit_line` accumulate into
/// the capture buffer instead of writing to the scroll region / transcript.
static EMIT_CAPTURING: AtomicBool = AtomicBool::new(false);
/// Captured lines while `EMIT_CAPTURING` is on.
fn emit_capture_slot() -> &'static Mutex<Vec<String>> {
    static C: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(Vec::new()))
}

/// Route agent output to the retained transcript, or to plain stdout when the TUI doesn't own the
/// screen (the `chat`/`agent` one-shots, pipes, CI).
///
/// **Capture mode**: when [`emit_capture_begin`] has been called, output is accumulated into a
/// buffer instead of being written to the terminal / transcript. [`emit_capture_take`] drains it.
pub fn emit(s: &str) {
    if EMIT_CAPTURING.load(Ordering::Relaxed) {
        // Split multi-line output so each visual line is a separate overlay row. Preserve intentional
        // blank lines (`emit("\n")`) while removing only the line terminator added by `emit_line`.
        if !s.is_empty() {
            let body = s.strip_suffix('\n').unwrap_or(s);
            let mut cap = emit_capture_slot()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            for line in body.split('\n') {
                cap.push(line.to_string());
            }
        }
        return;
    }
    if retained::is_running() {
        // Route to the render thread even while SUSPENDED for a dialoguer menu: it folds this into
        // its block buffer (no paint yet), and `resume` redraws from that buffer. Printing straight
        // to the terminal here would be wiped by resume's clear+redraw (the "/sessions restore shows
        // nothing" bug). This is why emit routes on `is_running()`, NOT `is_active()`.
        retained::emit(s);
        return;
    }
    print!("{s}");
    let _ = std::io::stdout().flush();
}

/// `emit` a whole line.
pub fn emit_line(s: &str) {
    let mut line = String::with_capacity(s.len() + 1);
    line.push_str(s);
    line.push('\n');
    emit(&line);
}

// ── structured transcript events (the mockup redesign) ───────────────────────────
// Tool calls, the plan checklist, edit diffs, and the verify line are no longer pre-styled strings
// blindly `emit`ted: they flow through here as structured data so the retained backend can
// right-align digests, box the panels, and update the plan/tool line IN PLACE. When retained isn't
// running (classic / plain / one-shot), the SAME `retained::render_*` layout is rendered to a string
// at `width()` and emitted append-only — so every surface reads identically, degrading only where
// in-place updates are impossible (the plan simply re-prints, a too-narrow digest wraps to `└`).

/// The ONE funnel for out-of-band diagnostics — warnings, fallbacks, "skipping unreadable X" notes
/// raised deep in a subsystem that has no idea whether a TUI owns the screen.
///
/// Any such note MUST come through here rather than `println!`/`eprintln!`. A raw print lands
/// directly in the terminal while the retained render thread believes it still owns every cell;
/// ratatui then diffs against a cell buffer that no longer matches reality and only repaints cells
/// it thinks changed, so the injected text survives inside later frames — the character-level
/// interleaving and doubled rows that look like "the UI is corrupted". Routing through
/// [`emit_line`] instead makes the note a transcript block the renderer knows about.
///
/// Routes on `retained_running()`, not just `active()`, for the same reason [`emit`] does: while a
/// dialoguer menu has the TUI SUSPENDED the render thread still folds emissions into its block
/// buffer and `resume` redraws from it, so a note printed straight to the menu's screen would be
/// wiped. Outside the REPL (one-shot `aizen agent`, pipes, CI) it degrades to `eprintln!`, keeping
/// stdout clean for the model's answer.
pub fn note_line(s: &str) {
    if active() || retained_running() {
        emit_line(s);
    } else {
        eprintln!("{s}");
    }
}

/// Emit a trace line the way the agent's `emit_trace` does: into the sticky/retained scroll region
/// when the TUI owns the screen, else `eprintln!` to stderr so a one-shot `aizen agent` keeps stdout
/// clean (only the model's final answer belongs on stdout there).
fn emit_trace_line(s: &str) {
    note_line(s);
}

/// Outcome of a tool call, for the digest colour. `None` while it's still running.
pub type ToolOutcome = Option<bool>;

/// Monotonic id so a tool result can update the same line it opened (retained matches by seq; the
/// classic path renders the whole line once on `end`, ignoring the intermediate `begin`).
static TOOL_SEQ: AtomicU64 = AtomicU64::new(1);

fn next_tool_seq() -> u64 {
    TOOL_SEQ.fetch_add(1, Ordering::Relaxed)
}

fn tool_state(outcome: ToolOutcome) -> retained::ToolState {
    match outcome {
        None => retained::ToolState::Running,
        Some(true) => retained::ToolState::Ok,
        Some(false) => retained::ToolState::Err,
    }
}

/// Open a tool-call line (`⚙ name   target`) with no digest yet. Returns a `seq` to pass back to
/// [`tool_call_end`] so the result lands on the same line under retained. On the classic path this
/// renders nothing (the append-only surface can't update a line in place) — the full line is drawn
/// once by `tool_call_end`; the returned seq is still valid.
pub fn tool_call_begin(icon: &str, name: &str, target: &str) -> u64 {
    let seq = next_tool_seq();
    if retained::is_running() {
        retained::tool_event(retained::ToolEvent {
            seq,
            icon: icon.to_string(),
            name: name.to_string(),
            target: target.to_string(),
            digest: String::new(),
            state: retained::ToolState::Running,
            elapsed_ms: None,
        });
    }
    seq
}

/// Close a tool-call line with its result digest + run time. Under retained this updates the block
/// opened by [`tool_call_begin`] in place; on the classic path it renders the whole call line plus
/// the indented `└ <digest> · <time>` result line once, so both surfaces read the same. `elapsed_ms`
/// is the wall-clock run time (`None` → no time shown, e.g. restored transcripts).
pub fn tool_call_end(
    seq: u64,
    icon: &str,
    name: &str,
    target: &str,
    digest: &str,
    outcome: ToolOutcome,
    elapsed_ms: Option<u64>,
) {
    let ev = retained::ToolEvent {
        seq,
        icon: icon.to_string(),
        name: name.to_string(),
        target: target.to_string(),
        digest: digest.to_string(),
        state: tool_state(outcome),
        elapsed_ms,
    };
    if retained::is_running() {
        retained::tool_event(ev);
    } else {
        // Classic / plain / one-shot: render the identical stacked layout, emit once (may be 2 lines).
        for line in retained::render_tool_row(&ev, width()).split('\n') {
            emit_trace_line(line);
        }
    }
}

/// Replace the in-place plan checklist. `items` = `(status, text)` where status 0/1/2 = pending /
/// in-progress / done. Empty removes the panel. Classic path re-prints the box each call.
pub fn plan_update(items: &[(u8, String)]) {
    let rows: Vec<retained::PlanRow> = items
        .iter()
        .map(|(s, t)| retained::PlanRow {
            status: *s,
            text: t.clone(),
        })
        .collect();
    if retained::is_running() {
        retained::plan_update(rows);
    } else if !rows.is_empty() {
        for line in retained::render_plan_box(&rows, width()) {
            emit_trace_line(&line);
        }
    }
}

/// Push a boxed diff preview. `lines` = `(is_add, content)` already clipped of the leading `+`/`-`.
pub fn diff_box(path: &str, adds: usize, dels: usize, lines: Vec<(bool, String)>) {
    let d = retained::DiffPayload {
        path: path.to_string(),
        adds,
        dels,
        lines,
    };
    if retained::is_running() {
        retained::diff_box(d);
    } else {
        for line in retained::render_diff_box(&d, width()) {
            emit_trace_line(&line);
        }
    }
}

/// Push a green verify-gate success line (`✓ <cmd> — <detail>`).
pub fn verify_line(cmd: &str, detail: &str) {
    let v = retained::VerifyPayload {
        cmd: cmd.to_string(),
        detail: detail.to_string(),
    };
    if retained::is_running() {
        retained::verify_line(v);
    } else {
        emit_trace_line(&retained::render_verify_line(&v, width()));
    }
}

/// Set the working flag (drives the box indicator + the input thread's Esc semantics) and repaint.
/// Always updates the flag even when the TUI is inactive, so the input thread sees it.
pub fn set_working(working: bool) {
    WORKING.store(working, Ordering::Relaxed);
    if !working {
        // Defensive cleanup for error/early-return paths. Normal turns use identity-aware
        // `disarm_cancel`; clearing here is safe because no turn is active once WORKING is false.
        *active_turn_cancel()
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }
    // The elapsed clock and spinner frame live in the render thread's `AppState`: `set_working` below
    // stamps `working_since` and zeroes `frame`, so there is nothing to reset here. What IS local is
    // the queue depth shown in the prompt placeholder, and the per-turn token counter.
    if working {
        STREAM_CHARS.store(0, Ordering::Relaxed); // fresh token counter for this turn
        let d = SUBMISSION_DEPTH.load(Ordering::Relaxed);
        render()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queued_count = d;
        start_ticker();
    } else {
        render()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .queued_count = 0;
    }
    if retained::is_running() {
        retained::set_working(working);
    }
}

/// The status line currently on screen. Needed by any surface that [`suspend`]s for a dialoguer
/// prompt and must hand the SAME status back to [`resume`] — passing an empty string there blanks
/// the footer for the rest of the session.
pub fn current_status() -> String {
    render()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .status
        .clone()
}

/// Update the status text (model · tokens · yolo) and repaint. (Does not touch the output slot —
/// see [`set_working`].)
pub fn set_status(status: &str) {
    // Keep the classic shared status in sync even under retained — every keystroke snapshot reads it.
    {
        let mut r = render().lock().unwrap();
        r.status = status.to_string();
    }
    if retained::is_running() {
        retained::set_status(status);
    }
}

/// Recolour the retained input box for ultimate mode (gold ON, moonlight OFF). No-op on the classic
/// path (it has no persistent box to recolour). Called once when `/ultimate` toggles and once at
/// activation so the box opens in the right colour.
pub fn set_ultimate(on: bool) {
    if retained::is_running() {
        retained::set_ultimate(on);
    }
}

/// Point the working caption (the typewriter line beside the bottom-of-transcript spinner) at a
/// concrete action, e.g. "Reading retained.rs". An empty string falls back to the whimsical verb.
/// No-op off the retained path. The reveal replays only when the text actually changes.
pub fn set_work_caption(text: &str) {
    if retained::is_running() {
        retained::set_work_caption(text);
    }
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

    InputHandles {
        submissions,
        cancel,
        resume: resume_tx,
        inject,
        _handle: handle,
    }
}

/// Replace the live input draft without submitting it. Used by crash recovery: the interrupted user
/// request is restored for review/editing, never auto-sent to the model. Safe before/after retained
/// activation because the classic shared Render state remains the input source of truth.
pub fn set_draft(text: &str) {
    {
        let mut r = render().lock().unwrap();
        r.draft = text.chars().collect();
        r.cursor = r.draft.len();
        r.draft_sel = None; // a highlight from the old draft would point into unrelated text
        r.palette_sel = 0;
    }
    repaint_force();
}

/// Repaint the box from the current shared state (used by the input thread after an edit).
fn repaint() {
    if !active() {
        return;
    }
    repaint_force();
}

/// Translate the classic shared input/menu state into one retained-frame snapshot. The input thread
/// still owns editing semantics; only drawing moved to the render thread.
fn retained_input_snapshot() -> retained::InputSnapshot {
    let r = render().lock().unwrap();
    let overlay = if r.model_menu_active {
        Some(retained::OverlaySnapshot {
            title: "model".to_string(),
            lines: r
                .model_menu_rows
                .iter()
                .map(|row| {
                    if row.label.is_empty() {
                        row.id.clone()
                    } else {
                        row.label.clone()
                    }
                })
                .collect(),
            selected: Some(r.model_menu_sel),
            hint: "↑↓ pick · Enter set · Esc cancel".to_string(),
        })
    } else if r.sessions_menu_active {
        Some(retained::OverlaySnapshot {
            title: "sessions".to_string(),
            lines: r
                .sessions_menu_rows
                .iter()
                .map(|row| {
                    if row.subtitle.is_empty() {
                        row.title.clone()
                    } else {
                        format!("{}  ·  {}", row.title, row.subtitle)
                    }
                })
                .collect(),
            selected: Some(r.sessions_menu_sel),
            hint: "↑↓ pick · Enter restore · d delete · Esc cancel".to_string(),
        })
    } else if r.text_overlay_active {
        Some(retained::OverlaySnapshot {
            title: r.text_overlay_title.clone(),
            lines: r.text_overlay_lines.clone(),
            selected: None,
            hint: "↑↓/PgUp/PgDn scroll · Esc/q close".to_string(),
        })
    } else {
        // `@` file picker — takes priority over slash palette (you can't type both at once).
        let at = at_matches(&r.draft);
        if !at.is_empty() {
            Some(retained::OverlaySnapshot {
                title: "files".to_string(),
                lines: at.iter().map(|p| format!("@{p}")).collect(),
                selected: Some(r.at_sel.min(at.len().saturating_sub(1))),
                hint: "↑↓ pick · Tab complete · Enter attach · Esc close".to_string(),
            })
        } else {
            let matches = slash_matches(&r.draft);
            (!matches.is_empty()).then(|| retained::OverlaySnapshot {
                title: "commands".to_string(),
                lines: matches
                    .iter()
                    .map(|c| format!("/{}  ·  {}", c.name, c.description))
                    .collect(),
                selected: Some(r.palette_sel.min(matches.len().saturating_sub(1))),
                hint: "↑↓ pick · Tab complete · Enter run".to_string(),
            })
        }
    };
    retained::InputSnapshot {
        draft: r.draft.clone(),
        cursor: r.cursor,
        sel: r.draft_sel,
        images: r.images,
        status: r.status.clone(),
        queued_count: r.queued_count,
        overlay,
    }
}

/// Push the current input/menu state to the render thread, even when only a menu needs a refresh.
///
/// The retained backend owns every pixel, so this is a pure state send: the render thread diffs and
/// paints on its own schedule. A no-op when no session is up (one-shot `agent`/`chat`, pipes, CI) —
/// those surfaces have no footer to refresh.
fn repaint_force() {
    if retained::is_running() {
        retained::update_input(retained_input_snapshot());
    }
}

/// Manual recovery hatch (Ctrl-L): clear the terminal and repaint the whole frame from scratch.
///
/// Every KNOWN raw-print path now routes through [`note_line`], but the failure mode is structural —
/// anything that writes to the terminal behind the render thread's back (a dependency's own
/// `eprintln!`, a child process inheriting our stdout, a stray panic message) leaves ratatui's cell
/// buffer disagreeing with the screen, and its diff then only repaints cells it *thinks* changed, so
/// the foreign text stays wedged in later frames. `repaint_force` cannot fix that — it just resends
/// input state and the same stale diff applies. This drops the cached buffer entirely.
///
/// No-op when no retained session is up (one-shot `agent`/`chat`, pipes, CI).
pub fn force_redraw() {
    if retained::is_running() {
        retained::redraw();
    }
}

/// Recall the previous history entry into the draft (↑ / Ctrl-P). Shared by the arrow keys and the
/// readline-style Ctrl bindings so both stay in lock-step. `hist_idx` walks backward through
/// `history`; the first recall stashes the in-progress draft in `draft_saved` so ↓ can restore it.
fn recall_history_prev(
    hist_idx: &mut Option<usize>,
    draft_saved: &mut Vec<char>,
    history: &[String],
) {
    if history.is_empty() {
        return;
    }
    let mut r = render().lock().unwrap();
    let idx = match *hist_idx {
        None => {
            *draft_saved = r.draft.clone();
            history.len() - 1
        }
        Some(0) => 0,
        Some(i) => i - 1,
    };
    *hist_idx = Some(idx);
    r.draft = history[idx].chars().collect();
    r.cursor = r.draft.len();
    drop(r);
    repaint();
}

/// Recall the next history entry (↓ / Ctrl-N). Walks forward through `history`; stepping past the
/// newest entry restores the draft that was in progress when history recall began.
fn recall_history_next(hist_idx: &mut Option<usize>, draft_saved: &[char], history: &[String]) {
    let mut r = render().lock().unwrap();
    match *hist_idx {
        Some(i) if i + 1 < history.len() => {
            *hist_idx = Some(i + 1);
            r.draft = history[i + 1].chars().collect();
            r.cursor = r.draft.len();
        }
        Some(_) => {
            *hist_idx = None;
            r.draft = draft_saved.to_vec();
            r.cursor = r.draft.len();
        }
        None => {}
    }
    drop(r);
    repaint();
}

/// Translate a crossterm `KeyEvent` into the `console::Key` the rest of the input stack already
/// speaks, so migrating the reader from `console::read_key` to crossterm's event stream doesn't
/// force a re-type of every menu/overlay handler. Returns `None` for keys we don't act on.
///
/// Control combos are folded back to their ASCII control codepoint (Ctrl-C → `'\u{3}'`, Ctrl-O →
/// `'\u{f}'`, …) — the exact bytes the old `console` reader produced and every downstream `match`
/// arm expects. Shift+Enter is handled by the caller BEFORE this (crossterm can see the SHIFT bit
/// that `console` could not), so it never reaches here.
fn crossterm_to_console_key(ev: crossterm::event::KeyEvent) -> Option<Key> {
    use crossterm::event::{KeyCode, KeyModifiers};
    let ctrl = ev.modifiers.contains(KeyModifiers::CONTROL);
    Some(match ev.code {
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Escape,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Del,
        KeyCode::Left => Key::ArrowLeft,
        KeyCode::Right => Key::ArrowRight,
        KeyCode::Up => Key::ArrowUp,
        KeyCode::Down => Key::ArrowDown,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Insert => Key::Insert,
        KeyCode::Char(c) => {
            if ctrl && c.is_ascii_alphabetic() {
                // Ctrl-A..Ctrl-Z → U+0001..U+001A (Ctrl-C → '\u{3}', matching the old console bytes).
                Key::Char(((c.to_ascii_uppercase() as u8) - b'A' + 1) as char)
            } else if ctrl {
                return None; // Ctrl+non-letter: nothing downstream binds it
            } else {
                Key::Char(c)
            }
        }
        _ => return None,
    })
}

/// Wrapped lines moved per wheel notch. Three is the usual terminal default (xterm's `scrollLines`),
/// small enough that a drag-then-wheel misfire can't fling the viewport far, brisk enough to page a
/// transcript without spinning.
const WHEEL_LINES: usize = 3;

/// Is screen cell (`col`, `row`) inside `rect`? Saturating throughout so a rect flush against the
/// right/bottom edge can't wrap into a false miss.
fn hit(rect: ratatui::layout::Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

/// Phase 3 mouse handler for the retained backend: wheel scroll, text selection (drag +
/// copy-on-release) and scrollbar thumb drag. Mutates `selecting` / `dragging_scrollbar` so state
/// survives across successive mouse events. No-ops harmlessly when geometry is empty (first frame).
///
/// The wheel scrolls the transcript (or the open overlay) EXCEPT while a scrollbar-thumb drag is in
/// flight — see the `match` below for why that one carve-out matters and why a live text selection is
/// deliberately NOT one. PageUp/PageDown and End still work as the keyboard path.
///
/// The RIGHT button is not handled either. It used to pop a one-item "Copy" box over the transcript,
/// which was a floating surface to draw, clamp, hit-test and dismiss for an action Ctrl-C now does
/// from the keyboard — and it stole the button the terminal itself uses for paste on Windows.
fn handle_retained_mouse(
    kind: crossterm::event::MouseEventKind,
    col: u16,
    row: u16,
    selecting: &mut Option<retained::SelectionRange>,
    dragging_scrollbar: &mut bool,
    dragging_draft: &mut Option<usize>,
) {
    use crossterm::event::{MouseButton, MouseEventKind};
    // The input box gets first refusal on the buttons, and it is checked BEFORE the transcript
    // geometry gate below: the footer is painted even on a frame where the transcript is still empty,
    // and clicking into the box you are typing in must work from the very first keystroke.
    if handle_input_box_mouse(
        kind,
        col,
        row,
        selecting,
        dragging_scrollbar,
        dragging_draft,
    ) {
        return;
    }
    let (start, visible, total, area) = retained::last_transcript_geom();
    if area.width == 0 || area.height == 0 {
        // Nothing painted yet: there is no line/column mapping to hit-test against, and the wheel is
        // deliberately not a scroll input (see the match below), so every event is a no-op here.
        return;
    }
    // Scrollbar gutter = rightmost cell of the transcript area.
    let on_scrollbar = col >= area.x.saturating_add(area.width.saturating_sub(1))
        && row >= area.y
        && row < area.y.saturating_add(area.height);
    let in_transcript = col >= area.x
        && col < area.x.saturating_add(area.width.saturating_sub(1))
        && row >= area.y
        && row < area.y.saturating_add(area.height);

    match kind {
        // The wheel scrolls the transcript (or the open overlay — `Command::Scroll` routes there
        // itself). It NEVER reaches the input line: it goes through `retained::scroll`, which moves
        // only the viewport/overlay offset, never the draft. The only carve-out is an active
        // scrollbar-thumb drag, where a wheel tick would fight the thumb the mouse is holding —
        // and `dragging_scrollbar` is cleared reliably on mouse-up.
        //
        // We deliberately do NOT also gate on `selecting`. A missed mouse-up can leave `selecting`
        // stuck at `Some` for the rest of the session (documented in the Esc arm above); gating the
        // wheel on it would then silently kill scrolling for good. Selection endpoints are absolute
        // line numbers, so a pure wheel scroll doesn't corrupt a live highlight anyway — reliable
        // history scrolling outranks protecting the rare drag-then-wheel-then-move case.
        //
        // Mouse capture stays on regardless — it is what stops the terminal's "alternateScroll" from
        // leaking wheel ticks through as ↑/↓ and walking input history behind the user's back.
        MouseEventKind::ScrollUp if !*dragging_scrollbar => {
            retained::scroll(-(WHEEL_LINES as i32)); // negative delta = up, toward history
        }
        MouseEventKind::ScrollDown if !*dragging_scrollbar => {
            retained::scroll(WHEEL_LINES as i32); // positive delta = down, toward the live tail
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {} // suppressed only mid scrollbar drag
        MouseEventKind::Down(MouseButton::Left) => {
            // Floating "jump to bottom" button takes priority: a click anywhere on it lands the
            // viewport back on the live tail (only present while scrolled up off the tail).
            if let Some(b) = retained::jump_button_rect() {
                if hit(b, col, row) {
                    *dragging_scrollbar = false;
                    *selecting = None;
                    retained::clear_selection();
                    retained::scroll_end();
                    return;
                }
            }
            if on_scrollbar && total > visible {
                *dragging_scrollbar = true;
                *selecting = None;
                retained::clear_selection();
                let rel_y = row.saturating_sub(area.y) as usize;
                let max_start = total.saturating_sub(visible);
                let desired = if area.height <= 1 {
                    0
                } else {
                    (rel_y.saturating_mul(max_start)) / (area.height as usize - 1).max(1)
                };
                retained::scroll_to(desired.min(max_start));
            } else if in_transcript {
                *dragging_scrollbar = false;
                let line = start.saturating_add(row.saturating_sub(area.y) as usize);
                let c = col.saturating_sub(area.x) as usize;
                let sel = retained::SelectionRange {
                    anchor_line: line,
                    anchor_col: c,
                    cursor_line: line,
                    cursor_col: c,
                };
                *selecting = Some(sel);
                retained::set_selection(sel);
            } else {
                // Click outside transcript/scrollbar clears any live selection.
                *dragging_scrollbar = false;
                *selecting = None;
                retained::clear_selection();
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if *dragging_scrollbar && total > visible {
                let rel_y = row.saturating_sub(area.y) as usize;
                let max_start = total.saturating_sub(visible);
                let desired = if area.height <= 1 {
                    0
                } else {
                    (rel_y.saturating_mul(max_start)) / (area.height as usize - 1).max(1)
                };
                retained::scroll_to(desired.min(max_start));
            } else if let Some(sel) = selecting.as_mut() {
                // Velocity-based auto-scroll: deeper past the edge → faster (1/2/4 lines). Geometry
                // is only updated after the render thread paints, so we apply an optimistic start
                // delta locally — otherwise the selection lag-stutters one frame behind the scroll.
                let top = area.y as i32;
                let bot = area.y.saturating_add(area.height.saturating_sub(1)) as i32;
                let r = row as i32;
                let (scroll_delta, start_delta): (i32, isize) = if r <= top {
                    let dist = (top - r + 1) as i32;
                    let n = if dist >= 4 {
                        4
                    } else if dist >= 2 {
                        2
                    } else {
                        1
                    };
                    (-n, -(n as isize))
                } else if r >= bot {
                    let dist = (r - bot + 1) as i32;
                    let n = if dist >= 4 {
                        4
                    } else if dist >= 2 {
                        2
                    } else {
                        1
                    };
                    (n, n as isize)
                } else if r <= top + 1 {
                    (-1, -1)
                } else if r >= bot - 1 {
                    (1, 1)
                } else {
                    (0, 0)
                };
                if scroll_delta != 0 {
                    retained::scroll(scroll_delta);
                }
                // Clamp the cursor into the transcript area for col/line mapping, but keep absolute
                // line growing via optimistic start so the selection extends while auto-scrolling.
                let start2 = if start_delta < 0 {
                    start.saturating_sub((-start_delta) as usize)
                } else {
                    start.saturating_add(start_delta as usize)
                };
                let clamp_row =
                    row.clamp(area.y, area.y.saturating_add(area.height.saturating_sub(1)));
                let clamp_col =
                    col.clamp(area.x, area.x.saturating_add(area.width.saturating_sub(2)));
                let line = start2.saturating_add(clamp_row.saturating_sub(area.y) as usize);
                let c = clamp_col.saturating_sub(area.x) as usize;
                // Skip no-op updates — floods of identical Drag events were jamming the render queue.
                if sel.cursor_line != line || sel.cursor_col != c {
                    sel.cursor_line = line;
                    sel.cursor_col = c;
                    retained::set_selection(*sel);
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if *dragging_scrollbar {
                *dragging_scrollbar = false;
            } else if let Some(sel) = selecting.take() {
                // Keep highlight until next click; copy text to the OS clipboard on release and
                // confirm with a one-line note so the user knows the copy landed.
                retained::set_selection(sel);
                let text = retained::extract_selection_text(sel);
                if !text.is_empty() {
                    let ok = copy_to_os_clipboard(&text);
                    note_copied(&text, ok);
                }
            }
        }
        // Right-click is NOT a copy path anymore. It used to pop a floating "Copy" button over the
        // highlight, which meant the one gesture people already know for copying (Ctrl-C) still quit
        // the app while a mouse-only affordance did the copying. Ctrl-C is the copy key now — see the
        // `Key::CtrlC` arm in the keyboard loop — so the menu, its layout clamp, and its hit-test rect
        // are all gone rather than left as a second way to do the same thing.
        _ => {}
    }
}

/// Mouse inside the input box: click to park the caret, drag to select, release to copy. Returns
/// whether the event belonged to the box (the caller then leaves the transcript alone).
///
/// This is the mouse half of editing the draft. Without it the caret could only be walked with ←/→,
/// which on a draft longer than the box also dragged the whole line under a stationary cursor, and the
/// only way to copy what you had typed was Ctrl-C taking ALL of it.
///
/// The box's highlight and the transcript's are deliberately exclusive — Ctrl-C has one meaning at a
/// time, so starting one drops the other rather than leaving two highlights on screen competing to be
/// "the selection".
fn handle_input_box_mouse(
    kind: crossterm::event::MouseEventKind,
    col: u16,
    row: u16,
    selecting: &mut Option<retained::SelectionRange>,
    dragging_scrollbar: &mut bool,
    dragging_draft: &mut Option<usize>,
) -> bool {
    use crossterm::event::{MouseButton, MouseEventKind};
    match kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(idx) = retained::input_hit(col, row) else {
                // Clicking anywhere else ends the box's claim on the highlight — including a click that
                // starts a transcript selection, which the caller goes on to handle.
                *dragging_draft = None;
                clear_draft_selection();
                return false;
            };
            *selecting = None;
            retained::clear_selection();
            *dragging_scrollbar = false;
            *dragging_draft = Some(idx);
            set_draft_caret(idx, None);
            true
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let Some(anchor) = *dragging_draft else {
                return false;
            };
            // Column only, row ignored: the pointer leaves a one-row box constantly while selecting
            // inside it, and a drag that stopped extending the moment it strayed a row would be
            // unusable. Off either end of the text it clamps to the nearest edge.
            if let Some(idx) = retained::input_hit_col(col) {
                set_draft_caret(idx, Some((anchor, idx)));
            }
            true
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if dragging_draft.take().is_none() {
                return false;
            }
            // Copy on release, as the transcript does: letting go of the button is the moment the user
            // has finished saying what they meant. The highlight stays until the next click so it is
            // still there to retype over.
            if let Some((a, b)) = draft_selection() {
                let text: String = {
                    let r = render().lock().unwrap();
                    r.draft[a..b].iter().collect()
                };
                if !text.is_empty() {
                    let ok = copy_to_os_clipboard(&text);
                    note_copied(&text, ok);
                }
            }
            true
        }
        _ => false,
    }
}

/// Copy selected transcript text to the OS clipboard, reporting whether it actually landed.
///
/// DESKTOP-ONLY: `arboard` is target-gated to Windows/macOS (Linux would need X11/Wayland libs at
/// runtime, breaking the headless static binary — see Cargo.toml), so on Linux this is a no-op.
/// It returns `bool` rather than `()` so a deliberate Ctrl-C copy can confirm honestly instead of
/// printing "copied" on a platform where nothing was — a key the user pressed on purpose must not
/// lie about what it did, least of all when the alternative reading of that key is "quit".
#[cfg(any(windows, target_os = "macos"))]
fn copy_to_os_clipboard(text: &str) -> bool {
    match arboard::Clipboard::new() {
        Ok(mut cb) => cb.set_text(text.to_string()).is_ok(),
        Err(_) => false,
    }
}
#[cfg(not(any(windows, target_os = "macos")))]
fn copy_to_os_clipboard(_text: &str) -> bool {
    false
}

/// Read plain text from the OS clipboard (for Ctrl-V / right-click / Shift+Insert paste).
///
/// Normalizes CRLF → LF so a Windows copy lands as the same newlines Shift+Enter would type.
/// `None` when the clipboard is empty, holds non-text, or this platform has no clipboard crate.
#[cfg(any(windows, target_os = "macos"))]
fn clipboard_text() -> Option<String> {
    let mut cb = arboard::Clipboard::new().ok()?;
    let text = cb.get_text().ok()?;
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}
#[cfg(not(any(windows, target_os = "macos")))]
fn clipboard_text() -> Option<String> {
    None
}

/// Insert `text` at the draft caret, replacing any live highlight (editor paste semantics).
fn insert_draft_text(text: &str) {
    if text.is_empty() {
        return;
    }
    delete_draft_selection();
    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();
    let mut r = render().lock().unwrap();
    let cur = r.cursor.min(r.draft.len());
    r.draft.splice(cur..cur, chars);
    r.cursor = cur + n;
    r.palette_sel = 0;
    r.at_sel = 0;
}

/// Paste OS clipboard text into the draft. Returns whether anything was inserted.
fn paste_clipboard_into_draft() -> bool {
    let Some(text) = clipboard_text() else {
        return false;
    };
    insert_draft_text(&text);
    true
}

/// Confirm (or honestly deny) a copy with one dim transcript line.
///
/// Routed through `note_line`, never `eprintln!`: a raw write behind the render thread's back lands
/// inside a retained frame and corrupts it, because ratatui's cell diff compares against its own
/// last frame and never sees the foreign text.
fn note_copied(text: &str, ok: bool) {
    let msg = if ok {
        format!("· copied {}", copy_size(text))
    } else {
        "· clipboard unavailable on this platform — nothing copied".to_string()
    };
    note_line(&style(msg).dim().to_string());
}

/// `12 chars` / `48 chars (3 lines)` — the size half of every copy confirmation.
fn copy_size(text: &str) -> String {
    let chars = text.chars().count();
    let rows = text.lines().count().max(1);
    if rows > 1 {
        format!("{chars} chars ({rows} lines)")
    } else {
        format!("{chars} chars")
    }
}

/// What a Ctrl-C press should copy, if anything, and the word to call it in the confirmation.
///
/// Order matters: a transcript highlight is an explicit, visible act of selection, so it outranks the
/// draft. The draft is the fallback because "copy what I just typed" is the case with no other route
/// at all — the input row is a single ratatui line, so the terminal's own mouse selection cannot reach
/// the parts of a long draft that are scrolled out of the window.
///
/// `None` means there is nothing to copy, and the press keeps its original meaning: quit.
fn ctrl_c_copy_target() -> Option<(String, &'static str)> {
    if let Some(sel) = retained::live_selection() {
        let text = retained::extract_selection_text(sel);
        if !text.trim().is_empty() {
            return Some((text, "selection"));
        }
    }
    let r = render().lock().unwrap();
    // A highlight dragged inside the box is as explicit an act of selection as a transcript one, so it
    // outranks the whole-draft fallback — selecting three words and pressing Ctrl-C must not put the
    // entire draft on the clipboard.
    if let Some((a, b)) = normalized_draft_sel(r.draft_sel, r.draft.len()) {
        let text: String = r.draft[a..b].iter().collect();
        if !text.trim().is_empty() {
            return Some((text, "selection"));
        }
    }
    let draft: String = r.draft.iter().collect();
    if !draft.trim().is_empty() {
        return Some((draft, "draft"));
    }
    None
}

/// How long a Ctrl-C that copied stays "armed", so the next Ctrl-C quits instead of copying again.
const CTRL_C_QUIT_WINDOW: Duration = Duration::from_millis(2000);

/// What one Ctrl-C press means, given how long ago the previous press copied and whether there is
/// anything to copy right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CtrlC {
    Copy,
    Quit,
}

/// Resolve the two meanings of Ctrl-C. Pure so the arbitration is testable without a terminal, a
/// clipboard, or a running input loop — the arm state is the only thing standing between "copy" and
/// "the app exits", so it must not be verifiable by hand-play alone.
///
/// `since_copy` is `None` when no Ctrl-C has copied yet this session.
fn ctrl_c_action(since_copy: Option<Duration>, has_target: bool) -> CtrlC {
    let armed = since_copy.map(|d| d < CTRL_C_QUIT_WINDOW).unwrap_or(false);
    if !armed && has_target {
        CtrlC::Copy
    } else {
        CtrlC::Quit
    }
}

/// Confirm a Ctrl-C copy AND say how to still quit — the two meanings of the key now share it, so the
/// note has to resolve the ambiguity in the same breath it reports the copy.
fn note_ctrl_c_copy(text: &str, ok: bool, what: &str) {
    let msg = if ok {
        format!(
            "· copied {what} — {} · Ctrl-C again to quit",
            copy_size(text)
        )
    } else {
        "· clipboard unavailable on this platform · Ctrl-C again to quit".to_string()
    };
    note_line(&style(msg).dim().to_string());
}

fn input_loop(
    sub_tx: UnboundedSender<Submission>,
    cancel_tx: UnboundedSender<()>,
    resume_rx: stdmpsc::Receiver<()>,
) {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    let mut history: Vec<String> = Vec::new();
    let mut hist_idx: Option<usize> = None;
    let mut draft_saved: Vec<char> = Vec::new();
    // Arrival time of the PREVIOUS key, so we can measure the inter-key gap (below). `None` until the
    // first key of the session.
    let mut last_arrival: Option<Instant> = None;
    // Arrival time TWO keys ago, for detecting paste burst end (when prev was buffered but current is not).
    let mut last_arrival_prev: Option<Instant> = None;
    // Draft was edited during a paste burst without a repaint. Flushed on the first idle poll after
    // the burst ends (see short timeout below) so the full paste appears without needing another key.
    let mut pending_paste_repaint = false;
    // Phase 3 mouse drag state (retained only). `selecting` tracks left-drag text selection;
    // `dragging_scrollbar` tracks thumb drag on the right gutter. Cleared on mouse-up / Esc.
    let mut selecting: Option<retained::SelectionRange> = None;
    let mut dragging_scrollbar = false;
    // Anchor of a left-drag that started INSIDE the input box (draft char index), so the drag extends
    // the same selection it began. `None` when no such drag is in flight; cleared on release.
    let mut dragging_draft: Option<usize> = None;
    // Idle screensaver state (retained only). After IDLE_SCREENSAVER secs with no key/mouse activity
    // — and only when idle, not working, and no menu/overlay is open — the render thread blits one
    // static feature card over the alt-screen. The next input event clears it (and is swallowed, so
    // the wake key never edits the draft). `last_activity` is the wall-clock of the last event.
    let mut last_activity = Instant::now();
    let mut screensaver_up = false;
    // When the last Ctrl-C copied something instead of quitting. Ctrl-C now means "copy" whenever
    // there IS something to copy, so quitting needs a second press — and this timestamp is what makes
    // the second press mean quit rather than copying the same text again. It expires
    // (`CTRL_C_QUIT_WINDOW`) so a Ctrl-C minutes later is a fresh copy, not a surprise exit.
    let mut ctrl_c_armed: Option<Instant> = None;

    // Startup card: show ONE rotating feature card over the landing screen the moment the sticky TUI
    // is up (retained only — the blit is a raw sixel the alt-screen renderer can't carry any other
    // way). It rides the SAME blit path as the idle screensaver, so the first keystroke tears it down
    // and is swallowed (revealing the landing splash underneath). `next_startup_card` advances a
    // persisted counter so each launch shows the next card. Skipped when sixel isn't supported (the
    // card would be escape-code garbage) — the text splash already stands on its own there.
    if retained::is_active() && crate::ui::splash::logo_is_sixel() {
        if let Some(idx) = crate::ui::cards::next_startup_card() {
            retained::screensaver(Some(idx));
            screensaver_up = true;
            last_activity = Instant::now();
        }
    }

    loop {
        // STAND DOWN while a `dialoguer` menu owns stdin. This is the whole fix for the input freeze:
        // the flag is set by `suspend()` itself, so it can't disagree with who actually holds the
        // terminal, and we spin on a short sleep instead of blocking on a resume signal — a menu that
        // exits by an unexpected path can never leave the keyboard wedged forever. Raw mode is dropped
        // once on the parking edge so the menu's cooked mode survives (the re-assert below is what used
        // to clobber it every iteration).
        if KEYBOARD_PARKED.load(Ordering::SeqCst) {
            let _ = crossterm::terminal::disable_raw_mode();
            // Tell `suspend()` the keyboard is genuinely out of the way. It blocks on this (with a
            // deadline) before handing stdin to the menu, so the menu can't open while we're still
            // finishing a `poll`.
            KEYBOARD_RELEASED.store(true, Ordering::SeqCst);
            while KEYBOARD_PARKED.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(25));
            }
            KEYBOARD_RELEASED.store(false, Ordering::SeqCst);
            // Drain resume pings buffered by the old park protocol so they can't unpark a later menu.
            while resume_rx.try_recv().is_ok() {}
            last_activity = Instant::now();
        }
        // Raw mode is required for crossterm's event reader (no line buffering / echo). Re-assert it
        // every iteration: it's idempotent, and a slash command that parked us for a `dialoguer` menu
        // flips stdin back to cooked mode (see `prepare_dialoguer_session`) — re-enabling here restores
        // raw the moment we're unparked, without threading any state through the park/resume dance.
        let _ = crossterm::terminal::enable_raw_mode();
        // Read the next actionable key from crossterm's event stream. Non-key events are handled and
        // skipped inline: on Windows the console delivers BOTH press AND release records, so we keep
        // only `Press` (otherwise every key fires twice). With mouse capture on (retained): wheel
        // scrolls the transcript; left-drag selects text (copy-on-release via arboard); the right
        // gutter scrollbar is draggable. Shift+Enter inserts a literal newline into the draft.
let key = loop {
            // Poll (not a bare blocking read) so the idle clock is checked on a ~1s cadence: after
            // IDLE_SCREENSAVER_SECS of no input — and only when quiescent (retained, not working, no
            // menu/overlay/approval up) — the render thread blits one static card. A busy turn or an
            // open menu never triggers it. When there IS input the read below returns immediately.
            //
            // While a paste burst skipped per-char repaints, poll on the coalesce window instead of
            // 1s: the first idle tick after the burst flushes one repaint so the full paste shows
            // without the user having to type another key (space, arrow, …).
            let poll_ms = if pending_paste_repaint {
                PASTE_COALESCE_MS
            } else {
                1000
            };
            let have_event = match event::poll(Duration::from_millis(poll_ms)) {
                Ok(v) => v,
                Err(_) => {
                    let _ = sub_tx.send(Submission::Quit);
                    return;
                }
            };
            if !have_event {
                if pending_paste_repaint {
                    pending_paste_repaint = false;
                    repaint();
                }
                if !screensaver_up
                    && retained::is_active()
                    // Same sixel gate as the startup card above. Without it this path fires every
                    // 15 idle seconds on a terminal that cannot decode sixel — the startup blit was
                    // gated but this one was not, so the freeze came back on a timer.
                    && crate::ui::splash::logo_is_sixel()
                    && !WORKING.load(Ordering::Relaxed)
                    && !APPROVAL_PENDING.load(Ordering::Relaxed)
                    && !model_menu_active()
                    && !sessions_menu_active()
                    && !text_overlay_active()
                    && !RETAINED_INFO_OVERLAY.load(Ordering::Relaxed)
                    && last_activity.elapsed() >= Duration::from_secs(IDLE_SCREENSAVER_SECS)
                {
                    if let Some(idx) = crate::ui::cards::screensaver_card() {
                        retained::screensaver(Some(idx));
                        screensaver_up = true;
                    }
                }
                continue;
            }
            let ev = match event::read() {
                Ok(ev) => ev,
                Err(_) => {
                    let _ = sub_tx.send(Submission::Quit);
                    return;
                }
            };
            // Any real event is activity: reset the idle clock, and if the screensaver is up, tear it
            // down and SWALLOW this event so the wake keystroke never also edits the draft (mirrors the
            // RETAINED_INFO_OVERLAY key-swallow below).
            last_activity = Instant::now();
if screensaver_up {
                retained::screensaver(None);
                screensaver_up = false;
                continue;
            }
            // Bracketed paste (when the terminal supports it): one event, whole string, one repaint.
            // Falls through to the key-burst / clipboard path below on hosts that still synthesize
            // pastes as key events or offer no paste channel at all (classic conhost/CMD).
            if let Event::Paste(text) = ev {
                if !text.is_empty() {
                    insert_draft_text(&text.replace("\r\n", "\n").replace('\r', "\n"));
                    hist_idx = None;
                    last_arrival = None;
                    last_arrival_prev = None;
                    pending_paste_repaint = false;
                    repaint();
                }
                continue;
            }
            match ev {
                Event::Key(ke) if ke.kind == KeyEventKind::Press => {
                    // Shift+Insert = paste (classic Windows / terminal convention). Handled here
                    // because `crossterm_to_console_key` drops the Shift bit on Insert.
                    if ke.code == KeyCode::Insert && ke.modifiers.contains(KeyModifiers::SHIFT) {
                        if paste_clipboard_into_draft() {
                            hist_idx = None;
                            last_arrival = None;
                            last_arrival_prev = None;
                            pending_paste_repaint = false;
                            repaint();
                        }
                        continue;
                    }
                    if ke.code == KeyCode::Enter && ke.modifiers.contains(KeyModifiers::SHIFT) {
                        // Reached before the central selection rule below, so apply it here: a newline
                        // typed over a highlight replaces it, like any other inserted char.
                        delete_draft_selection();
                        let mut r = render().lock().unwrap();
                        let cur = r.cursor;
                        r.draft.insert(cur, '\n');
                        r.cursor += 1;
                        r.palette_sel = 0;
                        drop(r);
                        hist_idx = None;
                        repaint();
                        continue;
                    }
                    // Alt+Enter (or Ctrl+Enter) = STEER: hand the draft to the RUNNING turn instead of
                    // the post-turn queue, so "wait, also do X" reaches the agent mid-flight (it folds
                    // the message in at its next step) instead of waiting for the turn to finish. Two
                    // chords because Windows Terminal binds Alt+Enter to fullscreen by default and
                    // swallows it before the app sees it; Ctrl+Enter is the fallback there (and the
                    // `>` draft prefix below covers terminals that eat both). Idle, or a mailbox that
                    // refuses (no live turn / backlog full / oversized), falls through to the normal
                    // Enter path below so the keystroke is never silently swallowed.
                    if ke.code == KeyCode::Enter
                        && (ke.modifiers.contains(KeyModifiers::ALT)
                            || ke.modifiers.contains(KeyModifiers::CONTROL))
                    {
                        let line: String = render().lock().unwrap().draft.iter().collect();
                        if crate::core::steer::push(&line) {
                            let mut r = render().lock().unwrap();
                            r.draft.clear();
                            r.cursor = 0;
                            r.draft_sel = None; // the text it highlighted just left for the agent
                            r.palette_sel = 0;
                            drop(r);
                            if !line.trim().is_empty() {
                                history.push(line.trim().to_string());
                            }
                            hist_idx = None;
                            repaint();
                            continue;
                        }
                    }
                    // Esc with a live mouse selection clears the selection — but ONLY when there is no
                    // turn to stop. Stopping the agent always outranks dropping a highlight.
                    //
                    // This branch used to consume Esc unconditionally, and `selecting` is only cleared
                    // on a left-button RELEASE. Press inside the transcript and release anywhere the
                    // terminal doesn't report (drag out of a small panel, focus lost mid-drag) and the
                    // state stays `Some` for the rest of the session — from then on EVERY Esc was eaten
                    // here and cancel never ran. Falling through while a turn is in flight (and clearing
                    // the stale selection on the way) means a missed mouse-up can no longer disarm Esc.
                    // Esc also disarms a stuck input-box drag. `dragging_draft` is only cleared on a
                    // left-button RELEASE, and a release the terminal never reports (drag out of the
                    // window, focus lost mid-drag) would otherwise leave the box swallowing every later
                    // drag event, killing transcript selection for the rest of the session.
                    if ke.code == KeyCode::Esc {
                        dragging_draft = None;
                    }
                    if ke.code == KeyCode::Esc && selecting.is_some() {
                        selecting = None;
                        retained::clear_selection();
                        if !turn_in_flight() {
                            continue; // idle: dropping the highlight is the whole action
                        }
                        // A turn IS running: the highlight is gone, but this Esc still has to reach
                        // the cancel arm below, so don't consume it.
                    }
match crossterm_to_console_key(ke) {
                        Some(k) => break k,
                        None => continue,
                    }
                }
                Event::Mouse(me) if retained::is_active() => {
                    use crossterm::event::{MouseButton, MouseEventKind};
                    // Right-click paste. conhost/CMD's own Quick-Edit paste never reaches us once
                    // mouse capture is on (we need capture for wheel + selection), so the app has
                    // to own the gesture. Down only — Up would double-insert the same clipboard.
                    if matches!(me.kind, MouseEventKind::Down(MouseButton::Right)) {
                        if paste_clipboard_into_draft() {
                            hist_idx = None;
                            last_arrival = None;
                            last_arrival_prev = None;
                            pending_paste_repaint = false;
                            repaint();
                        }
                        continue;
                    }
                    handle_retained_mouse(
                        me.kind,
                        me.column,
                        me.row,
                        &mut selecting,
                        &mut dragging_scrollbar,
                        &mut dragging_draft,
                    );
                    continue;
                }
                // Release/Repeat key records, other mouse, resize, focus → not actioned here.
                _ => continue,
            }
        };
        // Paste detection by INTER-KEY GAP. Windows Terminal delivers a paste as a burst of individual
        // key events (crossterm has no bracketed-paste on Windows), so we infer a paste from how close
        // successive key ARRIVALS are. Measuring the gap (not how long the read blocked) folds a slow
        // repaint while the agent is WORKING into the gap, so a real keystroke (arrivals ≥ ~100 ms
        // apart) is never mistaken for a paste (arrivals < 1 ms apart), regardless of how busy the turn
        // is. Consumed only by the `Key::Enter if buffered` arm → a newline inside a paste becomes a
        // literal `\n` instead of firing one message per line.
        //
        // IME FIX: When typing Vietnamese (Telex/VNI), Windows IME sends `Backspace` + new composed
        // char within <50ms (e.g., `a` → backspace → `á`). Without filtering, this looks like a paste
        // burst → the composed char's repaint is skipped → the char is hidden until next keystroke.
        // A real paste never contains Backspace/Del, so reset `last_arrival` after seeing them to
        // break the burst chain. The next char (IME-committed) arrives with no "prev" timestamp → not
        // buffered → repaint happens immediately.
        let now = Instant::now();
        let is_ime_edit = matches!(key, Key::Backspace | Key::Del);
        let buffered = if is_ime_edit {
            false // Backspace/Del during IME composition are NOT part of a paste burst
        } else {
            last_arrival
                .map(|t| now.duration_since(t) < Duration::from_millis(PASTE_COALESCE_MS))
                .unwrap_or(false)
        };
// Repaint throttle: during a paste burst, skip per-char repaint. Without this, pasting 500
        // chars queues 500 retained::update_input calls → visible char-by-char lag. The final
        // repaint is flushed on the first idle poll after the burst (pending_paste_repaint), not on
        // the next keystroke — so the full paste appears without needing a space/arrow press.
        let prev_buffered = last_arrival
            .and_then(|t| {
                last_arrival_prev
                    .map(|p| t.duration_since(p) < Duration::from_millis(PASTE_COALESCE_MS))
            })
            .unwrap_or(false);
        last_arrival_prev = last_arrival;
        // Reset the timestamp chain after Backspace/Del so the next char (IME-committed) is not
        // mistaken for part of a burst.
        last_arrival = if is_ime_edit { None } else { Some(now) };
        // in_paste_burst: we are mid-burst → skip repaint this keystroke and mark a deferred flush.
        let in_paste_burst = buffered && prev_buffered;
        if in_paste_burst {
            pending_paste_repaint = true;
        } else if pending_paste_repaint {
            // First keystroke after the burst (or a non-burst edit): flush now so the deferred
            // draft is on screen before this keystroke's own edit is applied + repainted.
            pending_paste_repaint = false;
            repaint();
        }
        // If the agent is awaiting a per-action approval, THIS keystroke is the answer — route a
        // y/n/a decision to the blocked gate and never treat it as draft input. Other keys are
        // ignored so a stray press can't accidentally approve.
        if APPROVAL_PENDING.load(Ordering::Relaxed) {
            // Esc at an approval prompt means "stop", not merely "deny this one". Denying alone hands
            // the model an `error: denied` string and it keeps going — the user presses Esc, watches the
            // turn continue, and concludes cancel is broken. So answer the blocked gate with `n` (it is
            // waiting on that channel and would otherwise hang forever) AND request cancellation, so the
            // loop unwinds instead of proceeding to the next tool call.
            if matches!(key, Key::Escape) {
                if let Some(tx) = approval_slot()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                {
                    let _ = tx.send('n');
                }
                request_cancel();
                let _ = cancel_tx.send(());
                crate::core::steer::clear();
                continue;
            }
            let decided = match key {
                Key::Char('y') | Key::Char('Y') => Some('y'),
                Key::Char('a') | Key::Char('A') => Some('a'),
                Key::Char('n') | Key::Char('N') => Some('n'),
                _ => None,
            };
            if let Some(c) = decided {
                if let Some(tx) = approval_slot()
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take()
                {
                    let _ = tx.send(c);
                }
                continue;
            }
            // y/n/a only — other keys still edit the draft / queue messages (Claude-style).
        }
        if model_menu_handle_key(&key) {
            continue;
        }
        if sessions_menu_handle_key(&key) {
            continue;
        }
        if text_overlay_handle_key(&key) {
            continue;
        }
        if retained::is_active() && RETAINED_INFO_OVERLAY.load(Ordering::Relaxed) {
            match key {
                Key::Escape | Key::Char('q') | Key::Char('Q') => retained_overlay_close(),
                Key::PageUp => retained::scroll(-8),
                Key::PageDown => retained::scroll(8),
                Key::End => retained::scroll_end(),
                _ => {}
            }
            continue;
        }
// What a keystroke does to a live input-box highlight, decided in ONE place rather than in each
        // arm below: typing replaces it, Backspace/Del removes it, and anything else merely drops it.
        // That is what every editor does, and a highlight the next keystroke ignored would be a lie —
        // the user would type expecting a replacement and get an insertion beside the still-highlighted
        // text. Ctrl-C keeps the highlight (copy). Ctrl-V also keeps it so paste can replace the
        // selection the way every editor does (`insert_draft_text` drains it).
        if draft_selection().is_some()
            && !matches!(
                key,
                Key::CtrlC | Key::Char('\u{3}') | Key::Char('\u{16}')
            )
        {
            match key {
                Key::Backspace | Key::Del => {
                    delete_draft_selection();
                    hist_idx = None;
                    repaint();
                    continue;
                }
                // Fall through to the insert arm below, which now inserts at the collapsed caret.
                Key::Char(c) if !c.is_control() => delete_draft_selection(),
                _ => clear_draft_selection(),
            }
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
                // If the `@` file picker is open, Enter completes the file (same as Tab) instead of
                // submitting — the user can then continue typing or hit Enter again to send.
                {
                    let at = {
                        let r = render().lock().unwrap();
                        let m = at_matches(&r.draft);
                        (!m.is_empty()).then(|| {
                            (
                                m[r.at_sel.min(m.len() - 1)].clone(),
                                draft_at_prefix_start(&r.draft),
                            )
                        })
                    };
                    if let Some((path, at_start)) = at {
                        let mut r = render().lock().unwrap();
                        let pre: String = r.draft[..at_start].iter().collect();
                        let new_draft = format!("{pre}@{path} ");
                        r.draft = new_draft.chars().collect();
                        r.cursor = r.draft.len();
                        r.at_sel = 0;
                        drop(r);
                        hist_idx = None;
                        repaint();
                        continue;
                    }
                }
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
                        Some(matches[r.palette_sel.min(matches.len() - 1)].name.clone())
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
                    // Resolving `/wo` from the palette must reach the same mid-turn path a fully
                    // typed `/workflows` does, or the panel would open live only for whoever types
                    // the whole name. No argument can ride this branch (the palette hides itself the
                    // moment a space is typed), so the stop verb is unreachable here by construction.
                    if handle_status_command_inline(&name, "") {
                        continue;
                    }
                    if sub_tx.send(Submission::Slash(name)).is_err() {
                        return;
                    }
                    note_submission_enqueued();
                    // No park decision here: the command is only queued, and whether it opens a menu
                    // is the REPL's business (it calls `suspend`, which raises `KEYBOARD_PARKED` and
                    // the loop head stands down). Deciding here meant guessing from the name, at the
                    // wrong moment — see `KEYBOARD_PARKED`.
                    continue;
                }
                let trimmed = line.trim().to_string();
                if trimmed.is_empty() && images == 0 {
                    continue; // empty enter → ignore
                }
                if !trimmed.is_empty() {
                    history.push(trimmed.clone());
                }
                // `>` PREFIX = STEER (third entry point, terminal-independent): plain Enter on
                // `> also update the README` hands the rest to the running turn. Alt+Enter is the
                // ergonomic path but some terminals eat it (Windows Terminal binds it to fullscreen),
                // and Ctrl-S can be swallowed by legacy XON/XOFF flow control — a typed prefix always
                // arrives. Refusal (idle / backlog full) falls through to the ordinary queue path with
                // the marker stripped, so the message is delivered either way, never lost.
                let mut line = line;
                if let Some(rest) = trimmed.strip_prefix('>').filter(|_| images == 0) {
                    if crate::core::steer::push(rest) {
                        continue;
                    }
                    // Refused (idle, or the backlog is full) → fall through as an ordinary message
                    // with the routing character removed, so the model never sees the `>` marker.
                    line = rest.trim().to_string();
                }
                // `?` PREFIX = ASIDE: a quick side question answered on a SEPARATE worker thread
                // without perturbing the turn in flight (no history mutation, no cancel, no WORKING).
                // Only meaningful WHILE a turn runs — an aside beside idle is just an ordinary
                // question, so we gate on `turn_in_flight()` and otherwise fall through with the
                // marker stripped. A refused aside (no worker / blank / oversized) also falls through,
                // so the text is delivered either way and the model never sees the `?`. Not offered
                // for a vision message (an image belongs to the main turn).
                else if let Some(rest) = trimmed.strip_prefix('?').filter(|_| images == 0) {
                    if turn_in_flight() && crate::core::aside::ask(rest) {
                        continue;
                    }
                    line = rest.trim().to_string();
                }
                // A leading `/` is not enough to make a line a command — an XPath, a POSIX path, or
                // prose that merely starts with a slash (`/help... abcd`) used to be swallowed here
                // and answered with "unknown command" instead of reaching the model. `slash::classify`
                // is the single shared decision; all three dispatch surfaces call it.
                match crate::features::slash::classify(&trimmed).filter_command(images == 0) {
                    crate::features::slash::Verdict::Command { name, arg } => {
                        if handle_status_command_inline(&name, &arg) {
                            continue;
                        }
                        // Re-join name and arg: `handle_slash` re-splits, and the REPL's
                        // `slash_is_interactive` check keys off the whole line.
                        let cmd = if arg.is_empty() {
                            name
                        } else {
                            format!("{name} {arg}")
                        };
                        // No park decision here either (see the pick branch): if this command opens
                        // a menu, the REPL's `suspend()` raises KEYBOARD_PARKED and the loop head
                        // stands down — whenever that actually happens, including after a turn ends.
                        if sub_tx.send(Submission::Slash(cmd)).is_err() {
                            return;
                        }
                        note_submission_enqueued();
                    }
                    // Close to a command but not one: say so and stop. Auto-running the nearest
                    // match would let a slipped keystroke (`/claer`) wipe the conversation.
                    crate::features::slash::Verdict::DidYouMean { typed, best } => {
                        note_line(
                            &theme::muted(format!("/{typed} — did you mean /{best}?")).to_string(),
                        );
                    }
                    crate::features::slash::Verdict::Chat => {
                        // Image data URLs aren't carried here (the box only tracks a count); the
                        // REPL resolves attachments — we forward the text and the clipboard images
                        // live in shared state drained by the caller.
                        let imgs = take_pending_images();
                        if sub_tx.send(Submission::Chat(line, imgs)).is_err() {
                            return;
                        }
                        note_submission_enqueued();
                    }
                }
            }
            Key::Escape | Key::Char('\u{3}') | Key::Char('\u{4}') | Key::CtrlC => {
                // Key off `turn_in_flight`, not `WORKING`: the latter is false during turn PREP
                // (retrieval, checkpoint, registry build), and an Esc there used to be swallowed as
                // "clear the draft" while the turn went on to start anyway.
                if turn_in_flight() {
                    request_cancel(); // cooperative: lets a running tool (e.g. a long shell) abort now
                    let _ = cancel_tx.send(()); // and wake the REPL's select! at the next yield point
                                                // Esc means "stop everything" — a steer aimed at the turn being killed is moot, and
                                                // leaving it pending would re-inject it into the NEXT turn out of context (the REPL
                                                // also flushes the submission queue for the same reason).
                    crate::core::steer::clear();
                } else if matches!(key, Key::CtrlC | Key::Char('\u{3}')) {
                    // Ctrl-C carries TWO meanings, and copy takes the first press.
                    //
                    // The terminal's own Ctrl-C-copies-selection never reaches us: mouse capture is on
                    // (it has to be — it is what stops `alternateScroll` leaking wheel ticks in as
                    // ↑/↓), so the terminal has no selection of its own to copy, and the key arrives
                    // here as a plain `\u{3}`. Copying therefore has to be implemented on this side.
                    //
                    // Quitting still owns the key, just not the first press when there is something to
                    // copy: `ctrl_c_armed` makes the immediate next press quit, and expires so a press
                    // long afterwards is a fresh copy rather than a surprise exit. With nothing to copy
                    // (no highlight, empty draft) the first press quits exactly as before — which is
                    // the state the key is pressed in when someone means to leave.
                    let target = ctrl_c_copy_target();
                    if ctrl_c_action(ctrl_c_armed.map(|t| t.elapsed()), target.is_some())
                        == CtrlC::Copy
                    {
                        // `unwrap` is sound: `Copy` is only returned when `has_target` was true.
                        let (text, what) = target.expect("Copy implies a target");
                        let ok = copy_to_os_clipboard(&text);
                        note_ctrl_c_copy(&text, ok, what);
                        ctrl_c_armed = Some(Instant::now());
                        continue;
                    }
                    // Nothing to copy, or the second press inside the window: quit. Unconditional
                    // process exit — it must not merely clear a draft first, or some Windows terminals
                    // kill us before the REPL reaches its normal `deactivate()` cleanup path.
                    let _ = sub_tx.send(Submission::Quit);
                    return;
                } else {
                    // Esc / Ctrl-D never quit anymore: they only clear a pending draft (and drop image
                    // attachments). An empty prompt press is a no-op — closing the app is Ctrl-C only.
                    let mut r = render().lock().unwrap();
                    if !r.draft.is_empty() || r.images > 0 {
                        r.draft.clear();
                        r.cursor = 0;
                        r.images = 0;
                        drop(r);
                        clear_pending_images();
                        hist_idx = None;
                        repaint();
                    }
                }
            }
            Key::Tab => {
                // Tab completes the highlighted `@file` picker entry, or the slash palette.
                // For `@file`: replace the `@<prefix>` token at the end of draft with the chosen path.
                let at = {
                    let r = render().lock().unwrap();
                    let m = at_matches(&r.draft);
                    (!m.is_empty()).then(|| {
                        (
                            m[r.at_sel.min(m.len() - 1)].clone(),
                            draft_at_prefix_start(&r.draft),
                        )
                    })
                };
                if let Some((path, at_start)) = at {
                    let mut r = render().lock().unwrap();
                    // Replace `@<prefix>` with the chosen path + space.
                    let pre: String = r.draft[..at_start].iter().collect();
                    let new_draft = format!("{pre}@{path} ");
                    r.draft = new_draft.chars().collect();
                    r.cursor = r.draft.len();
                    r.at_sel = 0;
                    drop(r);
                    hist_idx = None;
                    repaint();
                    continue;
                }
                // Fall through to slash completion.
                let name = {
                    let r = render().lock().unwrap();
                    let m = slash_matches(&r.draft);
                    (!m.is_empty()).then(|| m[r.palette_sel.min(m.len() - 1)].name.clone())
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
            Key::Char('\u{16}') => {
                // Ctrl-V: paste clipboard TEXT into the draft. Required on classic conhost/CMD
                // (and any host that neither bracketed-pastes nor injects a key burst): mouse
                // capture steals the terminal's right-click paste, and Ctrl-V arrives here as a
                // control char rather than as characters. Images stay on Ctrl-O so a screenshot
                // on the clipboard doesn't silently become a vision attach when the user meant
                // to paste prose.
                if paste_clipboard_into_draft() {
                    hist_idx = None;
                    last_arrival = None;
                    last_arrival_prev = None;
                    pending_paste_repaint = false;
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
            Key::Char('\u{c}') => {
                // Ctrl-L: repaint the screen from scratch, the terminal convention. This is the
                // manual recovery hatch for a frame the renderer can no longer fix on its own —
                // anything that wrote to the terminal behind its back (a stray print from a
                // subsystem, a child process's output, a terminal that mangled a wide glyph) leaves
                // ratatui's cell diff believing cells hold content they don't, so the debris
                // survives every subsequent partial repaint. `force_redraw` clears first, making
                // the next frame unconditional. The transcript is rebuilt from `AppState.blocks`,
                // so nothing is lost — scroll position and draft included.
                force_redraw();
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
                // Paste throttle: during a paste burst (hundreds of chars arriving <50ms apart), skip
                // repaint for every char. Only repaint once when the burst ends. Cuts paste lag from
                // O(n chars) repaints to 1 final repaint showing the complete text instantly.
                if !in_paste_burst {
                    repaint();
                }
            }
            Key::Backspace => {
                let mut r = render().lock().unwrap();
                if r.cursor > 0 {
                    let cur = r.cursor - 1;
                    r.draft.remove(cur);
                    r.cursor = cur;
                    r.palette_sel = 0;
                    drop(r);
                    hist_idx = None;
                    if !in_paste_burst {
                        repaint();
                    }
                }
            }
            Key::Del => {
                let mut r = render().lock().unwrap();
                if r.cursor < r.draft.len() {
                    let cur = r.cursor;
                    r.draft.remove(cur);
                    r.palette_sel = 0;
                    drop(r);
                    if !in_paste_burst {
                        repaint();
                    }
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
            Key::PageUp if retained::is_active() => {
                retained::scroll(-8);
            }
            Key::PageDown if retained::is_active() => {
                retained::scroll(8);
            }
            Key::End if retained::is_active() && render().lock().unwrap().draft.is_empty() => {
                retained::scroll_end();
            }
            Key::End => {
                let mut r = render().lock().unwrap();
                r.cursor = r.draft.len();
                drop(r);
                repaint();
            }
            Key::ArrowUp => {
                // `@` file picker takes priority — ↑ moves up the file list.
                let at_len = { at_matches(&render().lock().unwrap().draft).len() };
                if at_len > 0 {
                    let mut r = render().lock().unwrap();
                    if retained::is_active() {
                        r.at_sel = r.at_sel.saturating_sub(1);
                    } else if r.at_sel + 1 < at_len {
                        r.at_sel += 1;
                    }
                    drop(r);
                    repaint();
                    continue;
                }
                // While the slash palette is open, ↑/↓ move the highlight over the FULL match list. The
                // two backends stack the list in OPPOSITE directions: classic draws index 0 nearest the
                // box (list climbs UP, so ↑ = index+1), retained's overlay draws index 0 at the TOP
                // (list runs DOWN, so ↑ = index-1). Match the visual direction per backend.
                let pal = {
                    let r = render().lock().unwrap();
                    slash_matches(&r.draft).len()
                };
                if pal > 0 {
                    let mut r = render().lock().unwrap();
                    if retained::is_active() {
                        r.palette_sel = r.palette_sel.saturating_sub(1);
                    } else if r.palette_sel + 1 < pal {
                        r.palette_sel += 1;
                    }
                    drop(r);
                    repaint();
                    continue;
                }
                // ↑ at the prompt recalls the previous message (readline-style), in BOTH backends.
                // Transcript scrolling lives on PageUp/PageDown, so arrows are never stolen from recall.
                if history.is_empty() {
                    continue;
                }
                recall_history_prev(&mut hist_idx, &mut draft_saved, &history);
            }
            Key::ArrowDown => {
                // `@` file picker ↓.
                let at_len = { at_matches(&render().lock().unwrap().draft).len() };
                if at_len > 0 {
                    let mut r = render().lock().unwrap();
                    if retained::is_active() {
                        if r.at_sel + 1 < at_len {
                            r.at_sel += 1;
                        }
                    } else {
                        r.at_sel = r.at_sel.saturating_sub(1);
                    }
                    drop(r);
                    repaint();
                    continue;
                }
                let pal = {
                    let r = render().lock().unwrap();
                    slash_matches(&r.draft).len()
                };
                if pal > 0 {
                    let mut r = render().lock().unwrap();
                    if retained::is_active() {
                        if r.palette_sel + 1 < pal {
                            r.palette_sel += 1;
                        }
                    } else {
                        r.palette_sel = r.palette_sel.saturating_sub(1);
                    }
                    drop(r);
                    repaint();
                    continue;
                }
                // Symmetric to ArrowUp: ↓ walks history forward. No transcript-scroll hijack.
                recall_history_next(&mut hist_idx, &draft_saved, &history);
            }
            _ => {}
        }
    }
}

// ── pending clipboard image attachments (set by Ctrl-O in the input thread, drained on submit) ──

/// Find the char index in `draft` where the last `@<prefix>` token starts (the `@` character
/// position). Used to replace the partial token with the completed path on Tab/Enter.
fn draft_at_prefix_start(draft: &[char]) -> usize {
    let s: String = draft.iter().collect();
    s.char_indices()
        .rev()
        .find(|&(i, c)| {
            c == '@' && (i == 0 || s[..i].chars().last().map_or(true, |p| p.is_whitespace()))
        })
        .map(|(i, _)| {
            // convert byte offset back to char index
            s[..i].chars().count()
        })
        .unwrap_or(draft.len())
}
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

/// Whether the `/model` overlay is open (input thread routes ↑↓/Enter/Esc to it).
pub fn model_menu_active() -> bool {
    model_menu_slot().lock().unwrap().active
}

/// True when the sticky footer REPL should use the in-terminal `/model` overlay (not dialoguer).
///
/// The opening half of the in-terminal `/model` picker. Currently unreachable: `/model` goes through
/// `pick_model_from` (dialoguer) instead. The *closing* half — `model_menu_active`, the slot, the key
/// routing in the input thread — is still live, so deleting only the openers would leave a menu that
/// can be driven but never shown. Kept whole so reconnecting the picker is a one-line call.
#[allow(dead_code)]
pub fn sticky_model_picker_available() -> bool {
    std::io::stdout().is_terminal() && !crate::core::cli_config::branded_flag("NO_STICKY")
}

/// Open the `/model` overlay immediately (loading state). Call [`model_menu_populate`] after fetch.
#[allow(dead_code)]
pub fn model_menu_begin() -> Option<oneshot::Receiver<Option<String>>> {
    if !sticky_model_picker_available() || !active() {
        return None;
    }
    let (tx, rx) = oneshot::channel();
    {
        let mut slot = model_menu_slot().lock().unwrap();
        *slot = ModelMenuState {
            active: true,
            sel: 0,
            rows: Vec::new(),
            done_tx: Some(tx),
        };
    }
    {
        let mut r = render().lock().unwrap();
        r.model_menu_active = true;
        r.model_menu_sel = 0;
        r.model_menu_rows.clear();
        r.palette_sel = 0;
        r.draft.clear();
        r.cursor = 0;
    }
    repaint_force();
    Some(rx)
}

/// Fill the model list after the provider responds (overlay must already be open).
#[allow(dead_code)]
pub fn model_menu_populate(models: Vec<String>, labels: Vec<String>, default_sel: usize) {
    if !model_menu_active() {
        return;
    }
    let rows: Vec<ModelMenuRow> = models
        .into_iter()
        .zip(labels)
        .map(|(id, label)| ModelMenuRow { id, label })
        .collect();
    if rows.is_empty() {
        return;
    }
    let sel = default_sel.min(rows.len() - 1);
    {
        let mut slot = model_menu_slot().lock().unwrap();
        slot.rows = rows.clone();
        slot.sel = sel;
    }
    {
        let mut r = render().lock().unwrap();
        r.model_menu_rows = rows;
        r.model_menu_sel = sel;
    }
    repaint_force();
}

/// Cancel the overlay without picking (e.g. fetch failed).
#[allow(dead_code)]
pub fn model_menu_abort() {
    if model_menu_active() {
        model_menu_finish(None);
    }
}

/// Open the model overlay with a ready list (used when data is already in hand).
#[allow(dead_code)]
pub fn model_menu_open(
    models: Vec<String>,
    labels: Vec<String>,
    default_sel: usize,
) -> Option<oneshot::Receiver<Option<String>>> {
    if !active() || !std::io::stdout().is_terminal() || models.is_empty() {
        return None;
    }
    let rows: Vec<ModelMenuRow> = models
        .into_iter()
        .zip(labels)
        .map(|(id, label)| ModelMenuRow { id, label })
        .collect();
    let sel = default_sel.min(rows.len().saturating_sub(1));
    let (tx, rx) = oneshot::channel();
    {
        let mut slot = model_menu_slot().lock().unwrap();
        *slot = ModelMenuState {
            active: true,
            sel,
            rows: rows.clone(),
            done_tx: Some(tx),
        };
    }
    {
        let mut r = render().lock().unwrap();
        r.model_menu_active = true;
        r.model_menu_sel = sel;
        r.model_menu_rows = rows;
        r.palette_sel = 0;
    }
    repaint();
    Some(rx)
}

fn model_menu_finish(picked: Option<String>) {
    let tx = {
        let mut slot = model_menu_slot().lock().unwrap();
        slot.active = false;
        slot.rows.clear();
        slot.sel = 0;
        slot.done_tx.take()
    };
    let mut r = render().lock().unwrap();
    r.model_menu_active = false;
    r.model_menu_rows.clear();
    r.model_menu_sel = 0;
    drop(r);
    repaint_force();
    if let Some(tx) = tx {
        let _ = tx.send(picked);
    }
}

/// Whether the `/sessions` overlay is open (input thread routes ↑↓/Enter/Esc to it).
pub fn sessions_menu_active() -> bool {
    sessions_menu_slot().lock().unwrap().active
}

/// True when the sticky footer REPL should use the in-terminal `/sessions` overlay (not dialoguer).
///
/// Unreachable for the same reason as the `/model` openers above: `/sessions` runs through
/// `main.rs`'s own `sessions_menu`. The key routing and `sessions_menu_finish` remain live.
#[allow(dead_code)]
pub fn sessions_menu_available() -> bool {
    active()
        && std::io::stdout().is_terminal()
        && !crate::core::cli_config::branded_flag("NO_STICKY")
}

/// Open the `/sessions` overlay with a ready row list. Returns `None` (caller falls back to
/// dialoguer) when the sticky footer isn't active or the list is empty.
#[allow(dead_code)]
pub fn sessions_menu_open(
    rows: Vec<(String, String)>,
    default_sel: usize,
    deletable_rows: usize,
) -> Option<oneshot::Receiver<Option<SessionsMenuChoice>>> {
    if !sessions_menu_available() || rows.is_empty() {
        return None;
    }
    let rows: Vec<SessionsMenuRow> = rows
        .into_iter()
        .map(|(title, subtitle)| SessionsMenuRow { title, subtitle })
        .collect();
    let sel = default_sel.min(rows.len().saturating_sub(1));
    let (tx, rx) = oneshot::channel();
    {
        let mut slot = sessions_menu_slot().lock().unwrap();
        *slot = SessionsMenuState {
            active: true,
            sel,
            rows: rows.clone(),
            deletable_rows: deletable_rows.min(rows.len()),
            done_tx: Some(tx),
        };
    }
    {
        let mut r = render().lock().unwrap();
        r.sessions_menu_active = true;
        r.sessions_menu_sel = sel;
        r.sessions_menu_rows = rows;
        r.sessions_menu_deletable_rows = deletable_rows.min(r.sessions_menu_rows.len());
        r.palette_sel = 0;
        r.draft.clear();
        r.cursor = 0;
    }
    repaint_force();
    Some(rx)
}

/// Cancel the sessions overlay without picking.
#[allow(dead_code)]
pub fn sessions_menu_abort() {
    if sessions_menu_active() {
        sessions_menu_finish(None);
    }
}

fn sessions_menu_finish(picked: Option<SessionsMenuChoice>) {
    let tx = {
        let mut slot = sessions_menu_slot().lock().unwrap();
        slot.active = false;
        slot.rows.clear();
        slot.sel = 0;
        slot.deletable_rows = 0;
        slot.done_tx.take()
    };
    let mut r = render().lock().unwrap();
    r.sessions_menu_active = false;
    r.sessions_menu_rows.clear();
    r.sessions_menu_sel = 0;
    r.sessions_menu_deletable_rows = 0;
    drop(r);
    repaint_force();
    if let Some(tx) = tx {
        let _ = tx.send(picked);
    }
}

/// Begin intercepting [`emit`]/[`emit_line`] calls. Returns false if capture is already active.
///
/// Capture + [`text_overlay_open`] were the pair that showed a print-based command's output in a
/// scrollable panel. No command routes through them now (they emit into the transcript directly), but
/// `EMIT_CAPTURING` is still honoured inside `emit`, so the capture path itself is live code.
#[allow(dead_code)]
pub fn emit_capture_begin() -> bool {
    if EMIT_CAPTURING.swap(true, Ordering::SeqCst) {
        return false;
    }
    emit_capture_slot().lock().unwrap().clear();
    true
}

/// Stop capture and return the collected source lines. ANSI/C0 controls are removed before paint so
/// captured config/provider text cannot move the terminal cursor or inject escape sequences.
#[allow(dead_code)]
pub fn emit_capture_take() -> Vec<String> {
    EMIT_CAPTURING.store(false, Ordering::SeqCst);
    std::mem::take(&mut *emit_capture_slot().lock().unwrap())
        .into_iter()
        .map(|line| {
            let plain = console::strip_ansi_codes(line.trim_end_matches('\r'));
            let mut clean = String::new();
            for c in plain.chars() {
                if c == '\t' {
                    clean.push_str("    ");
                } else if !c.is_control() {
                    clean.push(c);
                }
            }
            clean
        })
        .collect()
}

/// Cancel a capture without opening an overlay (used on early-exit/error paths).
pub fn emit_capture_abort() {
    EMIT_CAPTURING.store(false, Ordering::SeqCst);
    emit_capture_slot().lock().unwrap().clear();
}

/// Whether the pure-print text overlay currently owns keyboard input.
pub fn text_overlay_active() -> bool {
    text_overlay_slot().lock().unwrap().active
}

/// Open an informational overlay directly in retained mode (used by live workflow/status panels).
pub fn retained_overlay_open(title: impl Into<String>, text: impl Into<String>) -> bool {
    if !retained::is_active() {
        return false;
    }
    RETAINED_INFO_OVERLAY.store(true, Ordering::Relaxed);
    RETAINED_OVERLAY_GEN.fetch_add(1, Ordering::Relaxed);
    retained::open_overlay(retained::OverlaySnapshot {
        title: title.into(),
        lines: text.into().lines().map(str::to_string).collect(),
        selected: None,
        hint: "Esc/q close · PgUp/PgDn scroll".to_string(),
    });
    true
}

/// Generation counter for the informational overlay. Bumped on every open/close so a live refresher
/// from a previous `/workflows` can tell it has been superseded and exit — without this, opening the
/// panel twice would leave two threads writing the same surface.
static RETAINED_OVERLAY_GEN: AtomicU64 = AtomicU64::new(0);

/// Open an informational overlay that RE-READS itself while it stays up.
///
/// `/workflows` shows elapsed times; a one-shot snapshot froze them the moment the panel opened, so a
/// fan-out you were watching appeared stuck at whatever second you happened to press the key. The
/// refresher republishes the body (never re-opens it — see `Command::UpdateOverlay`, which preserves
/// scroll) and stops as soon as the panel closes or another overlay takes its place.
pub fn retained_overlay_open_live(
    title: impl Into<String>,
    refresh: impl Fn() -> String + Send + 'static,
) -> bool {
    if !retained_overlay_open(title, refresh()) {
        return false;
    }
    let gen = RETAINED_OVERLAY_GEN.load(Ordering::Relaxed);
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(900));
        // Three ways to become obsolete: the panel was closed, a different overlay was opened, or the
        // render thread went away (suspend/shutdown). Any of them ends this thread.
        if !RETAINED_INFO_OVERLAY.load(Ordering::Relaxed)
            || RETAINED_OVERLAY_GEN.load(Ordering::Relaxed) != gen
            || !retained::is_running()
        {
            return;
        }
        retained::update_overlay(refresh().lines().map(str::to_string).collect());
    });
    true
}

pub fn retained_overlay_close() {
    RETAINED_INFO_OVERLAY.store(false, Ordering::Relaxed);
    RETAINED_OVERLAY_GEN.fetch_add(1, Ordering::Relaxed);
    if retained::is_running() {
        retained::close_overlay();
        repaint_force();
    }
}

/// True when the sticky REPL can show the native text overlay.
#[allow(dead_code)]
pub fn text_overlay_available() -> bool {
    active()
        && std::io::stdout().is_terminal()
        && !crate::core::cli_config::branded_flag("NO_STICKY")
}

/// Open captured pure-print output as a temporary scrollable overlay. Resolves when Esc/q closes it.
#[allow(dead_code)]
pub fn text_overlay_open(title: String, lines: Vec<String>) -> Option<oneshot::Receiver<()>> {
    if !text_overlay_available() || lines.is_empty() {
        return None;
    }
    let (tx, rx) = oneshot::channel();
    {
        let mut slot = text_overlay_slot().lock().unwrap();
        *slot = TextOverlayState {
            active: true,
            scroll: 0,
            title: title.clone(),
            lines: lines.clone(),
            done_tx: Some(tx),
        };
    }
    {
        let mut r = render().lock().unwrap();
        r.text_overlay_active = true;
        r.text_overlay_scroll = 0;
        r.text_overlay_title = title;
        r.text_overlay_lines = lines;
        r.palette_sel = 0;
        r.draft.clear();
        r.cursor = 0;
    }
    repaint_force();
    Some(rx)
}

/// Close the text overlay from lifecycle cleanup paths.
pub fn text_overlay_abort() {
    if text_overlay_active() {
        text_overlay_finish();
    }
}

fn text_overlay_finish() {
    let tx = {
        let mut slot = text_overlay_slot().lock().unwrap();
        slot.active = false;
        slot.scroll = 0;
        slot.title.clear();
        slot.lines.clear();
        slot.done_tx.take()
    };
    {
        let mut r = render().lock().unwrap();
        r.text_overlay_active = false;
        r.text_overlay_scroll = 0;
        r.text_overlay_title.clear();
        r.text_overlay_lines.clear();
    }
    // Drop the overlay from the retained frame. The transcript underneath lives in `AppState.blocks`,
    // so the render thread repaints it from its own state — nothing to replay from here.
    if retained::is_running() {
        retained::close_overlay();
    }
    repaint_force();
    if let Some(tx) = tx {
        let _ = tx.send(());
    }
}

/// Whether the input thread should park on `resume` after dispatching this slash (false for native overlays).
/// Drop sticky overlays and reset Windows stdin to cooked line mode (echo + line input).
pub fn prepare_dialoguer_session() {
    if model_menu_active() {
        model_menu_finish(None);
    }
    if sessions_menu_active() {
        sessions_menu_finish(None);
    }
    if text_overlay_active() {
        text_overlay_abort();
    }
    emit_capture_abort();
    let term = Term::stdout();
    let _ = term.show_cursor();
    restore_stdin_cooked();
    if active() {
        let _ = writeln!(std::io::stdout());
        let _ = std::io::stdout().flush();
    }
}

/// After the crossterm input loop / dialoguer, `stdin` may be raw on Windows — `read_line` then shows
/// nothing. Also clears crossterm's own raw-mode latch so its state agrees with the cooked mode we set.
pub fn restore_stdin_cooked() {
    // Clear crossterm's internal raw-mode flag first (it caches the pre-raw console mode); the explicit
    // `SetConsoleMode` below then pins a clean cooked mode regardless of what crossterm restored.
    let _ = crossterm::terminal::disable_raw_mode();
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
        use windows_sys::Win32::System::Console::{
            FlushConsoleInputBuffer, GetStdHandle, SetConsoleMode, ENABLE_ECHO_INPUT,
            ENABLE_LINE_INPUT, ENABLE_PROCESSED_INPUT, STD_INPUT_HANDLE,
        };
        unsafe {
            let h = GetStdHandle(STD_INPUT_HANDLE);
            if h.is_null() || h == INVALID_HANDLE_VALUE {
                return;
            }
            let _ = FlushConsoleInputBuffer(h);
            // Cooked line mode from scratch — OR-ing onto a leftover raw/dialoguer mode often leaves echo off.
            let cooked = ENABLE_ECHO_INPUT | ENABLE_LINE_INPUT | ENABLE_PROCESSED_INPUT;
            let _ = SetConsoleMode(h, cooked);
        }
    }
}

/// Read one line with visible echo (types + paste show on screen). Uses `console::Term::read_line`
/// so input works after sticky TUI / dialoguer on Windows; `std::io::stdin().read_line` often stays silent.
///
/// No current caller: every prompt that needed it now runs inside a `dialoguer` suspend window, which
/// does its own echoing. Kept because the Windows behaviour it works around is a property of the
/// platform, not of the call site that used to hit it.
#[allow(dead_code)]
pub fn read_visible_line(prompt: &str) -> std::io::Result<String> {
    restore_stdin_cooked();
    let term = Term::stdout();
    let _ = term.show_cursor();
    term.write_str(prompt)?;
    term.flush()?;
    let line = term.read_line()?;
    Ok(line)
}

/// Handle one key while the model overlay is open. Returns true if the key was consumed.
fn model_menu_handle_key(key: &Key) -> bool {
    if !model_menu_active() {
        return false;
    }
    match key {
        Key::ArrowUp | Key::Char('k') | Key::Char('K') => {
            let n = model_menu_slot().lock().unwrap().rows.len();
            if n == 0 {
                return true;
            }
            let mut slot = model_menu_slot().lock().unwrap();
            if slot.sel > 0 {
                slot.sel -= 1;
                let s = slot.sel;
                drop(slot);
                render().lock().unwrap().model_menu_sel = s;
                repaint();
            }
            true
        }
        Key::ArrowDown | Key::Char('j') | Key::Char('J') => {
            let n = model_menu_slot().lock().unwrap().rows.len();
            if n == 0 {
                return true;
            }
            let mut slot = model_menu_slot().lock().unwrap();
            if slot.sel + 1 < slot.rows.len() {
                slot.sel += 1;
                let s = slot.sel;
                drop(slot);
                render().lock().unwrap().model_menu_sel = s;
                repaint();
            }
            true
        }
        Key::Enter => {
            let pick = {
                let slot = model_menu_slot().lock().unwrap();
                if slot.rows.is_empty() {
                    None
                } else {
                    slot.rows.get(slot.sel).map(|r| r.id.clone())
                }
            };
            if pick.is_none() {
                return true;
            }
            model_menu_finish(pick);
            true
        }
        Key::Escape | Key::Char('\u{3}') | Key::Char('\u{4}') => {
            model_menu_finish(None);
            true
        }
        _ => true, // swallow other keys so they don't edit the draft mid-menu
    }
}

/// Handle one key while the `/sessions` overlay is open. Returns true if the key was consumed.
/// Enter resolves `Pick`; d/Del resolves `Delete` only for the leading deletable session rows.
fn sessions_menu_handle_key(key: &Key) -> bool {
    if !sessions_menu_active() {
        return false;
    }
    match key {
        Key::ArrowUp | Key::Char('k') | Key::Char('K') => {
            let mut slot = sessions_menu_slot().lock().unwrap();
            if slot.rows.is_empty() {
                return true;
            }
            if slot.sel > 0 {
                slot.sel -= 1;
                let s = slot.sel;
                drop(slot);
                render().lock().unwrap().sessions_menu_sel = s;
                repaint();
            }
            true
        }
        Key::ArrowDown | Key::Char('j') | Key::Char('J') => {
            let mut slot = sessions_menu_slot().lock().unwrap();
            if slot.rows.is_empty() {
                return true;
            }
            if slot.sel + 1 < slot.rows.len() {
                slot.sel += 1;
                let s = slot.sel;
                drop(slot);
                render().lock().unwrap().sessions_menu_sel = s;
                repaint();
            }
            true
        }
        Key::Enter => {
            let pick = {
                let slot = sessions_menu_slot().lock().unwrap();
                if slot.rows.is_empty() {
                    None
                } else {
                    Some(SessionsMenuChoice::Pick(slot.sel))
                }
            };
            if pick.is_none() {
                return true;
            }
            sessions_menu_finish(pick);
            true
        }
        Key::Char('d') | Key::Char('D') | Key::Del => {
            let pick = {
                let slot = sessions_menu_slot().lock().unwrap();
                (slot.sel < slot.deletable_rows).then_some(SessionsMenuChoice::Delete(slot.sel))
            };
            if let Some(pick) = pick {
                sessions_menu_finish(Some(pick));
            }
            true
        }
        Key::Escape | Key::Char('\u{3}') | Key::Char('\u{4}') | Key::CtrlC => {
            sessions_menu_finish(None);
            true
        }
        _ => true, // swallow other keys so they don't edit the draft mid-menu
    }
}

/// Handle one key while the pure-print text overlay is open. Returns true if consumed.
///
/// Scrolling is delegated to the render thread (`Command::Scroll`/`ScrollEnd`), which owns the
/// overlay's offset and clamps it against the overlay's own visible height at draw time — so a
/// PageDown past the end snaps back to the last page instead of drifting into empty space. That
/// removes the need to re-derive the wrapped line count and page height on this thread, which is
/// also why the row/column geometry no longer has to be mirrored into the shared state.
fn text_overlay_handle_key(key: &Key) -> bool {
    if !text_overlay_active() {
        return false;
    }
    // Sign convention matches the informational-overlay handler in `input_loop`: a negative delta
    // pages forward through the overlay body, positive pages back.
    match key {
        Key::ArrowUp | Key::Char('k') | Key::Char('K') => {
            retained::scroll(1);
            true
        }
        Key::ArrowDown | Key::Char('j') | Key::Char('J') => {
            retained::scroll(-1);
            true
        }
        Key::PageUp => {
            retained::scroll(8);
            true
        }
        Key::PageDown => {
            retained::scroll(-8);
            true
        }
        Key::Home | Key::End => {
            retained::scroll_end();
            true
        }
        Key::Escape
        | Key::Char('q')
        | Key::Char('Q')
        | Key::Char('\u{3}')
        | Key::Char('\u{4}')
        | Key::CtrlC => {
            text_overlay_finish();
            true
        }
        _ => true, // swallow other keys so they don't edit the draft under the overlay
    }
}

/// Whether this slash command line opens a `dialoguer` menu (or a daemon) that takes over stdin, so
/// the REPL must [`suspend`] the retained frame before running it.
///
/// The rule itself is a `stdin:` field on the command's row in [`crate::features::slash::BUILTINS`],
/// so it cannot drift from the command's name or its aliases. This function stays only because the
/// REPL calls it by this name; it forwards. Two earlier copies of this decision — one here, one in
/// `main.rs::slash_is_interactive` — had already drifted apart once, which is why it is a field now.
///
/// Takes the FULL command line, because whether stdin is claimed depends on the argument: bare
/// `/effort` drags a slider, `/effort high` just sets it; `/tools` prints, `/tools menu` picks.
pub fn slash_takes_stdin(input: &str) -> bool {
    crate::features::slash::takes_stdin(input)
}

// ── the animated `/effort` slider ─────────────────────────────────────────────
// A keyboard-dragged horizontal slider for the per-turn reasoning-effort tier. Four discrete stops
// (`auto` · `low` · `medium` · `high`) sit on a rail; a moonlit knob slides between them with an
// ease-out glide (the "kéo"/drag feel), and a small pulse plays on commit. Colour keys the mood —
// auto is calm moonlight, low goes green (light & cheap), medium dim-silver, high burns the reserved
// gold (runs hot). Runs while the sticky box is SUSPENDED (or in the plain REPL), so it owns stdin;
// degrades to a no-op (returns `None`) off-TTY. The caller maps the returned index to config writes.

/// Rail inner width in cells (index range `0..=RAIL`). Widened 39 → 48 to seat the 7th stop
/// (`ultimate`) so its 8-char label clears `max` without colliding.
const RAIL: usize = 48;
/// Cell position of each tier's notch on the rail. Spaced so no two labels overlap — the last gap is
/// a touch wider to clear the long `ultimate` label pinned at the rail end.
const NOTCHES: [usize; 7] = [0, 8, 16, 24, 31, 38, 48];
/// Stop labels, left→right. Index is the value returned by [`effort_slider`]. The last stop,
/// `ultimate`, is not merely a hotter tier — it's the mode toggle (max effort + orchestrate-by-default),
/// folded onto the far end of the rail so one drag reaches it.
const E_TIERS: [&str; 7] = ["auto", "low", "medium", "high", "xhigh", "max", "ultimate"];
/// One-line gist shown under the focused stop.
const E_DESCS: [&str; 7] = [
    "detect per-turn from your wording — keyword + complexity",
    "minimal reasoning — fastest & cheapest",
    "balanced reasoning — the middle ground",
    "deep reasoning — the everyday ceiling",
    "deeper exploration — always thinks deeply",
    "no limit on thinking depth — slowest & most thorough",
    "max effort + auto-launches workflows — aizen's ultracode",
];
/// Rows the slider block occupies (title · blank · rail · labels · blank · desc · hint).
const SLIDER_ROWS: usize = 7;

/// The resting knob glyph for a tier: the signature ✦ for `ultimate` (its brand mark, tying it to the
/// `✦ ultimate` chip), a plain ● for every other stop. Swapped out during the commit pulse.
fn rest_glyph(sel: usize) -> &'static str {
    if sel == E_TIERS.len() - 1 {
        "✦"
    } else {
        "●"
    }
}

/// The moonlight-palette colour for a tier: auto = accent, low = green (ok), medium = dim silver,
/// high/xhigh = the reserved warm gold (matches the `⚡ yolo` "runs hot" cue), max/ultimate = salmon
/// (the hottest end). high/xhigh share the gold and max/ultimate share the salmon; within each pair
/// the label text and the knob glyph (● vs ✦) tell them apart.
fn e_color(i: usize) -> u8 {
    match i {
        1 => theme::OK,
        2 => theme::ACCENT_DIM,
        3 | 4 => theme::WARN,
        5 | 6 => theme::ERR,
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
///
/// The fill isn't flat: the two cells right behind the knob glow brighter (bold) than the settled
/// track — a small **comet tail** so a moving knob reads as motion, not a teleport. The `ultimate`
/// stop burns the whole track bold salmon: the visual "this is the hot end" cue.
fn slider_frame(sel: usize, knob: usize, glyph: &str) -> String {
    let col = e_color(sel);
    let ultimate = sel == E_TIERS.len() - 1;
    let mut out = String::new();
    // 1) title
    out.push_str("\x1b[2K");
    out.push_str(&theme::muted("  reasoning effort").to_string());
    out.push('\n');
    // 2) blank
    out.push_str("\x1b[2K\n");
    // 3) rail — settled fill up to the knob with a bright comet tail behind it, faint track beyond.
    out.push_str("\x1b[2K  ");
    for c in 0..=RAIL {
        if c == knob {
            out.push_str(&style(glyph).color256(col).bold().to_string());
        } else if c < knob {
            // The two cells just behind the knob glow (bold) then settle to the plain tinted track;
            // ultimate burns the whole track bold.
            if ultimate || knob - c <= 2 {
                out.push_str(&style("━").color256(col).bold().to_string());
            } else {
                out.push_str(&style("━").color256(col).to_string());
            }
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
    out.push_str(
        &style(format!("› {}", E_DESCS[sel]))
            .color256(col)
            .to_string(),
    );
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
/// dragging animation. `to` is the destination tier, so the fill/labels recolour to it as it moves,
/// and the moving knob already wears the destination's resting glyph (✦ when sliding onto ultimate).
/// Frame count scales with the distance travelled so a one-notch nudge and a far throw both glide at
/// the same per-cell speed (a fixed count made long throws blur past and short ones crawl).
fn slider_glide(from: usize, to: usize) {
    let (a, b) = (NOTCHES[from] as f32, NOTCHES[to] as f32);
    let span = (b - a).abs();
    // ~1 frame per 3 rail cells, clamped so even a neighbour hop shows a few in-between positions.
    let frames = ((span / 3.0).round() as usize).clamp(5, 12);
    let glyph = rest_glyph(to);
    for f in 1..=frames {
        let t = f as f32 / frames as f32;
        let e = 1.0 - (1.0 - t).powi(3); // ease-out
        let cell = (a + (b - a) * e).round() as usize;
        slider_redraw(&slider_frame(to, cell, glyph));
        std::thread::sleep(Duration::from_millis(14));
    }
}

/// A pulse on the knob when the choice is committed (a little "click" of feedback): the knob swells
/// through a ring then settles back to its resting glyph — ✦ blooms to a star burst for ultimate,
/// ● to a filled ring for the rest.
fn slider_commit_pulse(sel: usize) {
    let rest = rest_glyph(sel);
    let bloom = if sel == E_TIERS.len() - 1 {
        "✧"
    } else {
        "◉"
    };
    for g in [bloom, rest, bloom, rest] {
        slider_redraw(&slider_frame(sel, NOTCHES[sel], g));
        std::thread::sleep(Duration::from_millis(50));
    }
    // Land on the resting glyph so the committed frame matches the steady state.
    slider_redraw(&slider_frame(sel, NOTCHES[sel], rest));
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
    println!("{}", slider_frame(sel, NOTCHES[sel], rest_glyph(sel)));
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
    fn health_kind_labels_and_colours_are_stable() {
        // Idle chip copy + palette must stay distinct so green/yellow/red keep meaning.
        assert_eq!(HealthKind::Ok.label(false), "ready");
        assert_eq!(HealthKind::Ok.label(true), "ok");
        assert_eq!(HealthKind::Unstable.label(false), "unstable");
        assert_eq!(HealthKind::Down.label(false), "down");
        assert_eq!(HealthKind::Unknown.label(false), "checking");
        assert_eq!(HealthKind::Ok.color_code(), theme::OK);
        assert_eq!(HealthKind::Unstable.color_code(), theme::WARN);
        assert_eq!(HealthKind::Down.color_code(), theme::ERR);
        assert_eq!(HealthKind::Unknown.color_code(), theme::MUTED);
    }

    #[test]
    fn draft_selection_normalizes_and_ignores_a_bare_click() {
        // Drag right, drag left — same range either way.
        assert_eq!(normalized_draft_sel(Some((2, 5)), 10), Some((2, 5)));
        assert_eq!(normalized_draft_sel(Some((5, 2)), 10), Some((2, 5)));
        // A plain click leaves anchor == cursor. That is a caret move, NOT a one-char selection — if it
        // read as one, the next keystroke would silently eat the char under the click.
        assert_eq!(normalized_draft_sel(Some((3, 3)), 10), None);
        assert_eq!(normalized_draft_sel(None, 10), None);
        // A range left over from a longer draft (history recall, Esc, a submit) clamps, and collapses to
        // nothing rather than pointing at whatever now sits at those indices.
        assert_eq!(normalized_draft_sel(Some((1, 99)), 4), Some((1, 4)));
        assert_eq!(normalized_draft_sel(Some((7, 9)), 4), None);
    }

    /// Typing over a highlight replaces it — asserted through the shared draft state, in one test
    /// because that state is process-global and splitting it would race across test threads.
    #[test]
    fn editing_over_an_input_selection_removes_the_highlighted_chars() {
        {
            let mut r = render().lock().unwrap();
            r.draft = "hello world".chars().collect();
            r.cursor = 11;
            r.draft_sel = Some((6, 11)); // "world", dragged right-to-left or left-to-right
        }
        assert_eq!(draft_selection(), Some((6, 11)));
        delete_draft_selection();
        {
            let r = render().lock().unwrap();
            assert_eq!(r.draft.iter().collect::<String>(), "hello ");
            assert_eq!(
                r.cursor, 6,
                "the caret collapses onto where the range started"
            );
            assert!(r.draft_sel.is_none(), "and the highlight is gone with it");
        }
        // Idempotent: a second call with nothing selected must not eat a char.
        delete_draft_selection();
        assert_eq!(
            render().lock().unwrap().draft.iter().collect::<String>(),
            "hello "
        );
        // Leave the shared state clean for anything else that reads it.
        let mut r = render().lock().unwrap();
        r.draft.clear();
        r.cursor = 0;
    }

    #[test]
    fn session_allow_short_circuits_approval() {
        reset_session_allow();
        assert!(!session_allow_all(), "starts off");
        // When session-allow is set, ask_approval returns true immediately (no input thread needed).
        SESSION_ALLOW.store(true, Ordering::Relaxed);
        assert!(
            ask_approval("⚙ file_edit x — approve?"),
            "allow-all short-circuits to true"
        );
        reset_session_allow();
        assert!(!session_allow_all(), "reset clears it");
    }

    /// The Esc-responsiveness invariant, pinned end to end.
    ///
    /// `turn_in_flight` — not `WORKING` — is what the input thread keys Esc off. `WORKING` is only
    /// flipped immediately before the model call, so it is FALSE for the whole prep stretch
    /// (retrieval, checkpoint, LSP spawn, registry build). An armed token has to cover that window,
    /// or Esc lands in the idle branch and just clears the draft while the turn starts anyway. All
    /// three phases are asserted in one test because the state is process-global — splitting them
    /// would let the phases race each other across parallel test threads.
    #[test]
    fn esc_is_live_across_prep_working_and_teardown() {
        let _g = TEST_CANCEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let token = crate::core::cancel::TurnCancel::new();
        // Sanity: a clean slot with no turn reports idle, so Esc clears the draft (and Ctrl-C quits).
        disarm_cancel(&token);
        WORKING.store(false, Ordering::Relaxed);

        // PREP — armed, but `WORKING` is still false. This is the window the bug lived in.
        arm_cancel(token.clone());
        assert!(
            !WORKING.load(Ordering::Relaxed),
            "prep runs before the working pill goes up"
        );
        assert!(
            turn_in_flight(),
            "an armed token alone must make Esc mean cancel"
        );
        request_cancel();
        assert!(
            token.is_cancelled(),
            "Esc during prep must reach the turn's token"
        );

        // WORKING — the classic window; still in flight.
        let token2 = crate::core::cancel::TurnCancel::new();
        arm_cancel(token2.clone());
        WORKING.store(true, Ordering::Relaxed);
        assert!(turn_in_flight());

        // TEARDOWN — the REPL clears both; Esc goes back to being a draft-clear.
        WORKING.store(false, Ordering::Relaxed);
        disarm_cancel(&token2);
        assert!(
            !turn_in_flight(),
            "no turn ⇒ Esc must not be treated as cancel"
        );
        // Identity-checked disarm: a finished OLD turn cannot disarm the one running now.
        arm_cancel(token2.clone());
        disarm_cancel(&token);
        assert!(
            turn_in_flight(),
            "a stale token's disarm must not clear a newer turn"
        );
        disarm_cancel(&token2);
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
        assert_eq!(
            TIPS[TIP_SEED.fetch_add(1, Ordering::Relaxed) % TIPS.len()],
            a
        );
        assert_eq!(
            TIPS[TIP_SEED.fetch_add(1, Ordering::Relaxed) % TIPS.len()],
            b
        );
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
    fn slash_palette_filters_live() {
        let v = |s: &str| s.chars().collect::<Vec<_>>();
        assert!(
            slash_matches(&v("hello")).is_empty(),
            "no leading slash → no palette"
        );
        assert_eq!(
            slash_matches(&v("/")).len(),
            crate::features::slash::list().len(),
            "bare / lists the whole catalog"
        );
        let se: Vec<String> = slash_matches(&v("/se"))
            .into_iter()
            .map(|c| c.name)
            .collect();
        assert!(
            se.contains(&"sessions".to_string()) && se.contains(&"serve".to_string()),
            "/se → sessions, serve"
        );
        assert!(
            !se.contains(&"model".to_string()),
            "/se excludes non-matches"
        );
        assert!(
            slash_matches(&v("/model foo")).is_empty(),
            "once an arg is typed the palette hides"
        );
        assert!(
            !slash_matches(&v("/xyz")).iter().any(|c| c.name == "xyz"),
            "no /xyz command to complete"
        );
        // /init must be reachable from the live palette (the reported bug).
        assert!(
            slash_matches(&v("/init")).iter().any(|c| c.name == "init"),
            "/init appears in the palette"
        );
    }

    #[test]
    fn slider_frame_has_all_stops_and_bounded_rows() {
        // A frame must name every tier, describe the focused one, carry the knob glyph, and be
        // exactly SLIDER_ROWS lines (the redraw jumps up by that count — a mismatch smears the UI).
        let frame = slider_frame(2, NOTCHES[2], "●");
        for t in E_TIERS {
            assert!(frame.contains(t), "frame must show the '{t}' label");
        }
        assert!(
            frame.contains(E_DESCS[2]),
            "frame shows the focused tier's description"
        );
        assert!(frame.contains('●'), "frame carries the knob glyph");
        assert_eq!(
            frame.lines().count(),
            SLIDER_ROWS,
            "frame must be exactly SLIDER_ROWS lines"
        );
    }

    #[test]
    fn slider_notches_span_the_rail_in_order() {
        // The notches must be sorted, start at 0, end at RAIL, and match the tier count — otherwise
        // the knob would jump off the rail or land between labels.
        assert_eq!(NOTCHES.len(), E_TIERS.len(), "one notch per tier");
        assert_eq!(NOTCHES[0], 0, "first stop sits at the rail start");
        assert_eq!(
            *NOTCHES.last().unwrap(),
            RAIL,
            "last stop sits at the rail end"
        );
        assert!(
            NOTCHES.windows(2).all(|w| w[0] < w[1]),
            "notches strictly ascend"
        );
    }

    #[test]
    fn labels_line_contains_every_tier_name() {
        // Every stop's name must survive as a plain substring regardless of which is focused, so the
        // label row always reads correctly (the styling groups spans but never splits a name).
        for sel in 0..E_TIERS.len() {
            let line = labels_line(sel);
            for t in E_TIERS {
                assert!(
                    line.contains(t),
                    "labels row (sel={sel}) must contain '{t}'"
                );
            }
        }
    }

    #[test]
    fn e_color_maps_each_tier_to_a_palette_role() {
        // auto→accent, low→ok(green), medium→dim, high/xhigh→warn(gold), max/ultimate→err(salmon).
        // Guards the "hot end" escalation and the shared-hue pairs (label + glyph disambiguate).
        assert_eq!(e_color(0), theme::ACCENT);
        assert_eq!(e_color(1), theme::OK);
        assert_eq!(e_color(2), theme::ACCENT_DIM);
        assert_eq!(e_color(3), theme::WARN);
        assert_eq!(e_color(4), theme::WARN, "xhigh shares high's gold");
        assert_eq!(e_color(5), theme::ERR);
        assert_eq!(e_color(6), theme::ERR, "ultimate shares max's salmon");
    }

    #[test]
    fn ultimate_is_the_last_stop_with_the_star_glyph() {
        // The ultimate mode folds onto the far end of the effort rail, and its knob wears the ✦ brand
        // mark (tying it to the `✦ ultimate` chip) — every other stop rests on the plain ●.
        assert_eq!(*E_TIERS.last().unwrap(), "ultimate");
        assert_eq!(rest_glyph(E_TIERS.len() - 1), "✦");
        assert_eq!(rest_glyph(0), "●");
        assert_eq!(rest_glyph(3), "●");
    }

    #[test]
    fn submission_variants_roundtrip() {
        // The REPL classifies on these — guard the shape.
        let s = Submission::Chat("hi".into(), vec!["data:...".into()]);
        assert_eq!(s, Submission::Chat("hi".into(), vec!["data:...".into()]));
        assert_ne!(Submission::Quit, Submission::Slash("help".into()));
    }

    /// Ctrl-C now means two things, and getting the arbitration wrong costs the user either their
    /// clipboard or their session. The one path that must never regress: a press that copied cannot
    /// also be the press that quits, and the very next press must still be able to leave.
    #[test]
    fn ctrl_c_copies_first_then_quits() {
        // Nothing selected and an empty draft — the state someone is in when they mean to leave.
        // The key keeps its original, unconditional meaning.
        assert_eq!(ctrl_c_action(None, false), CtrlC::Quit);

        // Something to copy: the first press copies rather than exiting.
        assert_eq!(ctrl_c_action(None, true), CtrlC::Copy);

        // Immediately after a copy the key is armed, so the next press leaves — even though there is
        // still a selection sitting there. Without this, Ctrl-C would copy forever and the app could
        // not be closed by the only key that closes it.
        assert_eq!(
            ctrl_c_action(Some(Duration::from_millis(0)), true),
            CtrlC::Quit
        );
        assert_eq!(
            ctrl_c_action(Some(CTRL_C_QUIT_WINDOW - Duration::from_millis(1)), true),
            CtrlC::Quit
        );

        // The arm expires: a press long after copying is a fresh copy, not a surprise exit.
        assert_eq!(
            ctrl_c_action(Some(CTRL_C_QUIT_WINDOW), true),
            CtrlC::Copy,
            "the quit window must expire, or a copy an hour ago still closes the app"
        );

        // Expired arm with nothing left to copy still quits.
        assert_eq!(
            ctrl_c_action(Some(Duration::from_secs(60)), false),
            CtrlC::Quit
        );
    }

    #[test]
    fn slash_parking_only_claims_direct_stdin_owners() {
        assert!(slash_takes_stdin("config"));
        assert!(slash_takes_stdin("provider"));
        assert!(slash_takes_stdin("provider add"));
        assert!(slash_takes_stdin("provider manage"));
        assert!(!slash_takes_stdin("provider backup"));
        assert!(slash_takes_stdin("sessions"));
        // `/import` was missing here while `/sessions` — the same dialoguer Select — was listed, so
        // the input thread kept the keyboard and the import picker could not be paged.
        assert!(slash_takes_stdin("import"));
        assert!(slash_takes_stdin("effort"));
        assert!(!slash_takes_stdin("effort status"));
        assert!(slash_takes_stdin("timemachine"));
        assert!(slash_takes_stdin("timeline"));
        assert!(!slash_takes_stdin("memory"));
        assert!(!slash_takes_stdin("memory rust"));
        assert!(slash_takes_stdin("tools menu"));
        assert!(!slash_takes_stdin("tools list"));
        assert!(!slash_takes_stdin("help"));
        assert!(!slash_takes_stdin("custom-command arg"));
    }

    #[test]
    fn stdin_ownership_is_decided_per_argument_not_per_name() {
        // The freeze came from TWO tables disagreeing: `main.rs` matched only the bare NAME, so
        // `/tools menu` opened a dialoguer picker without suspending the retained frame, while this
        // table (the keyboard's copy) parked for it. `/memory` was the mirror image — main suspended,
        // the keyboard didn't. One argument-aware table now answers both.
        for line in [
            "timemachine",
            "timeline",
            "tm",
            "tools menu",
            "toolsets toggle",
            "effort",
            "update",
        ] {
            assert!(
                slash_takes_stdin(line),
                "/{line} opens a picker → must suspend"
            );
        }
        // Same command names WITHOUT the menu argument only print, so the box stays up.
        for line in ["tools", "tools list", "effort high", "memory", "mem rust"] {
            assert!(
                !slash_takes_stdin(line),
                "/{line} is pure-print → keep the sticky box"
            );
        }
    }

    #[test]
    fn keyboard_park_flag_tracks_suspend_and_resume() {
        // Drive the REAL entry points, not the flag. An earlier version of this test stored the
        // atomic by hand and passed while `resume()` did not clear it at all — which is the worst
        // possible bug here: the input thread stands down on the flag, so one stuck `true` wedges
        // the keyboard for the rest of the session. Off-TTY `suspend`/`resume` skip their retained
        // halves but still own this flag, so the edges are assertable in a unit test.
        let _g = TEST_CANCEL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(!keyboard_parked(), "idle: the keyboard owns stdin");
        suspend();
        assert!(
            keyboard_parked(),
            "suspend() must park the keyboard before a menu takes stdin"
        );
        resume("status");
        assert!(
            !keyboard_parked(),
            "resume() must hand the keyboard back, or input is dead"
        );
    }

    #[test]
    fn ctrl_l_reaches_the_redraw_binding_and_is_not_swallowed_as_a_control_char() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        // Ctrl-L is the recovery hatch for a frame corrupted by a print that bypassed the render
        // thread. It only works if the reader folds it to U+000C: the input loop's `Key::Char(c) if
        // c.is_control()` arm sits right below the binding and silently eats anything that doesn't
        // match the exact codepoint, so a wrong translation would fail *invisibly* — the key would
        // just do nothing, with no compile error and no panic to notice.
        let k = crossterm_to_console_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
        assert_eq!(
            k,
            Some(Key::Char('\u{c}')),
            "Ctrl-L must fold to U+000C or the redraw binding is unreachable"
        );
        // Upper-case Ctrl-Shift-L folds to the same control code (the reader upcases first), so the
        // hatch works regardless of caps/shift state.
        let up = crossterm_to_console_key(KeyEvent::new(
            KeyCode::Char('L'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert_eq!(up, Some(Key::Char('\u{c}')));
        // A bare `l` must stay a literal character — otherwise typing the letter would blank the
        // screen.
        assert_eq!(
            crossterm_to_console_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::NONE)),
            Some(Key::Char('l'))
        );
    }

    #[test]
    fn force_redraw_is_a_safe_noop_without_a_retained_session() {
        // The hatch is reachable from the input loop in every surface, including the plain REPL and
        // one-shots where no render thread exists. It must degrade to nothing rather than panic on
        // an absent runtime slot — a panic here would kill the process on a keystroke.
        assert!(
            !retained_running(),
            "unit tests own no terminal, so no retained session should be up"
        );
        force_redraw();
    }

    #[test]
    fn note_line_routes_out_of_band_warnings_without_panicking_off_tty() {
        // `note_line` is the funnel every deep-subsystem warning now goes through (dense fallback,
        // unreadable memory file, corrupt config, MCP connect). Those callers run on per-turn paths,
        // so this must be safe to call from anywhere: with no TUI it degrades to stderr.
        assert!(!active() && !retained_running());
        note_line("[test] out-of-band warning");
    }
}
