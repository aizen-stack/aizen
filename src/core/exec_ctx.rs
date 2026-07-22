//! Turn-scoped execution context — the explicit carrier of per-turn/per-conversation identity that
//! a tool body needs, threaded across the `spawn_blocking` boundary the way [`crate::core::cancel`]
//! already threads the cancellation token.
//!
//! # Why this exists
//!
//! Tool bodies run on a `spawn_blocking` worker thread (`src/agent/mod.rs`), so any per-turn fact a
//! tool needs must either be captured into the `move` closure or seeded into a thread-local INSIDE
//! that closure — a plain process-global read gives whatever the driver thread last set, which bleeds
//! across turns the moment two turns interleave. Today exactly one turn runs at a time per process
//! (the REPL is serial; the `serve` daemon handles messages serially), so the bleed is latent rather
//! than live — but the browser tools already key their per-conversation session registry off
//! [`crate::core::convo::active()`], so the conversation id is the one piece of context a tool body
//! genuinely reads across the hop. This module makes that read authoritative: the loop pins an
//! `ExecutionContext` for the turn, seeds it into the thread-local inside the same closure that seeds
//! the cancel token, and [`crate::core::convo::active()`] prefers it over the process-global slot.
//!
//! # Migration scaffold (plan P1-A)
//!
//! The context currently carries only the conversation id — the sole cross-hop reader today. Persona
//! override, per-turn reasoning-effort, and the Telegram approval route are also per-turn state, but
//! they are read on the DRIVER thread that sets them (not inside a tool body), so moving them here is
//! defense-in-depth against future turn interleaving rather than a live fix. They migrate onto this
//! struct field-by-field: add the field, seed it at the turn boundary, and switch the corresponding
//! accessor to prefer [`current`] before its process-global — never a big-bang cutover.

use crate::core::convo::ConversationId;
use std::cell::RefCell;
use std::sync::Arc;

/// Immutable per-turn identity. Cheap to clone (one `Arc` bump); shared by a top-level turn and every
/// delegated sub-agent that inherits it.
#[derive(Debug)]
struct Inner {
    /// The conversation this turn serves — a REPL session slug, or a `serve`
    /// `platform:route:chat` triple. Scopes per-conversation resources (the browser session).
    conversation: ConversationId,
}

/// Cloneable handle to one turn's execution context. Threaded through [`crate::agent::AgentConfig`]
/// and seeded into a thread-local inside the `spawn_blocking` closure so tool bodies read it.
#[derive(Debug, Clone)]
pub struct ExecutionContext(Arc<Inner>);

impl ExecutionContext {
    /// Build a context for a turn serving `conversation`.
    pub fn new(conversation: ConversationId) -> Self {
        Self(Arc::new(Inner { conversation }))
    }

    /// The conversation this turn serves.
    pub fn conversation(&self) -> ConversationId {
        self.0.conversation.clone()
    }
}

impl Default for ExecutionContext {
    /// The shared `"default"` identity — right for a one-shot CLI run or any turn with no persistent
    /// conversation thread. Matches the fallback [`crate::core::convo::active`] uses.
    fn default() -> Self {
        Self::new(ConversationId::new("default"))
    }
}

thread_local! {
    /// Stack of contexts active on THIS thread. A stack (not a single slot) so a nested
    /// `with_current` — e.g. a tool that re-enters the agent loop — restores the outer context on
    /// exit, exactly like [`crate::core::cancel`]'s token stack.
    static CURRENT: RefCell<Vec<ExecutionContext>> = const { RefCell::new(Vec::new()) };
}

/// Run synchronous tool code with `ctx` as the active per-turn context, restored on return (even on
/// panic, via the `Drop` guard). Must wrap the tool body INSIDE the `spawn_blocking` closure — a
/// context pushed on the driver thread is invisible on the worker thread the body actually runs on.
pub fn with_current<T>(ctx: ExecutionContext, f: impl FnOnce() -> T) -> T {
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            CURRENT.with(|slot| {
                slot.borrow_mut().pop();
            });
        }
    }

    CURRENT.with(|slot| slot.borrow_mut().push(ctx));
    let _pop = Pop;
    f()
}

/// The context of the currently executing synchronous tool body, if any. `None` on a thread with no
/// pinned turn (e.g. a caller reading before any `with_current` wrap) — callers fall back to their
/// process-global slot.
pub fn current() -> Option<ExecutionContext> {
    CURRENT.with(|slot| slot.borrow().last().cloned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_the_shared_identity() {
        assert_eq!(ExecutionContext::default().conversation().as_str(), "default");
    }

    #[test]
    fn current_is_none_without_a_pinned_turn() {
        assert!(current().is_none(), "no context pinned on a bare thread");
    }

    #[test]
    fn with_current_pins_and_restores() {
        let outer = ExecutionContext::new(ConversationId::new("repl:main"));
        let inner = ExecutionContext::new(ConversationId::new("telegram:main:42"));
        with_current(outer.clone(), || {
            assert_eq!(current().unwrap().conversation().as_str(), "repl:main");
            with_current(inner.clone(), || {
                assert_eq!(current().unwrap().conversation().as_str(), "telegram:main:42");
            });
            assert_eq!(
                current().unwrap().conversation().as_str(),
                "repl:main",
                "inner context popped on exit, outer restored"
            );
        });
        assert!(current().is_none(), "outer context popped after the outermost scope");
    }
}
