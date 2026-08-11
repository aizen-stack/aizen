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
//! # What lives here, and what deliberately does not
//!
//! The context carries per-turn facts whose reader would otherwise have to consult a process-global:
//! the conversation id (read by the browser session registry inside a tool body) and the approval
//! route (read by the approval gate on the driver). Both are read far from where they are set.
//!
//! Two other per-turn facts are deliberately NOT here, because they already have a direct owner and a
//! second copy would just be a second source of truth to keep in sync:
//!   * the workspace root → `AgentConfig::workspace_root`, read via `AgentConfig::effective_root`;
//!   * the reasoning-effort tier → passed explicitly to `client::chat_with_tools_effort`.
//!
//! How to READ each field, which differs by where the code runs:
//!   * inside a tool body (`spawn_blocking` worker) → [`current`], seeded by the executor;
//!   * on the driver (the approval gate) → through `AgentConfig::exec_ctx`, because the driver never
//!     pushes itself onto the thread-local stack.
//!
//! Every field is `Option`: `None` means "no per-turn override — fall back to the process-global /
//! config value", which is exactly what the REPL and one-shot CLI paths want.

use crate::core::convo::ConversationId;
use std::cell::RefCell;
use std::sync::Arc;

/// Where a destructive-op approval prompt for THIS turn should be delivered: the hosted-bot route
/// (sub-bot name) and the platform chat id, rendered as a string so this stays platform-agnostic
/// (Telegram `i64`, Discord `u64`). The Telegram platform maps `route` back to that bot's token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRoute {
    pub route: String,
    pub chat: String,
}

/// Immutable per-turn identity. Cheap to clone (one `Arc` bump); shared by a top-level turn and every
/// delegated sub-agent that inherits it.
#[derive(Debug)]
struct Inner {
    /// The conversation this turn serves — a REPL session slug, or a `serve`
    /// `platform:route:chat` triple. Scopes per-conversation resources (the browser session).
    conversation: Option<ConversationId>,
    /// One concrete agent/tool-execution scope within a conversation. Top-level turns inherit the
    /// conversation id; each delegated child derives its own id so stateful tools such as `process`
    /// cannot list/read/kill a sibling's handles.
    resource_scope: Option<String>,
    /// Whether tool bodies may emit progress lines into the parent transcript. Delegated children run
    /// quiet and publish progress through the orchestration board instead.
    trace_visible: bool,
    /// Persona this turn speaks as, layered over the global `config.persona`. `None` ⇒ the global one.
    persona: Option<String>,
    /// Where an approval prompt goes this turn. `None` ⇒ the platform's process-global fallback.
    approval_route: Option<ApprovalRoute>,
}

impl Default for Inner {
    fn default() -> Self {
        Self {
            conversation: None,
            resource_scope: None,
            trace_visible: true,
            persona: None,
            approval_route: None,
        }
    }
}

/// Cloneable handle to one turn's execution context. Threaded through [`crate::agent::AgentConfig`]
/// and seeded into a thread-local inside the `spawn_blocking` closure so tool bodies read it.
#[derive(Debug, Clone)]
pub struct ExecutionContext(Arc<Inner>);

impl ExecutionContext {
    /// Build a context for a turn serving `conversation`.
    pub fn new(conversation: ConversationId) -> Self {
        Self(Arc::new(Inner {
            resource_scope: Some(conversation.to_string()),
            conversation: Some(conversation),
            trace_visible: true,
            ..Inner::default()
        }))
    }

    /// The conversation this turn serves. Read by `convo::active()` inside tool bodies.
    #[cfg_attr(not(any(feature = "browser", test)), allow(dead_code))]
    pub fn conversation(&self) -> ConversationId {
        self.0
            .conversation
            .clone()
            .unwrap_or_else(|| ConversationId::new("default"))
    }

    // ── per-turn overrides ────────────────────────────────────────────────────────────
    //
    // Builders take `self` and return a new `Arc` rather than mutating: the context is shared with
    // already-spawned tool bodies, so an in-place edit would retroactively change a running turn's
    // view. Chain them at the turn boundary, before the context is handed to `AgentConfig`.

    fn with(&self, f: impl FnOnce(&mut Inner)) -> Self {
        let mut next = Inner {
            conversation: self.0.conversation.clone(),
            resource_scope: self.0.resource_scope.clone(),
            trace_visible: self.0.trace_visible,
            persona: self.0.persona.clone(),
            approval_route: self.0.approval_route.clone(),
        };
        f(&mut next);
        Self(Arc::new(next))
    }

    /// Give a delegated child an isolated resource namespace. The caller supplies a stable label for
    /// the dispatch lifetime; no process-global counter or provider-visible value is required.
    pub fn with_resource_scope(&self, scope: impl Into<String>) -> Self {
        let scope = scope.into();
        self.with(|i| i.resource_scope = Some(scope))
    }
    /// Resource namespace visible to stateful tool bodies.
    pub fn resource_scope(&self) -> String {
        self.0
            .resource_scope
            .clone()
            .or_else(|| self.0.conversation.as_ref().map(ToString::to_string))
            .unwrap_or_else(|| "default".to_string())
    }

    /// Control whether a tool body may append progress to the parent transcript.
    pub fn with_trace_visible(&self, visible: bool) -> Self {
        self.with(|i| i.trace_visible = visible)
    }
    pub fn trace_visible(&self) -> bool {
        self.0.trace_visible
    }

    /// Pin the persona this turn speaks as (`None` ⇒ fall back to the global `config.persona`).
    pub fn with_persona(&self, persona: Option<String>) -> Self {
        self.with(|i| i.persona = persona)
    }
    /// The turn's persona override, if any.
    pub fn persona(&self) -> Option<String> {
        self.0.persona.clone()
    }

    /// Pin where an approval prompt for this turn is delivered.
    pub fn with_approval_route(&self, route: Option<ApprovalRoute>) -> Self {
        self.with(|i| i.approval_route = route)
    }
    /// The turn's approval route, if any.
    pub fn approval_route(&self) -> Option<ApprovalRoute> {
        self.0.approval_route.clone()
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
        assert_eq!(
            ExecutionContext::default().conversation().as_str(),
            "default"
        );
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
                assert_eq!(
                    current().unwrap().conversation().as_str(),
                    "telegram:main:42"
                );
            });
            assert_eq!(
                current().unwrap().conversation().as_str(),
                "repl:main",
                "inner context popped on exit, outer restored"
            );
        });
        assert!(
            current().is_none(),
            "outer context popped after the outermost scope"
        );
    }

    #[test]
    fn a_fresh_context_carries_no_overrides() {
        // Every per-turn field defaults to "no override" so the REPL / one-shot CLI keeps reading
        // its process-global exactly as before. This is what makes P0 a no-behavior-change change.
        let ctx = ExecutionContext::new(ConversationId::new("repl:main"));
        assert_eq!(ctx.persona(), None);
        assert_eq!(ctx.approval_route(), None);
        assert_eq!(ctx.resource_scope(), "repl:main");
        assert!(ctx.trace_visible());
    }

    #[test]
    fn delegated_scope_and_trace_visibility_are_isolated() {
        let parent = ExecutionContext::new(ConversationId::new("repl:main"));
        let child = parent
            .with_resource_scope("repl:main/task/7")
            .with_trace_visible(false);
        assert_eq!(parent.resource_scope(), "repl:main");
        assert!(parent.trace_visible());
        assert_eq!(child.resource_scope(), "repl:main/task/7");
        assert!(!child.trace_visible());
        assert_eq!(child.conversation().as_str(), "repl:main");
    }

    #[test]
    fn each_override_round_trips_and_leaves_the_others_alone() {
        let base = ExecutionContext::new(ConversationId::new("telegram:work:7"));
        let ctx = base
            .with_persona(Some("Aria".into()))
            .with_approval_route(Some(ApprovalRoute {
                route: "work".into(),
                chat: "7".into(),
            }));

        assert_eq!(ctx.persona().as_deref(), Some("Aria"));
        assert_eq!(ctx.approval_route().unwrap().route, "work");
        assert_eq!(ctx.approval_route().unwrap().chat, "7");
        // Chaining must not drop the identity the whole context is keyed on.
        assert_eq!(ctx.conversation().as_str(), "telegram:work:7");
    }

    #[test]
    fn a_builder_does_not_mutate_the_context_already_handed_out() {
        // The context is shared with tool bodies that may already be running. A builder returns a
        // NEW Arc, so a later override can't retroactively change a turn in flight.
        let original = ExecutionContext::new(ConversationId::new("repl:main"));
        let derived = original.with_persona(Some("Aria".into()));
        assert_eq!(original.persona(), None, "original is untouched");
        assert_eq!(derived.persona().as_deref(), Some("Aria"));
    }

    #[test]
    fn two_lane_contexts_stay_independent_on_one_thread() {
        // The concurrency case this whole module exists for: two hosted-bot lanes must never read
        // each other's approval route, or one bot's ✓/✗ prompt lands in the other bot's chat.
        let a = ExecutionContext::new(ConversationId::new("telegram:default:1"))
            .with_approval_route(Some(ApprovalRoute {
                route: "default".into(),
                chat: "1".into(),
            }));
        let b = ExecutionContext::new(ConversationId::new("telegram:work:2")).with_approval_route(
            Some(ApprovalRoute {
                route: "work".into(),
                chat: "2".into(),
            }),
        );
        with_current(a.clone(), || {
            assert_eq!(
                current().unwrap().approval_route().unwrap().route,
                "default"
            );
            with_current(b.clone(), || {
                assert_eq!(current().unwrap().approval_route().unwrap().route, "work");
            });
            assert_eq!(
                current().unwrap().approval_route().unwrap().route,
                "default",
                "lane A's route is restored when lane B's scope ends"
            );
        });
    }
}
