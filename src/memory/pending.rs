//! The pending-recall ledger: which facts were injected into the CURRENT turn, under which
//! short handles.
//!
//! **RAM only.** Nothing here touches disk, and nothing here is a schema field. A recall block is
//! an ephemeral property of one turn, so persisting it would be storing a cache of a derived value —
//! and a stale one, since the same query in a different directory selects different facts.
//!
//! It exists for two jobs:
//!
//! 1. **Handles.** The block labels facts `[m1]`, `[m2]`, … instead of printing real ids. Cheaper,
//!    and it means the model can only refer to something it was actually shown — it cannot invent
//!    an id for a fact it never saw, because it never learns the id namespace.
//! 2. **Delta-injection.** The ledger remembers what the previous turn injected, so an unchanged
//!    selection is not re-folded. The model already has those lines in the transcript; repeating
//!    them costs tokens every turn and, worse, puts the same claim in the context twice with
//!    nothing to say which copy is newer.
//!
//! Cleared at every turn boundary AND at [`crate::reset_per_session_state`] — a restored or
//! `/clear`ed thread must not inherit handles pointing at a selection its transcript never saw.

use std::sync::Mutex;

/// One injected fact: the handle the model sees, and the store id it stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub handle: String,
    pub id: String,
}

/// The live ledger for this turn plus the previous turn's selection (for the delta check).
#[derive(Debug, Default)]
struct Ledger {
    /// Monotonic turn counter — only used to make "was this the immediately-previous turn?" cheap.
    turn_seq: u64,
    /// What the current turn injected.
    current: Vec<Pending>,
    /// The ids the last block carried, in selection order.
    last_ids: Vec<String>,
}

fn ledger() -> &'static Mutex<Ledger> {
    static L: Mutex<Ledger> = Mutex::new(Ledger {
        turn_seq: 0,
        current: Vec::new(),
        last_ids: Vec::new(),
    });
    &L
}

/// Record the selection this turn is about to inject, and bump the turn counter.
pub fn open_turn(pairs: Vec<Pending>) {
    if let Ok(mut l) = ledger().lock() {
        l.turn_seq = l.turn_seq.wrapping_add(1);
        l.last_ids = pairs.iter().map(|p| p.id.clone()).collect();
        l.current = pairs;
    }
}

/// Would a block carrying exactly `ids` repeat what the last one already said?
///
/// Order-insensitive: the same facts re-ranked are still the same facts, and re-injecting them
/// because BM25 shuffled two near-ties would defeat the whole delta check.
pub fn is_same_as_last(ids: &[String]) -> bool {
    match ledger().lock() {
        Ok(l) => {
            if l.last_ids.len() != ids.len() || ids.is_empty() {
                return false;
            }
            let mut a: Vec<&String> = l.last_ids.iter().collect();
            let mut b: Vec<&String> = ids.iter().collect();
            a.sort();
            b.sort();
            a == b
        }
        Err(_) => false,
    }
}

/// Resolve the handles a model claimed to have used back into store ids.
///
/// Unknown handles are dropped silently rather than reported: the only way to produce one is to
/// hallucinate it, and a fabricated handle is not evidence of anything worth acting on. Phase 3
/// feeds the result to `confirmations += 1`.
pub fn resolve_used(handles: &[String]) -> Vec<String> {
    let Ok(l) = ledger().lock() else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for h in handles {
        let want = h.trim().trim_start_matches('[').trim_end_matches(']');
        if let Some(p) = l
            .current
            .iter()
            .find(|p| p.handle.eq_ignore_ascii_case(want))
        {
            if !out.contains(&p.id) {
                out.push(p.id.clone());
            }
        }
    }
    out
}

/// What the current turn injected (handle → id), for inspection/tests.
pub fn current() -> Vec<Pending> {
    ledger()
        .lock()
        .map(|l| l.current.clone())
        .unwrap_or_default()
}

/// Forget everything. Called on thread switch (`/clear`, `/resume`, `/handoff`, `/recover`): the
/// new transcript never contained the old block, so its handles now point at nothing the model
/// can see.
pub fn clear() {
    if let Ok(mut l) = ledger().lock() {
        l.current.clear();
        l.last_ids.clear();
        l.turn_seq = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(handle: &str, id: &str) -> Pending {
        Pending {
            handle: handle.into(),
            id: id.into(),
        }
    }

    #[test]
    fn resolve_used_maps_handles_and_ignores_invented_ones() {
        clear();
        open_turn(vec![p("m1", "prefers-pnpm"), p("m2", "windows-sys-pinned")]);
        // Tolerates the bracketed spelling the block itself uses.
        assert_eq!(
            resolve_used(&["m1".into()]),
            vec!["prefers-pnpm".to_string()]
        );
        assert_eq!(
            resolve_used(&["[m2]".into()]),
            vec!["windows-sys-pinned".to_string()]
        );
        // A handle that was never injected resolves to nothing — a model cannot confirm a fact it
        // was not shown.
        assert!(resolve_used(&["m9".into()]).is_empty());
        // Duplicates collapse: claiming the same handle twice is still one confirmation.
        assert_eq!(resolve_used(&["m1".into(), "m1".into()]).len(), 1);
        clear();
    }

    #[test]
    fn delta_is_order_insensitive_and_empty_never_matches() {
        clear();
        // Nothing injected yet → nothing to repeat.
        assert!(!is_same_as_last(&["a".into()]));

        open_turn(vec![p("m1", "a"), p("m2", "b")]);
        assert!(
            is_same_as_last(&["a".into(), "b".into()]),
            "same set → skip re-injection"
        );
        assert!(
            is_same_as_last(&["b".into(), "a".into()]),
            "a re-rank is not a new selection"
        );
        assert!(
            !is_same_as_last(&["a".into()]),
            "a narrower set is a real change"
        );
        assert!(
            !is_same_as_last(&["a".into(), "c".into()]),
            "a different fact is a real change"
        );
        // An empty selection is never "the same": there is no block to have already said it.
        assert!(!is_same_as_last(&[]));
        clear();
    }

    #[test]
    fn clear_forgets_handles_so_a_restored_thread_cannot_confirm_stale_ids() {
        clear();
        open_turn(vec![p("m1", "a")]);
        assert!(!resolve_used(&["m1".into()]).is_empty());
        clear();
        assert!(
            resolve_used(&["m1".into()]).is_empty(),
            "handles must not survive a thread switch"
        );
        assert!(current().is_empty());
    }
}
