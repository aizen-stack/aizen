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

// WEAK "keep it light" tokens: single words that only SOMETIMES mean "spend less thought". Unlike
// `LOW_PHRASES` (which are unambiguous requests about the ANSWER), each of these is routinely a
// discourse particle or a property of the CODE rather than an instruction about effort — "just
// refactor X", "make it fast", "a simple wrapper", "quick question: why does this crash". So a hit
// here does NOT short-circuit to `Low`; it only wins when the complexity heuristic finds nothing
// hard (see `classify_effort_with`). `just` and `simple` are deliberately absent: as bare tokens they
// carry no signal about effort at all.
//
// Word-boundary matched, so `adjust` cannot fire `just`-style false positives.
const LOW_WORDS: &[&str] = &["quick", "quickly", "briefly", "fast"];

// Untranslated failure nouns. These survive in every language's bug report ("sửa cái bug này",
// "crash beim Start"), so they are the one difficulty signal that keeps working when the rest of the
// English-only lists go quiet. Each is worth a point AND suppresses the short-message penalty: a
// three-word crash report is a hard task stated briefly, not a trivial one.
// Boundary-matched, so each inflection needs its own entry (`fail` does not match `failing`).
const DIFFICULTY_NOUNS: &[&str] = &[
    "bug",
    "crash",
    "crashes",
    "panic",
    "deadlock",
    "race",
    "leak",
    "regression",
    "hang",
    "hangs",
    "timeout",
    "segfault",
    "fail",
    "fails",
    "failing",
    "failure",
    "broken",
];

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
    // Strong low phrases are explicit output/effort requests ("in one line", "don't overthink") —
    // respect them immediately. Bare tokens are weaker: "quick question" can introduce a hard bug,
    // and "make it fast" describes the code, not the thinking budget. Let real complexity outrank
    // those tokens; use Low only when the heuristic has no contrary signal.
    if LOW_PHRASES.iter().any(|k| p.contains(k)) {
        return Some(Effort::Low);
    }
    let weak_low = LOW_WORDS.iter().any(|w| contains_word(&p, w));
    if MEDIUM_WORDS.iter().any(|w| contains_word(&p, w)) {
        return Some(Effort::Medium);
    }

    // 2) complexity heuristic — only the extremes; the middle band stays `None` (defer/omit). A weak
    // "keep it light" token is one point of evidence here, not a verdict: it can tip a turn that
    // looks easy down to `low`, but it cannot outvote a fence, a heavy verb, or a failure noun.
    complexity_effort(&p, adaptive, weak_low)
}

/// PURE. Score the turn's shape; only the extremes yield an opinion (precision-first, like
/// `memory::learning::extract_free` — "better to miss than persist noise"). Expects a
/// pre-lowercased string. `allow_high_tiers` lets a very high score climb to `Xhigh` (P3); otherwise
/// the heuristic caps at `High` (never `Xhigh`/`Max` — those stay keyword/ultimate-only, so the
/// heuristic can't silently burn the top of the budget). `weak_low` is a single point AGAINST
/// difficulty, contributed by an ambiguous "keep it light" token — enough to settle an otherwise
/// featureless turn, never enough to overrule a fence, a heavy verb, or a failure noun.
///
/// The scoring is deliberately language-blind about DIFFICULTY: the heavy-verb and multi-file lists
/// only fire on English, so for any other language the live signals are the untranslated failure
/// nouns, a code fence, `@file` count, and length. That is why a short message loses its penalty as
/// soon as ANY difficulty signal appears — with the English lists silent, mis-scoring a hard request
/// as trivial is the failure mode to avoid, and `None` (defer to the configured default) is always
/// the safer miss than `Low`.
fn complexity_effort(p: &str, allow_high_tiers: bool, weak_low: bool) -> Option<Effort> {
    let words = p.split_whitespace().count();
    let has_fence = p.contains("```");
    let heavy = HEAVY_VERBS.iter().filter(|v| contains_word(p, v)).count() as i32;
    let difficult = DIFFICULTY_NOUNS
        .iter()
        .filter(|n| contains_word(p, n))
        .count() as i32;
    let multi_file = MULTIFILE_HINTS.iter().any(|h| p.contains(h)) || p.matches('@').count() >= 2;

    let mut score = 0i32;
    // A short message is only cheap when it has no sign of a hard problem. "fix this crash" is
    // brief but difficult; do not let the brevity prior erase the difficulty signal.
    if words <= 8 {
        if heavy == 0 && difficult == 0 && !has_fence && !multi_file {
            score -= 1;
        }
    } else if words >= 60 {
        score += 2;
    } else if words >= 25 {
        score += 1;
    }
    if has_fence {
        score += 2;
    }
    score += heavy.min(2);
    score += difficult.min(2);
    if multi_file {
        score += 2;
    }
    if weak_low {
        score -= 1;
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
        assert_eq!(
            classify_effort("just give me a quick answer"),
            Some(Effort::Low)
        );
        // HIGH phrase beats a LOW token in the same turn:
        assert_eq!(
            classify_effort("quick, but think hard about it"),
            Some(Effort::High)
        );
        // bare "think" ⇒ medium:
        assert_eq!(
            classify_effort("think about this API shape"),
            Some(Effort::Medium)
        );
    }

    #[test]
    fn top_of_ladder_max_and_xhigh() {
        // explicit "max effort" ⇒ Max, and it beats a lower keyword in the same turn:
        assert_eq!(
            classify_effort("max effort please, think hard"),
            Some(Effort::Max)
        );
        assert_eq!(
            classify_effort("give this maximum effort"),
            Some(Effort::Max)
        );
        // megathink ⇒ Xhigh; "think hard" (no ultra prefix) stays High (not swallowed):
        assert_eq!(classify_effort("megathink the design"), Some(Effort::Xhigh));
        assert_eq!(
            classify_effort("think hard about the design"),
            Some(Effort::High)
        );
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

    #[test]
    fn weak_low_token_cannot_veto_real_difficulty() {
        // "just"/"simple" are gone from LOW_WORDS entirely: as bare tokens they say nothing about
        // effort, and treating them as a verdict pinned the hardest turns to the cheapest rung.
        assert_eq!(
            classify_effort("just refactor the auth module across every file"),
            Some(Effort::High),
            "a heavy verb + multi-file scope must outrank an ambiguous discourse particle"
        );
        // A surviving weak token ("quick") still cannot erase a failure noun; the turn defers rather
        // than dropping to Low, because "cheap" is the expensive guess to get wrong.
        assert_eq!(
            classify_effort("quick question: why does the parse test fail?"),
            None,
            "politeness formula + a real failure ⇒ defer to the configured default, never Low"
        );
    }

    #[test]
    fn weak_low_still_decides_a_featureless_turn() {
        // Nothing hard anywhere, plus an explicit "keep it light" token ⇒ Low is the right call.
        assert_eq!(
            classify_effort("make the header font fast to load"),
            Some(Effort::Low)
        );
        // And an unambiguous LOW_PHRASE keeps its immediate short-circuit, difficulty or not.
        assert_eq!(
            classify_effort("in one line, why did the build fail?"),
            Some(Effort::Low),
            "an explicit request about the ANSWER is the user's call and is honoured verbatim"
        );
    }

    #[test]
    fn short_message_with_a_failure_noun_is_not_treated_as_trivial() {
        // The brevity prior (words <= 8 ⇒ -1) is what routed every terse bug report to `low`. A
        // failure noun now suppresses it, so these defer instead of being downgraded. This is the
        // path that matters when the English verb lists are silent (non-English requests): length
        // is the only other signal, and it must not be allowed to mean "easy" on its own.
        assert_eq!(classify_effort("fix this crash"), None);
        assert_eq!(classify_effort("the parse test is failing"), None);
        // Genuinely trivial and short ⇒ still Low; the penalty is intact where it belongs.
        assert_eq!(
            classify_effort("fix the typo in the README"),
            Some(Effort::Low)
        );
        assert_eq!(classify_effort("what is a closure"), Some(Effort::Low));
    }
}
