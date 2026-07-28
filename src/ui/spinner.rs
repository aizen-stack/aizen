//! A tiny thread-based spinner for the "waiting on the model" gap (after you hit Enter, before the
//! first token streams). No new crate — a background thread redraws a braille frame via `console`,
//! and `stop()` (or drop) clears the line so the streamed output starts clean. TTY-only: on a pipe
//! / CI it's a silent no-op (never pollutes captured output).

use console::{style, Term};
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct Spinner {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Spinner {
    /// Start a spinner with `label` (e.g. "thinking"). No-op (returns an inert handle) when stdout
    /// isn't a TTY, so it never interferes with piped/CI output.
    pub fn start(label: &str) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        if !std::io::stdout().is_terminal() {
            return Self { stop, handle: None };
        }
        let label = label.to_string();
        let flag = stop.clone();
        let accent = crate::ui::splash::ACCENT;
        let handle = std::thread::spawn(move || {
            let term = Term::stdout();
            let mut i = 0usize;
            while !flag.load(Ordering::Relaxed) {
                let frame = FRAMES[i % FRAMES.len()];
                let _ = term.clear_line();
                let mut out = std::io::stdout();
                let _ = write!(
                    out,
                    "\r{} {}",
                    style(frame).color256(accent),
                    style(&label).dim()
                );
                let _ = out.flush();
                i += 1;
                // sleep ~90ms but wake within ~15ms of a stop() for a snappy first token
                let mut waited = 0u64;
                while waited < 90 && !flag.load(Ordering::Relaxed) {
                    std::thread::sleep(Duration::from_millis(15));
                    waited += 15;
                }
            }
            // leave the line blank + cursor at column 0 for the caller's output
            let _ = term.clear_line();
            let mut out = std::io::stdout();
            let _ = write!(out, "\r");
            let _ = out.flush();
        });
        Self {
            stop,
            handle: Some(handle),
        }
    }

    /// Stop the spinner and clear its line (idempotent; also runs on drop). Kept as an explicit
    /// public API for callers that want deterministic teardown rather than relying on `Drop`.
    #[allow(dead_code)]
    pub fn stop(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        if let Some(h) = self.handle.take() {
            self.stop.store(true, Ordering::Relaxed);
            let _ = h.join();
        }
    }
}

impl Drop for Spinner {
    fn drop(&mut self) {
        self.finish();
    }
}
