//! Event-gating: classify a turn's *intent* so learning only fires on real signals
//! (a normal turn costs zero — no extraction, no writes). The strength also boosts the
//! confidence of whatever the free extractor pulls out of the same turn.

use once_cell::sync::Lazy;
use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalKind {
    /// Explicit "remember this" — strongest, → user-explicit provenance.
    Remember,
    /// The user is correcting the assistant ("no, actually …", "đừng …").
    Correction,
    /// A stated preference ("I prefer …", "always …", "tôi thích …").
    Preference,
    /// Nothing actionable — a normal turn. Learning skips it (event-gated).
    Passive,
}

#[derive(Debug, Clone, Copy)]
pub struct Signal {
    pub kind: SignalKind,
    /// 0.0 (passive) .. 1.0 (explicit remember). Used to boost extractor confidence.
    pub strength: f64,
}

static RE_REMEMBER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(remember|note this|keep in mind|for the record|don'?t forget|ghi nh[ớo]|nh[ớo] r[ằa]ng|nh[ớo] l[àa])\b")
        .unwrap()
});
static RE_CORRECTION: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)(^|\s)(no,|nope\b|actually\b|that'?s (wrong|incorrect|not right)|not (correct|right)|wrong\b|incorrect\b|không phải|sai r[ồo]i|đ[ừu]ng\b)")
        .unwrap()
});
static RE_PREFERENCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(i (prefer|like|favou?r|want|always|usually|never)|please (use|always|never)|always use|never use|use .+ instead|tôi (thích|muốn)|luôn luôn|thường dùng)\b")
        .unwrap()
});

/// The single strongest signal present in `text`.
pub fn detect(text: &str) -> Signal {
    if RE_REMEMBER.is_match(text) {
        return Signal { kind: SignalKind::Remember, strength: 1.0 };
    }
    if RE_CORRECTION.is_match(text) {
        return Signal { kind: SignalKind::Correction, strength: 0.8 };
    }
    if RE_PREFERENCE.is_match(text) {
        return Signal { kind: SignalKind::Preference, strength: 0.65 };
    }
    Signal { kind: SignalKind::Passive, strength: 0.0 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passive_turn_has_no_signal() {
        assert_eq!(detect("can you open the file and show me line 40").kind, SignalKind::Passive);
        assert_eq!(detect("what does this function return").kind, SignalKind::Passive);
    }

    #[test]
    fn remember_is_strongest() {
        assert_eq!(detect("remember that I deploy on fridays").kind, SignalKind::Remember);
        assert_eq!(detect("ghi nhớ là tôi dùng pnpm").kind, SignalKind::Remember);
        assert_eq!(detect("remember that I deploy on fridays").strength, 1.0);
    }

    #[test]
    fn correction_and_preference() {
        assert_eq!(detect("no, that's wrong — use tabs").kind, SignalKind::Correction);
        assert_eq!(detect("actually, use tabs not spaces").kind, SignalKind::Correction);
        assert_eq!(detect("I prefer pnpm over npm").kind, SignalKind::Preference);
    }
}
