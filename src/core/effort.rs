//! Per-turn reasoning-effort auto-detection (Claude-CLI-style), PURE + unit-testable.
//!
//! Each user turn is classified into `low`/`medium`/`high` from (a) explicit "think"-family /
//! "keep it light" KEYWORDS the user typed, and (b) a COMPLEXITY heuristic when no keyword fires.
//! Mirrors aizen's established "override-first, then heuristic, unknown ⇒ safe default" idiom
//! (`agent::prompt_tier_for`) and reuses the shared word-boundary matcher
//! (`crate::llm::client::contains_word`) so single tokens can't false-positive on substrings
//! (`adjust` must NOT trigger `just`).
//!
//! The classifier is PURE (`&str` in, `Option<Effort>` out). `None` means "no opinion" — the
//! caller defers to the configured `reasoning_effort` (or omits the field entirely, keeping the
//! request byte-identical for users who never set one). The config load / override plumbing lives
//! in `cli_config` and the REPL; nothing here reads globals.

use crate::llm::client::contains_word;

/// Provider-agnostic effort tier, matching the `reasoning_effort` wire strings. Five rungs, mirroring
/// the modern Claude effort scale (`low`/`medium`/`high`/`xhigh`/`max`): `high` is the everyday
/// ceiling, `xhigh` is where `ultrathink`/`megathink` land, and `max` = "reason to the limit".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effort {
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl Effort {
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::Xhigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

// The very top of the ladder: the user is explicitly asking to burn maximum reasoning. Maps to `max`
// (Claude's "always thinks, no depth limit"). Phrases only — `.contains()` fires anywhere.
const MAX_PHRASES: &[&str] = &[
    "max effort",
    "maximum effort",
    "reason to the limit",
    "think as hard as possible",
];

// `ultrathink`/`megathink` are Claude Code's top thinking keywords → the `xhigh` rung (deep
// exploration), just below `max`. Kept distinct from the "think hard"-family below so the ladder
// escalates: `think hard` (high) < `ultrathink` (xhigh) < `max effort` (max).
const XHIGH_PHRASES: &[&str] = &["ultrathink", "megathink", "think as hard as you can"];

// "think hard"-family + "be thorough": the user is asking for careful reasoning. Multi-word phrases
// use substring `.contains()` (like Claude Code's `.includes()`), so the trigger fires anywhere in
// the message. This is the `high` rung — `xhigh`/`max` live in the two lists above.
const HIGH_PHRASES: &[&str] = &[
    "think harder",
    "think hard",
    "think deeply",
    "think really hard",
    "reason carefully",
    "be thorough",
    "step by step",
    "prove rigorously",
    "deep dive",
];

// "keep it light" phrases: the user wants a short/cheap answer → force `low`.
const LOW_PHRASES: &[&str] = &[
    "one-liner",
    "one liner",
    "in one line",
    "short answer",
    "tl;dr",
    "don't overthink",
    "dont overthink",
    "no need to think",
    "keep it short",
    "quick answer",
];

// Single tokens use word-boundary matching (`adjust` must not fire `just`).
const LOW_WORDS: &[&str] = &["quick", "quickly", "briefly", "fast", "simple", "just"];

// Bare "think"/"consider"/"reason" (without a hard/deeply qualifier) → a gentle nudge to `medium`.
const MEDIUM_WORDS: &[&str] = &["think", "consider", "reason"];

// Heavy work verbs — each contributes to the complexity score (capped, so a verb-list doesn't run away).
const HEAVY_VERBS: &[&str] = &[
    "refactor",
    "debug",
    "design",
    "architect",
    "prove",
    "optimize",
    "migrate",
    "diagnose",
    "benchmark",
    "implement",
    "analyze",
    "rewrite",
];

// Phrases hinting the task spans many files (paired with a `@file`-count check at the call site).
const MULTIFILE_HINTS: &[&str] = &[
    "codebase",
    "across files",
    "these files",
    "every file",
    "entire",
    "all the files",
    "whole project",
    "multiple files",
];

/// PURE. Classify a fully-expanded user turn into an effort tier (non-adaptive: the complexity
/// heuristic tops out at `High`). Thin wrapper over [`classify_effort_with`] preserving the original
/// signature. Runtime callers now pass the adaptive flag via `classify_effort_with`; this stable
/// wrapper is retained for external/test use.
#[allow(dead_code)]
pub fn classify_effort(prompt: &str) -> Option<Effort> {
    classify_effort_with(prompt, false)
}

/// PURE. Classify a turn into an effort tier.
///
/// `None` == "no opinion" — the caller defers to the configured `reasoning_effort` (or omits it).
/// Internal precedence: explicit KEYWORD (max → xhigh → high → low → medium, first match wins) beats
/// the COMPLEXITY heuristic, which only speaks at the extremes. `adaptive` lets the heuristic climb
/// past `High` to `Xhigh` for the very hardest turns (P3 "adaptive routing", opt-in); when `false`
/// the heuristic behaves exactly as before (`High`/`Low`/`None`).
pub fn classify_effort_with(prompt: &str, adaptive: bool) -> Option<Effort> {
    let p = prompt.to_ascii_lowercase();

    // 1) keyword ladder — highest tier wins, so "quick, but ultrathink" resolves to Xhigh.
    if MAX_PHRASES.iter().any(|k| p.contains(k)) {
        return Some(Effort::Max);
    }
    if XHIGH_PHRASES.iter().any(|k| p.contains(k)) {
        return Some(Effort::Xhigh);
    }
    if HIGH_PHRASES.iter().any(|k| p.contains(k)) {
        return Some(Effort::High);
    }
    if LOW_PHRASES.iter().any(|k| p.contains(k)) || LOW_WORDS.iter().any(|w| contains_word(&p, w)) {
        return Some(Effort::Low);
    }
    if MEDIUM_WORDS.iter().any(|w| contains_word(&p, w)) {
        return Some(Effort::Medium);
    }

    // 2) complexity heuristic — only the extremes; the middle band stays `None` (defer/omit).
    complexity_effort(&p, adaptive)
}

/// PURE. Score the turn's shape; only the extremes yield an opinion (precision-first, like
/// `memory::learning::extract_free` — "better to miss than persist noise"). Expects a
/// pre-lowercased string. `allow_high_tiers` lets a very high score climb to `Xhigh` (P3); otherwise
/// the heuristic caps at `High` (never `Xhigh`/`Max` — those stay keyword/ultimate-only, so the
/// heuristic can't silently burn the top of the budget).
fn complexity_effort(p: &str, allow_high_tiers: bool) -> Option<Effort> {
    let words = p.split_whitespace().count();
    let has_fence = p.contains("```");
    let heavy = HEAVY_VERBS.iter().filter(|v| contains_word(p, v)).count() as i32;
    let multi_file = MULTIFILE_HINTS.iter().any(|h| p.contains(h)) || p.matches('@').count() >= 2;

    let mut score = 0i32;
    if words <= 8 {
        score -= 1;
    } else if words >= 60 {
        score += 2;
    } else if words >= 25 {
        score += 1;
    }
    if has_fence {
        score += 2;
    }
    score += heavy.min(2);
    if multi_file {
        score += 2;
    }

    if allow_high_tiers && score >= 5 {
        Some(Effort::Xhigh) // adaptive routing: the very hardest turns earn the deep-exploration rung
    } else if score >= 3 {
        Some(Effort::High)
    } else if score <= -1 {
        Some(Effort::Low)
    } else {
        None // no strong signal ⇒ defer to config default / omit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_beats_complexity_and_ladder_order() {
        // `ultrathink` now maps to the xhigh rung (not high):
        assert_eq!(classify_effort("ultrathink this"), Some(Effort::Xhigh));
        assert_eq!(classify_effort("just give me a quick answer"), Some(Effort::Low));
        // HIGH phrase beats a LOW token in the same turn:
        assert_eq!(classify_effort("quick, but think hard about it"), Some(Effort::High));
        // bare "think" ⇒ medium:
        assert_eq!(classify_effort("think about this API shape"), Some(Effort::Medium));
    }

    #[test]
    fn top_of_ladder_max_and_xhigh() {
        // explicit "max effort" ⇒ Max, and it beats a lower keyword in the same turn:
        assert_eq!(classify_effort("max effort please, think hard"), Some(Effort::Max));
        assert_eq!(classify_effort("give this maximum effort"), Some(Effort::Max));
        // megathink ⇒ Xhigh; "think hard" (no ultra prefix) stays High (not swallowed):
        assert_eq!(classify_effort("megathink the design"), Some(Effort::Xhigh));
        assert_eq!(classify_effort("think hard about the design"), Some(Effort::High));
    }

    #[test]
    fn adaptive_gate_climbs_only_when_enabled() {
        // A very-high-complexity turn (long + fence + heavy verbs + multi-file), no keyword.
        let big = "Refactor and redesign the auth module across files and optimize the whole \
                   project's session handling, migrate the token store, and debug the race:\n\
                   ```rust\nfn a(){}\nfn b(){}\n```\n\
                   there are many more considerations to weigh here across the entire codebase";
        // adaptive ON ⇒ climbs to Xhigh; OFF ⇒ caps at High (default behaviour unchanged).
        assert_eq!(classify_effort_with(big, true), Some(Effort::Xhigh));
        assert_eq!(classify_effort_with(big, false), Some(Effort::High));
        assert_eq!(classify_effort(big), Some(Effort::High)); // thin wrapper == non-adaptive
    }

    #[test]
    fn complexity_extremes_only() {
        // trivial one-liner ⇒ Low
        assert_eq!(classify_effort("what is a closure"), Some(Effort::Low));
        // code fence + heavy verb + multi-file ⇒ High
        let big = "Refactor the auth module across files:\n```rust\nfn a(){}\n```";
        assert_eq!(classify_effort(big), Some(Effort::High));
        // mid-length prose, no strong signal ⇒ None (defer/omit)
        assert_eq!(
            classify_effort("Add a small helper to format the date in the header component"),
            None
        );
    }

    #[test]
    fn word_boundary_no_false_positive() {
        // 'adjust' must NOT trigger 'just'; the sentence is long enough to stay neutral ⇒ None
        assert_eq!(
            classify_effort(
                "Please adjust the padding value in the header so it lines up with the sidebar navigation icons on wide screens"
            ),
            None
        );
    }
}
