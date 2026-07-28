//! Mid-turn STEERING mailbox — "điều hướng phiên làm việc".
//!
//! The submission queue answers "run this AFTER the current turn". Steering answers the other
//! question: the user thinks of something *while* the agent works and wants it folded into the run
//! in flight, without Esc-ing and starting over (which throws away every tool result gathered so
//! far, and the provider prompt cache with it).
//!
//! Shape mirrors [`crate::core::cancel`]'s armed-slot design, deliberately:
//!
//! * The interactive REPL [`arm`]s the mailbox around a top-level turn and [`disarm`]s after,
//!   handing back anything the agent never picked up so the REPL can re-queue it as a normal
//!   submission (a steer typed one millisecond before the turn ended must not vanish).
//! * The keyboard thread [`push`]es while `is_armed()`; a failed push means "no live turn" and the
//!   caller falls back to the ordinary queue.
//! * The agent loop [`drain`]s at each iteration boundary — the only place history is guaranteed
//!   coherent (no dangling `tool_calls` awaiting results), so an injected `user` message can never
//!   strand the conversation shape a strict gateway 400s on.
//!
//! Process-global rather than threaded through `AgentConfig` for the same reason `cancel` was
//! before it: the keyboard thread has no handle on the running turn. Exactly one top-level turn
//! runs at a time (see [`crate::core::convo`]), so one slot is enough — and delegated sub-agents
//! opt OUT via `AgentConfig::enable_steering`, since a steer is aimed at the top-level task, not
//! at whatever a child happens to be doing.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;

/// A live top-level turn is accepting steers.
static ARMED: AtomicBool = AtomicBool::new(false);
/// Count of un-drained steers, read by the input box painter (a lock-free peek — the render path
/// must never block on the mailbox mutex).
static PENDING: AtomicUsize = AtomicUsize::new(0);
static MAILBOX: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Cap the backlog: a stuck turn (long tool call) plus an enthusiastic typist shouldn't be able to
/// grow an unbounded prompt injection. Beyond this, [`push`] refuses and the caller queues instead.
pub const MAX_PENDING: usize = 8;
/// Per-steer length cap. A steer is a course correction, not a document — anything larger is
/// almost certainly a paste that belongs in its own turn.
pub const MAX_CHARS: usize = 4000;

fn mailbox() -> std::sync::MutexGuard<'static, Vec<String>> {
    MAILBOX.lock().unwrap_or_else(|e| e.into_inner())
}

/// Open the mailbox for one top-level turn. Clears any stale content (a previous turn's leftovers
/// are re-queued by [`disarm`], so anything still here would be a double-delivery).
pub fn arm() {
    mailbox().clear();
    PENDING.store(0, Ordering::SeqCst);
    ARMED.store(true, Ordering::SeqCst);
}

/// Close the mailbox and return whatever the agent never consumed, so the caller can re-queue it as
/// an ordinary submission. Ordering matters: flip `ARMED` first so a concurrent [`push`] either
/// landed before the drain (and comes back here) or is refused and queued by its own caller —
/// never accepted into a mailbox nobody will read again.
pub fn disarm() -> Vec<String> {
    ARMED.store(false, Ordering::SeqCst);
    let leftover = std::mem::take(&mut *mailbox());
    PENDING.store(0, Ordering::SeqCst);
    leftover
}

/// Is a live turn accepting steers?
pub fn is_armed() -> bool {
    ARMED.load(Ordering::SeqCst)
}

/// Un-drained steer count (for the input-box indicator).
pub fn pending() -> usize {
    PENDING.load(Ordering::SeqCst)
}

/// Offer a steer to the running turn. Returns `false` — caller should fall back to the normal
/// submission queue — when no turn is live, the text is blank, it exceeds [`MAX_CHARS`], or the
/// backlog is already [`MAX_PENDING`] deep.
pub fn push(text: &str) -> bool {
    let trimmed = text.trim();
    if !is_armed() || trimmed.is_empty() || trimmed.chars().count() > MAX_CHARS {
        return false;
    }
    let mut box_ = mailbox();
    if box_.len() >= MAX_PENDING {
        return false;
    }
    box_.push(trimmed.to_string());
    PENDING.store(box_.len(), Ordering::SeqCst);
    true
}

/// Take everything pending (called by the agent loop at an iteration boundary). Empty vec = nothing
/// to fold in, which is the common case — this is a cheap atomic read when the mailbox is quiet.
pub fn drain() -> Vec<String> {
    if PENDING.load(Ordering::SeqCst) == 0 {
        return Vec::new();
    }
    let out = std::mem::take(&mut *mailbox());
    PENDING.store(0, Ordering::SeqCst);
    out
}

/// Discard pending steers without delivering them. Esc means "stop everything", including course
/// corrections aimed at the turn being stopped.
pub fn clear() {
    mailbox().clear();
    PENDING.store(0, Ordering::SeqCst);
}

/// Render the drained steers as ONE `user` message. A single message (rather than one per steer)
/// keeps the injected history compact and reads as a coherent interruption; the prefix is what the
/// loop and the tests key off, mirroring the other hard-block gates (`[todo-poke]`, `[goal]`).
pub fn format_injection(steers: &[String]) -> String {
    let body = if steers.len() == 1 {
        steers[0].clone()
    } else {
        steers
            .iter()
            .map(|s| format!("- {s}"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    format!(
        "{PREFIX} The user sent this WHILE you were working — read it as a course correction to the \
         task in flight, not a new request:\n\n{body}\n\n\
         Fold it into what you are already doing. If it changes the plan, adjust the plan (and the \
         todo list) before continuing; if it contradicts an earlier instruction, the newer message \
         wins. Do not restart work you have already completed."
    )
}

/// Marker prefix for a steering injection.
pub const PREFIX: &str = "[user-steering]";

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex as StdMutex, OnceLock};

    /// The mailbox is process-global, so these tests must not interleave.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| StdMutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    fn reset() {
        ARMED.store(false, Ordering::SeqCst);
        clear();
    }

    #[test]
    fn push_is_refused_until_armed() {
        let _g = guard();
        reset();
        assert!(
            !push("do it differently"),
            "no live turn → caller must queue instead"
        );
        assert_eq!(pending(), 0);
        arm();
        assert!(push("do it differently"));
        assert_eq!(pending(), 1);
        reset();
    }

    #[test]
    fn drain_hands_over_once_and_resets_the_counter() {
        let _g = guard();
        reset();
        arm();
        assert!(push("first"));
        assert!(push("second"));
        let got = drain();
        assert_eq!(
            got,
            vec!["first".to_string(), "second".to_string()],
            "FIFO order preserved"
        );
        assert_eq!(pending(), 0);
        assert!(
            drain().is_empty(),
            "a second drain sees nothing — no double delivery"
        );
        reset();
    }

    #[test]
    fn disarm_returns_leftovers_for_requeue_and_closes_the_slot() {
        let _g = guard();
        reset();
        arm();
        assert!(push("typed just as the turn ended"));
        let leftover = disarm();
        assert_eq!(leftover, vec!["typed just as the turn ended".to_string()]);
        assert!(!is_armed());
        assert_eq!(pending(), 0);
        assert!(
            !push("too late"),
            "a closed mailbox refuses, so the REPL queues it"
        );
        reset();
    }

    #[test]
    fn blank_oversized_and_overflowing_pushes_are_refused() {
        let _g = guard();
        reset();
        arm();
        assert!(!push("   \n  "), "blank steer is not a course correction");
        let huge = "x".repeat(MAX_CHARS + 1);
        assert!(!push(&huge), "oversized paste belongs in its own turn");
        for i in 0..MAX_PENDING {
            assert!(
                push(&format!("steer {i}")),
                "backlog up to the cap is accepted"
            );
        }
        assert!(
            !push("one too many"),
            "past the cap the caller falls back to the queue"
        );
        assert_eq!(pending(), MAX_PENDING);
        reset();
    }

    #[test]
    fn clear_discards_without_delivering() {
        let _g = guard();
        reset();
        arm();
        assert!(push("never mind"));
        clear();
        assert_eq!(pending(), 0);
        assert!(drain().is_empty());
        assert!(
            is_armed(),
            "Esc clears the backlog but the turn slot stays as the REPL left it"
        );
        reset();
    }

    #[test]
    fn injection_text_is_prefixed_and_lists_every_steer() {
        let steers = vec![
            "also update the README".to_string(),
            "skip the benchmark".to_string(),
        ];
        let msg = format_injection(&steers);
        assert!(
            msg.starts_with(PREFIX),
            "prefix is what the loop + gates key off"
        );
        assert!(msg.contains("also update the README"));
        assert!(msg.contains("skip the benchmark"));
        assert!(
            msg.contains("- also update"),
            "multiple steers render as a list"
        );
        // Single steer stays inline (no stray bullet for a one-line correction).
        let one = format_injection(&["just one".to_string()]);
        assert!(one.contains("just one") && !one.contains("- just one"));
    }
}
