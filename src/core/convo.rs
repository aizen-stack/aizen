//! Process-global "active conversation" marker.
//!
//! Exactly ONE agent turn runs at a time within a process: the REPL is serial, the `serve` daemon
//! handles inbound messages serially, and a turn's delegated sub-agents serialize on any tool that
//! declares `is_concurrency_safe() == false` (the browser tools do). So a single slot naming the
//! conversation currently being served is enough to scope per-conversation resources (e.g. the
//! browser session) WITHOUT threading an id through every `Tool::execute` signature. Set at each
//! turn boundary; read by resource registries that must not bleed state across conversations.

use std::sync::RwLock;

/// Stable identity of one logical conversation: a REPL session slug, or a `serve`
/// `platform:route:chat` triple. Cheap to clone; used as a `HashMap` key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConversationId(String);

impl ConversationId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ConversationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

static ACTIVE: RwLock<Option<ConversationId>> = RwLock::new(None);

/// Mark the conversation currently being served (called at each turn boundary). `None` reverts to
/// the shared default identity — right for a one-shot CLI invocation that has no persistent thread.
pub fn set_active(id: Option<ConversationId>) {
    *ACTIVE.write().unwrap_or_else(|e| e.into_inner()) = id;
}

/// The conversation currently being served, or a stable `"default"` identity when none is set (a
/// single-shot CLI run). Never panics on a poisoned lock — reads through the poison.
pub fn active() -> ConversationId {
    ACTIVE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
        .unwrap_or_else(|| ConversationId::new("default"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_defaults_then_tracks_set() {
        set_active(None);
        assert_eq!(active().as_str(), "default");
        set_active(Some(ConversationId::new("telegram:main:42")));
        assert_eq!(active().as_str(), "telegram:main:42");
        set_active(None);
        assert_eq!(active().as_str(), "default", "clearing reverts to the shared default");
    }
}
