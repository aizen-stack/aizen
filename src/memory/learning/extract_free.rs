//! FREE (zero-token) regex fact extraction. Precision-first: it is better to miss a
//! soft signal than to persist noise, because the write path is automatic. Paid
//! aux-model extraction (higher recall, strong-signal-gated) is P6.

use crate::memory::store::MemoryType;
use once_cell::sync::Lazy;
use regex::Regex;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CandidateKind {
    Remember,
    Preference,
    NegPreference,
    StyleLanguage,
    StyleTone,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polarity {
    Affirm,
    Negate,
}

#[derive(Debug, Clone)]
pub struct Candidate {
    /// The declarative fact body to persist.
    pub text: String,
    /// A short title (becomes the file slug after `store::slugify`).
    pub name: String,
    // `kind`/`polarity` are captured at extraction for future kind- and negation-aware routing;
    // the current router keys off mtype/is_style only, so they aren't read yet (kept, not dropped,
    // so the signal survives until that wiring lands).
    #[allow(dead_code)]
    pub kind: CandidateKind,
    pub mtype: MemoryType,
    /// Style facts (language/tone) feed the always-on core (confirmation-gated).
    pub is_style: bool,
    #[allow(dead_code)]
    pub polarity: Polarity,
    pub confidence: f64,
}

static RE_REMEMBER: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(?:remember|note this|keep in mind|for the record|don'?t forget)(?: that| this)?[:,]?\s+(.{3,300})",
    )
    .unwrap()
});
static RE_USE_INSTEAD: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\buse\s+(.{1,60}?)\s+(?:instead of|rather than|not)\s+(.{1,60}?)(?:[.,;!?]|$)")
        .unwrap()
});
static RE_PREFERENCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\bi (?:prefer|like|favou?r|always use|usually use|want you to use)\s+(.{2,200}?)(?:[.,;!?]|$)")
        .unwrap()
});
static RE_NEG_PREFERENCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:don'?t|do not|never|please don'?t|avoid)\s+(?:use|using|do|run)\s+(.{2,120}?)(?:[.,;!?]|$)")
        .unwrap()
});
static RE_LANG: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:reply|respond|answer|write|speak|talk)\b[^.]{0,40}?\b(vietnamese|tiếng việt|english|tiếng anh)\b")
        .unwrap()
});
static RE_TONE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(?:be|keep it|make it|stay|reply|respond)\b[^.]{0,20}?\b(concise|brief|terse|short|succinct|to the point|ngắn gọn|súc tích)\b")
        .unwrap()
});

fn clean_span(s: &str) -> String {
    s.trim()
        .trim_matches(|c| c == '"' || c == '\'' || c == '`')
        .trim()
        .to_string()
}

/// Short title (≤ 60 chars at a word boundary) for the file slug.
fn title_of(fact: &str) -> String {
    let f = fact.trim();
    if f.chars().count() <= 60 {
        return f.to_string();
    }
    let mut out = String::new();
    for w in f.split_whitespace() {
        if out.len() + w.len() + 1 > 56 {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    if out.is_empty() {
        f.chars().take(56).collect()
    } else {
        out
    }
}

/// Extract candidate facts from a single user turn. May return several; the caller
/// sanitizes, threat-scans, routes, and consolidates each.
pub fn extract(text: &str) -> Vec<Candidate> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut push =
        |text: String, name: String, kind, mtype, is_style, polarity, confidence: f64| {
            let text = clean_span(&text);
            if text.len() < 3 {
                return;
            }
            // de-dup by case-insensitive body
            if out.iter().any(|c| c.text.eq_ignore_ascii_case(&text)) {
                return;
            }
            out.push(Candidate {
                name: title_of(if name.is_empty() { &text } else { &name }),
                text,
                kind,
                mtype,
                is_style,
                polarity,
                confidence,
            });
        };

    // Style: language preference (highest-value, high precision)
    if let Some(c) = RE_LANG.captures(text) {
        let lang = clean_span(&c[1]);
        push(
            format!("reply in {lang}"),
            format!("reply-language-{lang}"),
            CandidateKind::StyleLanguage,
            MemoryType::User,
            true,
            Polarity::Affirm,
            0.85,
        );
    }
    // Style: tone / length
    if let Some(c) = RE_TONE.captures(text) {
        let tone = clean_span(&c[1]);
        push(
            format!("keep replies {tone}"),
            format!("reply-tone-{tone}"),
            CandidateKind::StyleTone,
            MemoryType::User,
            true,
            Polarity::Affirm,
            0.85,
        );
    }
    // "use X instead of Y" → a directed preference
    if let Some(c) = RE_USE_INSTEAD.captures(text) {
        let x = clean_span(&c[1]);
        let y = clean_span(&c[2]);
        if !x.is_empty() && !y.is_empty() {
            push(
                format!("prefer {x} over {y}"),
                format!("prefer-{x}"),
                CandidateKind::Preference,
                MemoryType::User,
                false,
                Polarity::Affirm,
                0.82,
            );
        }
    }
    // "I prefer/like/always use X"
    if let Some(c) = RE_PREFERENCE.captures(text) {
        let span = clean_span(&c[1]);
        push(
            format!("prefers {span}"),
            format!("prefers-{span}"),
            CandidateKind::Preference,
            MemoryType::User,
            false,
            Polarity::Affirm,
            0.8,
        );
    }
    // "don't use / avoid X"
    if let Some(c) = RE_NEG_PREFERENCE.captures(text) {
        let span = clean_span(&c[1]);
        push(
            format!("avoid using {span}"),
            format!("avoid-{span}"),
            CandidateKind::NegPreference,
            MemoryType::User,
            false,
            Polarity::Negate,
            0.68, // bare negation → review; a correction-signalled one boosts over the store bar
        );
    }
    // Explicit "remember …" — capture the thing to remember verbatim.
    if let Some(c) = RE_REMEMBER.captures(text) {
        let span = clean_span(&c[1]);
        // a remember about the user vs a project/reference fact — cheap heuristic
        let lc = span.to_lowercase();
        let mtype = if lc.starts_with("i ") || lc.contains(" my ") || lc.starts_with("my ") {
            MemoryType::User
        } else if lc.contains("repo") || lc.contains("project") || lc.contains("codebase") {
            MemoryType::Project
        } else {
            MemoryType::Reference
        };
        push(
            span.clone(),
            span,
            CandidateKind::Remember,
            mtype,
            false,
            Polarity::Affirm,
            0.95,
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passive_turns_extract_nothing() {
        assert!(extract("open the file and show me the bug").is_empty());
        assert!(extract("what does AiProxyOrchestrator do?").is_empty());
        assert!(extract("run the tests please").is_empty());
    }

    #[test]
    fn preference_extracted_with_token() {
        let c = extract("I prefer pnpm over npm for everything");
        assert!(!c.is_empty());
        assert!(c.iter().any(|x| x.text.to_lowercase().contains("pnpm")));
    }

    #[test]
    fn use_instead_becomes_directed_pref() {
        let c = extract("use tabs instead of spaces");
        assert!(c.iter().any(|x| x.text.contains("prefer")
            && x.text.contains("tabs")
            && x.text.contains("spaces")));
    }

    #[test]
    fn language_is_style() {
        let c = extract("please reply in Vietnamese from now on");
        assert!(c
            .iter()
            .any(|x| x.is_style && x.kind == CandidateKind::StyleLanguage));
    }

    #[test]
    fn remember_high_confidence() {
        let c = extract("remember that I deploy on fridays only");
        assert!(c
            .iter()
            .any(|x| x.kind == CandidateKind::Remember && x.confidence >= 0.9));
    }

    #[test]
    fn negative_preference() {
        let c = extract("don't use yarn in this repo");
        assert!(c
            .iter()
            .any(|x| x.polarity == Polarity::Negate && x.text.to_lowercase().contains("yarn")));
    }
}
