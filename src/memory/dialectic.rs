//! Dialectic Q&A (phase B3) — answer a natural-language question ABOUT the user, FREE/local,
//! with a HARD ABSTAIN firewall. This is the free analogue of Honcho's dialectic API: it
//! NEVER free-form reasons and NEVER fabricates. It either (a) answers from a confident
//! profile dimension (cited), (b) returns the closest stored facts as evidence, or (c)
//! ABSTAINS — which is the explicit handoff point to the (deferred, paid) LLM tier.
//!
//! The abstain boundary is what makes this benchable: a counterfactual-novel question (predict
//! an UNSEEN situation, no matching fact) MUST abstain; a settled question (maps to a confident
//! dimension) must NOT.

use crate::memory::profile::{BasisFact, ProfileDim, UserProfile, Verdict};
use crate::memory::store::MemoryEntry;
use serde::Serialize;
use std::collections::HashSet;

/// Minimum per-dimension confidence to answer from the profile (else fall back / abstain).
const TAU_ANSWER: f64 = 0.35;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AbstainReason {
    /// Asks to predict an unseen/hypothetical situation with no matching evidence.
    CounterfactualNovel,
    /// Routed to a dimension, but evidence is too thin/conflicting.
    InsufficientEvidence,
    /// Nothing in memory addresses the question.
    NoMatch,
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AnswerKind {
    /// Answered from a profile dimension.
    Profile { verdict: Verdict },
    /// Fallback: the closest stored facts (in `basis`), no settled preference.
    Evidence,
    /// Could not answer — handoff to the user / paid tier.
    Abstain { reason: AbstainReason },
}

#[derive(Debug, Clone, Serialize)]
pub struct Answer {
    pub dimension: Option<ProfileDim>,
    pub kind: AnswerKind,
    pub confidence: f64,
    pub basis: Vec<BasisFact>,
    pub text: String,
}

impl Answer {
    /// Public predicate (used by tests + callers that branch on the handoff).
    #[allow(dead_code)]
    pub fn is_abstain(&self) -> bool {
        matches!(self.kind, AnswerKind::Abstain { .. })
    }
}

// ── intent routing lexicons (question → dimension) ───────────────────────────
const Q_VERBOSITY: &[&str] = &[
    "verbose", "terse", "concise", "brief", "short", "long", "detail", "detailed", "wordy",
    "length",
];
const Q_LANGUAGE: &[&str] = &[
    "reply in",
    "respond in",
    "speak",
    "write in",
    "vietnamese",
    "english",
    "tiếng việt",
    "tiếng anh",
    "what language should",
];
const Q_AUTONOMY: &[&str] = &[
    "ask",
    "ask first",
    "confirm",
    "permission",
    "autonomous",
    "just do it",
    "without asking",
    "should i ask",
    "proceed",
    "go ahead",
];
const Q_TOOLING: &[&str] = &[
    "package manager",
    "pnpm",
    "npm",
    "yarn",
    "formatter",
    "linter",
    "editor",
    "shell",
    "tabs",
    "spaces",
    "prettier",
    "eslint",
    "which tool",
    "vcs",
];
const Q_STACK: &[&str] = &[
    "framework",
    "stack",
    "programming",
    "rust",
    "typescript",
    "react",
    "python",
    "tech stack",
    "tokio",
    "axum",
    "library",
    "which language",
];
const Q_FRUSTRATIONS: &[&str] = &[
    "avoid",
    "hate",
    "dislike",
    "frustrat",
    "annoy",
    "footgun",
    "pet peeve",
    "should i not",
    "things to avoid",
    "never",
];

const COUNTERFACTUAL: &[&str] = &[
    "would you",
    "what if",
    "hypothetic",
    "imagine",
    "suppose",
    "if i were",
    "unfamiliar",
    "never seen",
    "predict",
    "in a new situation",
];

fn word_set(lower: &str) -> HashSet<String> {
    lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect()
}

fn hits(lower: &str, words: &HashSet<String>, kws: &[&str]) -> usize {
    kws.iter()
        .filter(|kw| {
            if kw.contains(' ') {
                lower.contains(**kw)
            } else {
                words.contains(**kw)
            }
        })
        .count()
}

/// Route a question to a profile dimension (argmax over keyword hits; tie → priority order).
pub fn route(query: &str) -> Option<ProfileDim> {
    let lower = query.to_lowercase();
    let words = word_set(&lower);
    // (hits, priority-rank, dim) — more hits wins; tie → lower rank.
    let candidates = [
        (hits(&lower, &words, Q_TOOLING), 0, ProfileDim::Tooling),
        (hits(&lower, &words, Q_AUTONOMY), 1, ProfileDim::Autonomy),
        (hits(&lower, &words, Q_VERBOSITY), 2, ProfileDim::Verbosity),
        (hits(&lower, &words, Q_LANGUAGE), 3, ProfileDim::Language),
        (hits(&lower, &words, Q_STACK), 4, ProfileDim::Stack),
        (
            hits(&lower, &words, Q_FRUSTRATIONS),
            5,
            ProfileDim::Frustrations,
        ),
    ];
    candidates
        .iter()
        .filter(|(c, _, _)| *c > 0)
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)))
        .map(|(_, _, d)| *d)
}

fn is_counterfactual(query: &str) -> bool {
    let lower = query.to_lowercase();
    COUNTERFACTUAL.iter().any(|m| lower.contains(m))
}

/// Answer a question about the user. `entries` is the active fact store (for the fallback).
pub fn answer(profile: &UserProfile, entries: &[MemoryEntry], query: &str) -> Answer {
    let routed = route(query);

    if let Some(d) = routed {
        if let Some(s) = profile.dim(d) {
            if !matches!(s.verdict, Verdict::Insufficient) && s.confidence >= TAU_ANSWER {
                return Answer {
                    dimension: Some(d),
                    confidence: s.confidence,
                    text: render_verdict(d, &s.verdict, s.confidence),
                    kind: AnswerKind::Profile {
                        verdict: s.verdict.clone(),
                    },
                    basis: s.basis.clone(),
                };
            }
        }
    }

    // Not confidently answerable from the profile. A hypothetical/unseen-situation question
    // with no settled answer is the exact thing the free tier must NOT fake.
    if is_counterfactual(query) {
        return abstain(routed, AbstainReason::CounterfactualNovel);
    }

    // Fact-grounded fallback: the closest stored facts, verbatim, with no claimed preference.
    let hits: Vec<_> = crate::memory::search_in(query, 3, entries.to_vec())
        .into_iter()
        .filter(|h| h.score > 0.05)
        .collect();
    if !hits.is_empty() {
        let basis: Vec<BasisFact> = hits
            .iter()
            .map(|h| BasisFact {
                id: h.entry.id.clone(),
                name: h.entry.name.clone(),
                weight: h.score,
            })
            .collect();
        let names: Vec<String> = basis.iter().map(|b| b.name.clone()).collect();
        return Answer {
            dimension: None,
            confidence: hits[0].score,
            kind: AnswerKind::Evidence,
            text: format!(
                "No settled preference; closest stored facts: {}",
                names.join("; ")
            ),
            basis,
        };
    }

    // We could route to a dimension but found no usable evidence (vs never routing at all).
    let reason = if routed.is_some() {
        AbstainReason::InsufficientEvidence
    } else {
        AbstainReason::NoMatch
    };
    abstain(routed, reason)
}

fn abstain(dim: Option<ProfileDim>, reason: AbstainReason) -> Answer {
    let text = match reason {
        AbstainReason::CounterfactualNovel => {
            "I have no evidence for this hypothetical/unseen case — answering would be a guess. Ask the user."
        }
        AbstainReason::InsufficientEvidence => "Not enough in memory to answer this confidently.",
        AbstainReason::NoMatch => "Nothing in memory addresses this question.",
    };
    Answer {
        dimension: dim,
        kind: AnswerKind::Abstain { reason },
        confidence: 0.0,
        basis: Vec::new(),
        text: text.to_string(),
    }
}

fn render_verdict(d: ProfileDim, v: &Verdict, conf: f64) -> String {
    let c = format!("{:.0}%", conf * 100.0);
    match v {
        Verdict::Scalar { label, .. } => format!("{}: {label} (confidence {c})", d.as_str()),
        Verdict::Choice {
            value, runner_up, ..
        } => match runner_up {
            Some(r) => format!("{}: {value} (over {r}, confidence {c})", d.as_str()),
            None => format!("{}: {value} (confidence {c})", d.as_str()),
        },
        Verdict::Ranked { items } => {
            let top: Vec<String> = items.iter().take(5).map(|(t, _)| t.clone()).collect();
            format!("{}: {} (confidence {c})", d.as_str(), top.join(", "))
        }
        Verdict::Insufficient => format!("{}: unknown", d.as_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::profile;
    use crate::memory::provenance::ProvenanceKind;
    use crate::memory::store::{MemoryEntry, MemoryType};

    fn fact(id: &str, body: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            name: id.into(),
            body: body.into(),
            mtype: MemoryType::User,
            source: ProvenanceKind::Manual,
            confidence: 0.95,
            reinforced: 4,
            created: Some("2026-06-20".into()),
            updated: Some("2026-06-20".into()),
            dimension: crate::memory::dimension::classify(body),
            ..Default::default()
        }
    }

    fn sample() -> (UserProfile, Vec<MemoryEntry>) {
        let entries = vec![
            fact("f1", "keep replies concise and terse"),
            fact("f2", "please reply in vietnamese"),
            fact("f3", "prefer pnpm over npm as my package manager"),
            fact("f4", "the backend is rust with tokio and axum"),
        ];
        let p = profile::build(&entries, "2026-06-20", 30.0);
        (p, entries)
    }

    #[test]
    fn routes_to_dimensions() {
        assert_eq!(
            route("how verbose should I be?"),
            Some(ProfileDim::Verbosity)
        );
        assert_eq!(
            route("which package manager do they use?"),
            Some(ProfileDim::Tooling)
        );
        assert_eq!(
            route("should I ask before deleting?"),
            Some(ProfileDim::Autonomy)
        );
        assert_eq!(
            route("what language should I reply in?"),
            Some(ProfileDim::Language)
        );
        assert_eq!(route("the meeting is on friday"), None);
    }

    #[test]
    fn settled_question_answers_not_abstains() {
        let (p, e) = sample();
        let a = answer(&p, &e, "should my replies be terse or verbose?");
        assert!(
            !a.is_abstain(),
            "a settled verbosity question must be answered, got {a:?}"
        );
        assert_eq!(a.dimension, Some(ProfileDim::Verbosity));
        assert!(!a.basis.is_empty(), "answer must cite basis facts");
    }

    #[test]
    fn tooling_question_answers() {
        let (p, e) = sample();
        let a = answer(&p, &e, "which package manager should I use?");
        assert!(!a.is_abstain());
        assert!(a.text.contains("pnpm"));
    }

    #[test]
    fn counterfactual_novel_abstains() {
        let (p, e) = sample();
        // hypothetical about something with no settled evidence → MUST abstain.
        let a = answer(
            &p,
            &e,
            "would you want me to rewrite the whole module in haskell?",
        );
        assert!(a.is_abstain());
        match a.kind {
            AnswerKind::Abstain { reason } => {
                assert_eq!(reason, AbstainReason::CounterfactualNovel)
            }
            other => panic!("expected counterfactual abstain, got {other:?}"),
        }
    }

    #[test]
    fn unknown_question_abstains_no_match() {
        let (p, e) = sample();
        let a = answer(
            &p,
            &e,
            "what is the airspeed velocity of an unladen swallow?",
        );
        assert!(a.is_abstain());
        match a.kind {
            AnswerKind::Abstain { reason } => assert_eq!(reason, AbstainReason::NoMatch),
            other => panic!("expected no-match abstain, got {other:?}"),
        }
    }

    #[test]
    fn routed_but_no_evidence_is_insufficient() {
        let (p, e) = sample();
        // routes to Autonomy, but the sample has no autonomy facts + nothing matches lexically.
        let a = answer(&p, &e, "should I ask first or just proceed?");
        assert!(a.is_abstain());
        match a.kind {
            AnswerKind::Abstain { reason } => {
                assert_eq!(reason, AbstainReason::InsufficientEvidence)
            }
            other => panic!("expected insufficient-evidence abstain, got {other:?}"),
        }
        assert_eq!(a.dimension, Some(ProfileDim::Autonomy));
    }

    #[test]
    fn confident_dim_answers_even_with_would_phrasing() {
        let (p, e) = sample();
        // "would you" phrasing, but Language is settled → answer, don't abstain.
        let a = answer(&p, &e, "would you reply in vietnamese?");
        assert!(
            !a.is_abstain(),
            "settled dim must win over counterfactual phrasing"
        );
        assert_eq!(a.dimension, Some(ProfileDim::Language));
    }

    #[test]
    fn fallback_returns_evidence_when_no_dim_but_facts_match() {
        let (p, e) = sample();
        // routes to nothing, not counterfactual, but a fact lexically matches "axum"
        let a = answer(&p, &e, "tell me about axum");
        // routes to Stack (axum is a stack term) → answered OR evidence; must not falsely abstain
        assert!(
            !a.is_abstain(),
            "a question with matching facts must not abstain, got {a:?}"
        );
    }
}
