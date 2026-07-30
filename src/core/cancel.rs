//! Turn-scoped cooperative cancellation.
//!
//! Every top-level agent turn owns one token. Delegated task/workflow children inherit that token,
//! while unrelated turns get independent tokens so a late Esc or a parallel test cannot cancel them.

use std::cell::RefCell;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::Notify;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

struct Inner {
    id: u64,
    cancelled: AtomicBool,
    notify: Notify,
    /// Tokens derived via [`TurnCancel::child`]. Cancellation flows DOWN this edge only: Esc on the
    /// turn stops every descendant, while stopping one sub-agent leaves its parent and siblings
    /// running. `Weak` so a finished child is reclaimed instead of pinning memory for the whole turn.
    children: Mutex<Vec<Weak<Inner>>>,
}

fn new_inner(cancelled: bool) -> Arc<Inner> {
    Arc::new(Inner {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        cancelled: AtomicBool::new(cancelled),
        notify: Notify::new(),
        children: Mutex::new(Vec::new()),
    })
}

/// Cloneable cancellation token for one logical top-level turn and all of its children.
#[derive(Clone)]
pub struct TurnCancel(Arc<Inner>);

impl std::fmt::Debug for TurnCancel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TurnCancel")
            .field("id", &self.0.id)
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

impl Default for TurnCancel {
    fn default() -> Self {
        Self::new()
    }
}

impl TurnCancel {
    pub fn new() -> Self {
        Self(new_inner(false))
    }

    /// Derive a token that this one can cancel, but which cannot cancel this one.
    ///
    /// Needed because "stop" has two meanings that used to share a single token. Esc means *stop the
    /// turn*; `/workflows stop <id>` means *stop that one sub-agent and let the rest finish*. With one
    /// token per turn the second was impossible to express — cancelling a child cancelled the parent
    /// and every sibling with it. A child starts already-cancelled if its parent is, so a token handed
    /// out during a cancel can't slip through the check.
    pub fn child(&self) -> Self {
        let inner = new_inner(self.is_cancelled());
        let mut kids = self.0.children.lock().unwrap_or_else(|e| e.into_inner());
        // Reclaim finished children on the way past; the list is only ever appended to, so without
        // this a long turn spawning many sub-agents would grow it without bound.
        kids.retain(|w| w.strong_count() > 0);
        kids.push(Arc::downgrade(&inner));
        Self(inner)
    }

    pub fn cancel(&self) {
        if !self.0.cancelled.swap(true, Ordering::SeqCst) {
            self.0.notify.notify_waiters();
        }
        // Downward only: a cancelled parent stops everything it spawned. Collected under the lock and
        // released before recursing, so a deep tree can't deadlock against a concurrent `child()`.
        let kids: Vec<Arc<Inner>> = self
            .0
            .children
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for kid in kids {
            Self(kid).cancel();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::SeqCst)
    }

    /// Missed-wakeup-safe wait: create the notification future before re-checking the flag.
    pub async fn cancelled(&self) {
        loop {
            if self.is_cancelled() {
                return;
            }
            let notified = self.0.notify.notified();
            if self.is_cancelled() {
                return;
            }
            notified.await;
        }
    }

    pub fn same_turn(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

thread_local! {
    static CURRENT: RefCell<Vec<TurnCancel>> = const { RefCell::new(Vec::new()) };
}

/// Run synchronous tool code with its turn token available to nested sync→async bridges and pollers.
pub fn with_current<T>(token: TurnCancel, f: impl FnOnce() -> T) -> T {
    struct Pop;
    impl Drop for Pop {
        fn drop(&mut self) {
            CURRENT.with(|slot| {
                slot.borrow_mut().pop();
            });
        }
    }

    CURRENT.with(|slot| slot.borrow_mut().push(token));
    let _pop = Pop;
    f()
}

/// Token inherited by the currently executing synchronous tool body, if any.
pub fn current() -> Option<TurnCancel> {
    CURRENT.with(|slot| slot.borrow().last().cloned())
}

/// Race a future against a token, returning `None` when cancellation wins.
pub async fn race<T>(token: &TurnCancel, future: impl Future<Output = T>) -> Option<T> {
    tokio::select! {
        out = future => Some(out),
        _ = token.cancelled() => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tokens_are_independent_and_wait_is_missed_wakeup_safe() {
        let a = TurnCancel::new();
        let b = TurnCancel::new();
        a.cancel();
        a.cancelled().await;
        assert!(a.is_cancelled());
        assert!(!b.is_cancelled());
        assert!(!a.same_turn(&b));
    }

    #[test]
    fn stopping_one_child_leaves_the_turn_and_its_siblings_running() {
        // The whole point of `child()`: `/workflows stop #3` must end one sub-agent, not the turn.
        let turn = TurnCancel::new();
        let a = turn.child();
        let b = turn.child();

        a.cancel();

        assert!(a.is_cancelled(), "the targeted child stops");
        assert!(!b.is_cancelled(), "a sibling dispatch keeps running");
        assert!(!turn.is_cancelled(), "the orchestrating turn keeps running");
    }

    #[test]
    fn esc_on_the_turn_cascades_to_every_descendant() {
        // Esc is still all-or-nothing, now by cascade rather than by sharing one token — including
        // through a workflow parent to its grandchildren.
        let turn = TurnCancel::new();
        let workflow = turn.child();
        let grandchild = workflow.child();
        let sibling = turn.child();

        turn.cancel();

        for (name, t) in [
            ("workflow", &workflow),
            ("grandchild", &grandchild),
            ("sibling", &sibling),
        ] {
            assert!(t.is_cancelled(), "{name} must observe the turn's cancel");
        }
    }

    #[test]
    fn a_child_born_after_the_cancel_starts_cancelled() {
        // Otherwise a dispatch that raced the Esc would run on with a fresh, un-cancelled token.
        let turn = TurnCancel::new();
        turn.cancel();
        assert!(turn.child().is_cancelled());
    }

    #[test]
    fn finished_children_are_reclaimed_rather_than_accumulating() {
        // A long turn spawns many sub-agents; the parent's child list must not grow forever.
        let turn = TurnCancel::new();
        for _ in 0..64 {
            drop(turn.child());
        }
        let live = turn.child();
        let held = turn.0.children.lock().unwrap().len();
        assert!(
            held <= 2,
            "dropped children should be swept on the next child(); held {held}"
        );
        assert!(!live.is_cancelled());
    }

    #[test]
    fn scoped_current_restores_outer_token() {
        let outer = TurnCancel::new();
        let inner = TurnCancel::new();
        with_current(outer.clone(), || {
            assert!(current().unwrap().same_turn(&outer));
            with_current(inner.clone(), || {
                assert!(current().unwrap().same_turn(&inner))
            });
            assert!(current().unwrap().same_turn(&outer));
        });
        assert!(current().is_none());
    }
}
