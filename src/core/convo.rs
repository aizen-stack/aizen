//! Which conversation the running turn belongs to.
//!
//! Per-conversation resources must not bleed between chats, so they key off [`active`]. Today the only
//! such resource is the browser session and its `@ref`s, which is behind `--features browser` — hence
//! the `cfg` gates below: in a default build nothing reads this, and an ungated `pub fn` would just be
//! dead code claiming to be API.
//!
//! The authoritative answer is the turn's [`crate::core::exec_ctx::ExecutionContext`], seeded into a
//! thread-local INSIDE the `spawn_blocking` closure that runs each tool body — that is the only value
//! that stays correct when the `serve` daemon runs one lane per `(bot, chat)` CONCURRENTLY.
//!
//! The process-global slot below is the fallback for paths that never build a context: the REPL and
//! one-shot CLI runs, where exactly one conversation exists per process anyway.

/// Stable identity of one logical conversation: a REPL session slug, or a `serve`
/// `platform:route:chat` triple. Cheap to clone; used as a `HashMap` key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConversationId(String);

impl ConversationId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    #[cfg(any(feature = "browser", test))]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(any(feature = "browser", test))]
static ACTIVE: std::sync::RwLock<Option<ConversationId>> = std::sync::RwLock::new(None);

/// Mark the conversation being served on paths that don't build an `ExecutionContext`.
///
/// NOT for the concurrent `serve` lanes: a process-global written by several lanes would name
/// whichever turn started last. Those pass the id on their turn's context instead.
#[cfg(any(feature = "browser", test))]
pub fn set_active(id: Option<ConversationId>) {
    *ACTIVE.write().unwrap_or_else(|e| e.into_inner()) = id;
}

/// The conversation this turn serves — the turn's own context when one is pinned (authoritative
/// inside a tool body, and the only correct answer under concurrent lanes), else the process-global
/// slot, else a stable `"default"`. Never panics on a poisoned lock.
#[cfg(any(feature = "browser", test))]
pub fn active() -> ConversationId {
    if let Some(ctx) = crate::core::exec_ctx::current() {
        return ctx.conversation();
    }
    ACTIVE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| ConversationId::new("default"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::exec_ctx::{self, ExecutionContext};

    #[test]
    fn active_defaults_then_tracks_set() {
        set_active(None);
        assert_eq!(active().as_str(), "default");
        set_active(Some(ConversationId::new("telegram:main:42")));
        assert_eq!(active().as_str(), "telegram:main:42");
        set_active(None);
        assert_eq!(
            active().as_str(),
            "default",
            "clearing reverts to the shared default"
        );
    }

    #[test]
    fn a_pinned_turn_context_wins_over_the_process_global() {
        // The concurrency guarantee: a tool body running for lane B must see lane B's conversation
        // even though lane A wrote the process-global slot most recently.
        set_active(Some(ConversationId::new("telegram:laneA:1")));
        exec_ctx::with_current(
            ExecutionContext::new(ConversationId::new("telegram:laneB:2")),
            || {
                assert_eq!(
                    active().as_str(),
                    "telegram:laneB:2",
                    "the running turn's own context is authoritative"
                );
            },
        );
        assert_eq!(
            active().as_str(),
            "telegram:laneA:1",
            "outside the turn, the global slot answers again"
        );
        set_active(None);
    }
}
