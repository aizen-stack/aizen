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

/// Turns about authoring / becoming a role-play CHARACTER (a persona), EN + VI. Such a turn
/// describes a FICTIONAL character's traits (role, voice, a language it speaks), NOT the user's
/// own preferences — so the free extractor must NOT mine it into user memory (that is what leaked
/// a `persona-…` "fact" into the user's verbosity profile). Persona content belongs only in
/// `~/.aizen/personas` via the `persona_create` tool. Deliberately noun-anchored (character /
/// persona / role-play / nhân vật / đóng vai …) so it never suppresses a genuine preference turn.
static RE_PERSONA_INTENT: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)(nhân vật|nhân cách|đóng vai|nhập vai|vào vai|hoá thân|hóa thân|vai diễn|\bpersona\b|\brole[\- ]?play\b|\broleplay\b|character card|in character|stay in character|hãy là|đóng giả)",
    )
    .unwrap()
});

/// True when the turn is about creating / role-playing a character (persona), not stating a user
/// preference. The learning pipeline skips these so a character's traits never become user facts.
pub fn looks_like_persona_intent(text: &str) -> bool {
    RE_PERSONA_INTENT.is_match(text)
}

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

    #[test]
    fn persona_authoring_turns_are_flagged() {
        // EN + VI ways of asking for a character / role-play — must be caught.
        assert!(looks_like_persona_intent("tạo cho tôi một nhân vật là tướng lịch sử"));
        assert!(looks_like_persona_intent("hãy đóng vai một thám tử noir"));
        assert!(looks_like_persona_intent("create a persona named ApexCode, a principal architect"));
        assert!(looks_like_persona_intent("let's do some role-play as a pirate captain"));
        assert!(looks_like_persona_intent("here is a character card, become them"));
    }

    #[test]
    fn genuine_preference_turns_are_not_persona() {
        // A real user preference must NOT be mistaken for persona authoring (no over-suppression).
        assert!(!looks_like_persona_intent("I prefer pnpm over npm"));
        assert!(!looks_like_persona_intent("tôi thích dùng rust hơn go"));
        assert!(!looks_like_persona_intent("remember that I deploy on fridays"));
        assert!(!looks_like_persona_intent("reply in vietnamese and keep it terse"));
    }

    // ── the STRONG, fact-based leak-guard: turn_authored_persona ─────────────────
    use crate::core::types::{FunctionCall, Message, ToolCall};
    use crate::memory::learning::{turn_authored_persona, PERSONA_AUTHORING_TOOL};

    fn assistant_calling(tool: &str) -> Message {
        Message {
            role: "assistant".into(),
            content: None,
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                kind: "function".into(),
                function: FunctionCall { name: tool.into(), arguments: "{}".into() },
            }],
            tool_call_id: None,
            images: Vec::new(),
            cache_control: None,
        }
    }

    #[test]
    fn turn_authored_persona_fires_only_when_the_tool_did() {
        // A turn whose assistant fired `persona_create` is authoring a character → guarded, even
        // if the user's phrasing carries no persona keyword (what the regex layer would miss).
        let authored = vec![
            Message::user("here, use this system prompt: a terse strategist who speaks english"),
            assistant_calling(PERSONA_AUTHORING_TOOL),
        ];
        assert!(turn_authored_persona(&authored), "persona_create firing must be caught");

        // A normal turn that ran OTHER tools is not authoring → learning proceeds as usual.
        let plain = vec![Message::user("I prefer pnpm over npm"), assistant_calling("read_file")];
        assert!(!turn_authored_persona(&plain), "unrelated tools must not trip the guard");

        // Only the LAST turn counts: a persona authored earlier must not suppress this turn's learning.
        let prior_turn = vec![
            Message::user("make me a pirate character"),
            assistant_calling(PERSONA_AUTHORING_TOOL),
            Message::user("I prefer pnpm over npm"),
            assistant_calling("read_file"),
        ];
        assert!(!turn_authored_persona(&prior_turn), "guard is scoped to the current turn only");
    }
}
