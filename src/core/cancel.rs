//! Turn-scoped cooperative cancellation.
//!
//! Every top-level agent turn owns one token. Delegated task/workflow children inherit that token,
//! while unrelated turns get independent tokens so a late Esc or a parallel test cannot cancel them.

use std::cell::RefCell;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::Notify;

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

struct Inner {
    id: u64,
    cancelled: AtomicBool,
    notify: Notify,
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
        Self(Arc::new(Inner {
            id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
        }))
    }

    pub fn cancel(&self) {
        if !self.0.cancelled.swap(true, Ordering::SeqCst) {
            self.0.notify.notify_waiters();
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
