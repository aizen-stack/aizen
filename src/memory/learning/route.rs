//! Confidence-gate routing. A candidate that survives the threat-scan is sent to one
//! of four destinations by confidence + whether it's a style fact:
//!
//! - `Drop`        — below the floor; not worth persisting.
//! - `Review`      — mid-confidence; lands in the review queue for a human (`ng memory review`).
//! - `Store`       — high-confidence; written to the live long-tail store automatically.
//! - `CorePromote` — a high-confidence STYLE fact; eligible for the always-on core,
//!                   but ONLY after confirmation (auto-learned facts never silently
//!                   enter the always-injected prompt).

use crate::config::MemorySettings;
use crate::memory::learning::extract_free::Candidate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    Drop,
    Review,
    Store,
    CorePromote,
}

pub fn route(c: &Candidate, s: &MemorySettings) -> Route {
    if c.confidence < s.learn_min_confidence {
        return Route::Drop;
    }
    if c.is_style && c.confidence >= s.learn_core_confidence {
        return Route::CorePromote;
    }
    if c.confidence >= s.learn_store_confidence {
        Route::Store
    } else {
        Route::Review
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::learning::extract_free::{CandidateKind, Polarity};
    use crate::memory::store::MemoryType;

    fn cand(conf: f64, is_style: bool) -> Candidate {
        Candidate {
            text: "x".into(),
            name: "x".into(),
            kind: CandidateKind::Preference,
            mtype: MemoryType::User,
            is_style,
            polarity: Polarity::Affirm,
            confidence: conf,
        }
    }

    #[test]
    fn routes_by_confidence() {
        let s = MemorySettings::default();
        assert_eq!(route(&cand(0.3, false), &s), Route::Drop);
        assert_eq!(route(&cand(0.6, false), &s), Route::Review);
        assert_eq!(route(&cand(0.8, false), &s), Route::Store);
        assert_eq!(route(&cand(0.9, true), &s), Route::CorePromote);
        // a style fact below the core threshold is still just a normal store
        assert_eq!(route(&cand(0.8, true), &s), Route::Store);
    }
}
