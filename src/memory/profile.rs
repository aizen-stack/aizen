//! Deterministic user-profile rollup (phase B2) — the FREE, local "theory of mind".
//!
//! A structured summary of the user's working preferences, aggregated from the fact store
//! with NO LLM. Each dimension emits a verdict + a confidence + the cited basis facts
//! (auditable — every claim is backed by ≥1 stored fact). The consumer is the agent loop
//! (and `ng memory profile` / the `memory_profile` tool).
//!
//! Evidence weight per fact reuses shipped primitives:
//!   w = source.priority()/4 · confidence · (1 + ln1p(reinforced)) · recency
//! (recency = the same curated-exempt decay the live search uses). Confidence per dimension
//! = evidence agreement × a thin-evidence penalty, so one weak inferred fact never reads as
//! a settled preference.

use crate::memory::bloat;
use crate::memory::dimension::Dimension;
use crate::memory::store::MemoryEntry;
use serde::Serialize;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ProfileDim {
    Language,
    Verbosity,
    Autonomy,
    Tooling,
    Stack,
    Frustrations,
}

impl ProfileDim {
    pub fn as_str(self) -> &'static str {
        match self {
            ProfileDim::Language => "language",
            ProfileDim::Verbosity => "verbosity",
            ProfileDim::Autonomy => "autonomy",
            ProfileDim::Tooling => "tooling",
            ProfileDim::Stack => "stack",
            ProfileDim::Frustrations => "frustrations",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Verdict {
    /// A position on an axis, [-1,1], with a human label for the leaning pole.
    Scalar { value: f64, label: String },
    /// A winning choice (+ runner-up + margin).
    Choice {
        value: String,
        runner_up: Option<String>,
        margin: f64,
    },
    /// A ranked set (term, weight).
    Ranked { items: Vec<(String, f64)> },
    /// Not enough evidence to say anything.
    Insufficient,
}

#[derive(Debug, Clone, Serialize)]
pub struct BasisFact {
    pub id: String,
    pub name: String,
    pub weight: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DimSummary {
    pub dim: ProfileDim,
    pub verdict: Verdict,
    pub confidence: f64,
    pub basis: Vec<BasisFact>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserProfile {
    pub dims: Vec<DimSummary>,
}

impl UserProfile {
    pub fn dim(&self, d: ProfileDim) -> Option<&DimSummary> {
        self.dims.iter().find(|s| s.dim == d)
    }
}

// ── lexicons (EN + VI) ───────────────────────────────────────────────────────
const TERSE_KW: &[&str] = &[
    "terse",
    "concise",
    "brief",
    "short",
    "succinct",
    "to the point",
    "ngắn gọn",
    "súc tích",
];
const VERBOSE_KW: &[&str] = &[
    "verbose",
    "detailed",
    "thorough",
    "elaborate",
    "in detail",
    "step by step",
    "dài dòng",
];
const AUTONOMOUS_KW: &[&str] = &[
    "just do it",
    "autonomous",
    "go ahead",
    "proceed",
    "without asking",
    "don't ask",
    "do not ask",
];
const ASKFIRST_KW: &[&str] = &[
    "ask first",
    "ask before",
    "confirm",
    "check with me",
    "wait for",
    "let me review",
    "hỏi trước",
];
const TOOL_NAMES: &[&str] = &[
    "pnpm",
    "npm",
    "yarn",
    "bun",
    "cargo",
    "pip",
    "poetry",
    "uv",
    "git",
    "prettier",
    "eslint",
    "rustfmt",
    "clippy",
    "black",
    "ruff",
    "make",
    "docker",
    "vite",
    "webpack",
    "bash",
    "zsh",
    "fish",
    "powershell",
    "vim",
    "neovim",
    "vscode",
    "tabs",
    "spaces",
];
const STACK_TERMS: &[&str] = &[
    "rust",
    "typescript",
    "javascript",
    "python",
    "react",
    "nextjs",
    "vue",
    "svelte",
    "dotnet",
    "csharp",
    "golang",
    "kotlin",
    "node",
    "postgres",
    "redis",
    "valkey",
    "tailwind",
    "fastapi",
    "django",
    "flask",
    "express",
    "axum",
    "tokio",
    "java",
    "go",
];
const NEG_KW: &[&str] = &[
    "don't", "dont", "do not", "never", "avoid", "stop", "không", "đừng",
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

/// Per-dimension confidence: evidence agreement × thin-evidence penalty, in [0,1].
fn confidence(agree_w: f64, total_w: f64) -> f64 {
    if total_w <= 0.0 {
        return 0.0;
    }
    let agreement = (agree_w / total_w).clamp(0.0, 1.0);
    let evidence = 1.0 - (-total_w).exp();
    (agreement * evidence).clamp(0.0, 1.0)
}

fn fact_text(e: &MemoryEntry) -> String {
    format!("{} {}", e.name, e.body).to_lowercase()
}

/// Build the user profile from the fact store. Pure (no I/O) → unit-testable. `today` is
/// `YYYY-MM-DD`; `half_life` the recency half-life (days).
pub fn build(entries: &[MemoryEntry], today: &str, half_life: f64) -> UserProfile {
    let active: Vec<&MemoryEntry> = entries.iter().filter(|e| e.is_active()).collect();
    let w = |e: &MemoryEntry| -> f64 {
        let prio = e.source.priority() as f64 / 4.0;
        let conf = e.confidence.clamp(0.0, 1.0);
        let reinf = 1.0 + (e.reinforced as f64).ln_1p();
        let rec = bloat::decay::decayed_score(1.0, e, today, half_life);
        (prio * conf * reinf * rec).max(0.0)
    };

    let dims = vec![
        build_language(&active, &w),
        build_scalar(
            ProfileDim::Verbosity,
            Dimension::Style,
            &active,
            &w,
            TERSE_KW,
            VERBOSE_KW,
            "terse",
            "verbose",
        ),
        build_scalar(
            ProfileDim::Autonomy,
            Dimension::Workflow,
            &active,
            &w,
            AUTONOMOUS_KW,
            ASKFIRST_KW,
            "autonomous",
            "asks first",
        ),
        build_tooling(&active, &w),
        build_stack(&active, &w),
        build_frustrations(&active, &w),
    ];
    UserProfile { dims }
}

fn top_basis(mut b: Vec<BasisFact>, n: usize) -> Vec<BasisFact> {
    b.sort_by(|x, y| {
        y.weight
            .partial_cmp(&x.weight)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(x.id.cmp(&y.id))
    });
    b.truncate(n);
    b
}

#[allow(clippy::too_many_arguments)]
fn build_scalar(
    dim: ProfileDim,
    topic: Dimension,
    active: &[&MemoryEntry],
    w: &impl Fn(&MemoryEntry) -> f64,
    pos_kw: &[&str],
    neg_kw: &[&str],
    pos_label: &str,
    neg_label: &str,
) -> DimSummary {
    let mut raw = 0.0;
    let mut total = 0.0;
    let mut basis = Vec::new();
    for e in active.iter().filter(|e| e.dimension == topic) {
        let lower = fact_text(e);
        let words = word_set(&lower);
        let p = hits(&lower, &words, pos_kw) as i32;
        let n = hits(&lower, &words, neg_kw) as i32;
        let s = (p - n).signum() as f64;
        if s == 0.0 {
            continue;
        }
        let wi = w(e);
        if wi <= 0.0 {
            continue;
        }
        raw += wi * s;
        total += wi;
        basis.push(BasisFact {
            id: e.id.clone(),
            name: e.name.clone(),
            weight: wi,
        });
    }
    if total <= 0.0 {
        return DimSummary {
            dim,
            verdict: Verdict::Insufficient,
            confidence: 0.0,
            basis: Vec::new(),
        };
    }
    let value = (raw / 2.0).tanh();
    let label = if value > 0.15 {
        pos_label
    } else if value < -0.15 {
        neg_label
    } else {
        "mixed"
    };
    DimSummary {
        dim,
        verdict: Verdict::Scalar {
            value,
            label: label.to_string(),
        },
        confidence: confidence(raw.abs(), total),
        basis: top_basis(basis, 3),
    }
}

fn build_language(active: &[&MemoryEntry], w: &impl Fn(&MemoryEntry) -> f64) -> DimSummary {
    let langs: [(&str, &[&str]); 2] = [
        ("vietnamese", &["vietnamese", "tiếng việt", "vietnam"]),
        ("english", &["english", "tiếng anh"]),
    ];
    let mut weights = [0.0f64; 2];
    let mut basis = Vec::new();
    for e in active.iter().filter(|e| e.dimension == Dimension::Style) {
        let lower = fact_text(e);
        let words = word_set(&lower);
        for (i, (_, kws)) in langs.iter().enumerate() {
            if hits(&lower, &words, kws) > 0 {
                let wi = w(e);
                weights[i] += wi;
                basis.push(BasisFact {
                    id: e.id.clone(),
                    name: e.name.clone(),
                    weight: wi,
                });
            }
        }
    }
    let total: f64 = weights.iter().sum();
    if total <= 0.0 {
        return DimSummary {
            dim: ProfileDim::Language,
            verdict: Verdict::Insufficient,
            confidence: 0.0,
            basis: Vec::new(),
        };
    }
    let (top_i, top_w) = weights
        .iter()
        .enumerate()
        .fold((0usize, 0.0), |a, (i, &v)| if v > a.1 { (i, v) } else { a });
    let second = weights
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != top_i)
        .map(|(_, &v)| v)
        .fold(0.0, f64::max);
    let margin = (top_w - second) / top_w;
    let runner_up = if second > 0.0 {
        Some(
            langs[weights
                .iter()
                .enumerate()
                .filter(|(i, _)| *i != top_i)
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .map(|(i, _)| i)
                .unwrap_or(top_i)]
            .0
            .to_string(),
        )
    } else {
        None
    };
    DimSummary {
        dim: ProfileDim::Language,
        verdict: Verdict::Choice {
            value: langs[top_i].0.to_string(),
            runner_up,
            margin,
        },
        confidence: confidence(top_w, total) * margin.max(0.5),
        basis: top_basis(basis, 3),
    }
}

fn build_tooling(active: &[&MemoryEntry], w: &impl Fn(&MemoryEntry) -> f64) -> DimSummary {
    use std::collections::HashMap;
    const MARKERS: &[&str] = &[" over ", " instead of ", " rather than ", " not "];
    let mut net: HashMap<&str, f64> = HashMap::new();
    let mut basis = Vec::new();
    for e in active.iter().filter(|e| e.dimension == Dimension::Tooling) {
        let lower = fact_text(e);
        let words = word_set(&lower);
        let wi = w(e);
        if wi <= 0.0 {
            continue;
        }
        let neg_fact = hits(&lower, &words, NEG_KW) > 0;
        // Directional "prefer X over Y": split into preferred (pre) vs dispreferred (post)
        // by WORD set (avoids the "npm" ⊂ "pnpm" substring trap).
        let split = MARKERS
            .iter()
            .filter_map(|m| lower.find(m).map(|i| (i, m.len())))
            .min_by_key(|(i, _)| *i);
        let (pre_words, post_words) = match split {
            Some((i, ml)) => (word_set(&lower[..i]), word_set(&lower[i + ml..])),
            None => (words.clone(), HashSet::new()),
        };
        let mut touched = false;
        for tool in TOOL_NAMES {
            let in_pre = pre_words.contains(*tool);
            let in_post = post_words.contains(*tool);
            if !in_pre && !in_post {
                continue;
            }
            let sign = if neg_fact || (in_post && !in_pre) {
                -1.0
            } else {
                1.0
            };
            *net.entry(tool).or_insert(0.0) += sign * wi;
            touched = true;
        }
        if touched {
            basis.push(BasisFact {
                id: e.id.clone(),
                name: e.name.clone(),
                weight: wi,
            });
        }
    }
    let mut items: Vec<(String, f64)> = net
        .into_iter()
        .filter(|(_, v)| *v > 0.0)
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    items.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    items.truncate(6);
    if items.is_empty() {
        return DimSummary {
            dim: ProfileDim::Tooling,
            verdict: Verdict::Insufficient,
            confidence: 0.0,
            basis: Vec::new(),
        };
    }
    let total: f64 = items.iter().map(|(_, v)| v).sum();
    DimSummary {
        dim: ProfileDim::Tooling,
        verdict: Verdict::Ranked { items },
        confidence: confidence(total, total + 1.0).max(0.3),
        basis: top_basis(basis, 5),
    }
}

fn build_stack(active: &[&MemoryEntry], w: &impl Fn(&MemoryEntry) -> f64) -> DimSummary {
    use std::collections::HashMap;
    let mut weights: HashMap<&str, f64> = HashMap::new();
    let mut basis = Vec::new();
    for e in active.iter().filter(|e| e.dimension == Dimension::Stack) {
        let lower = fact_text(e);
        let words = word_set(&lower);
        let wi = w(e);
        let mut touched = false;
        for term in STACK_TERMS {
            if words.contains(*term) {
                *weights.entry(term).or_insert(0.0) += wi;
                touched = true;
            }
        }
        if touched {
            basis.push(BasisFact {
                id: e.id.clone(),
                name: e.name.clone(),
                weight: wi,
            });
        }
    }
    let mut items: Vec<(String, f64)> = weights
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    items.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    items.truncate(8);
    if items.is_empty() {
        return DimSummary {
            dim: ProfileDim::Stack,
            verdict: Verdict::Insufficient,
            confidence: 0.0,
            basis: Vec::new(),
        };
    }
    let total: f64 = items.iter().map(|(_, v)| v).sum();
    DimSummary {
        dim: ProfileDim::Stack,
        verdict: Verdict::Ranked { items },
        confidence: confidence(total, total + 1.0).max(0.3),
        basis: top_basis(basis, 5),
    }
}

fn build_frustrations(active: &[&MemoryEntry], w: &impl Fn(&MemoryEntry) -> f64) -> DimSummary {
    let mut items: Vec<(String, f64)> = Vec::new();
    let mut basis = Vec::new();
    for e in active.iter() {
        let lower = fact_text(e);
        let words = word_set(&lower);
        if hits(&lower, &words, NEG_KW) > 0 {
            let wi = w(e);
            if wi > 0.0 {
                items.push((e.name.clone(), wi));
                basis.push(BasisFact {
                    id: e.id.clone(),
                    name: e.name.clone(),
                    weight: wi,
                });
            }
        }
    }
    items.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    items.truncate(8);
    if items.is_empty() {
        return DimSummary {
            dim: ProfileDim::Frustrations,
            verdict: Verdict::Insufficient,
            confidence: 0.0,
            basis: Vec::new(),
        };
    }
    let total: f64 = items.iter().map(|(_, v)| v).sum();
    DimSummary {
        dim: ProfileDim::Frustrations,
        verdict: Verdict::Ranked { items },
        confidence: confidence(total, total + 1.0).max(0.3),
        basis: top_basis(basis, 5),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::provenance::ProvenanceKind;
    use crate::memory::store::MemoryType;

    fn fact(id: &str, body: &str, src: ProvenanceKind, reinforced: u32) -> MemoryEntry {
        let dim = crate::memory::dimension::classify(body);
        MemoryEntry {
            id: id.into(),
            name: id.into(),
            body: body.into(),
            mtype: MemoryType::User,
            source: src,
            confidence: 0.9,
            reinforced,
            created: Some("2026-06-20".into()),
            updated: Some("2026-06-20".into()),
            dimension: dim,
            ..Default::default()
        }
    }

    const TODAY: &str = "2026-06-20";

    #[test]
    fn verbosity_terse_from_style_facts() {
        let entries = vec![
            fact(
                "f1",
                "keep replies concise and terse",
                ProvenanceKind::Manual,
                3,
            ),
            fact(
                "f2",
                "be brief and to the point",
                ProvenanceKind::UserExplicit,
                1,
            ),
        ];
        let p = build(&entries, TODAY, 30.0);
        let v = p.dim(ProfileDim::Verbosity).unwrap();
        match &v.verdict {
            Verdict::Scalar { value, label } => {
                assert!(*value > 0.3, "should lean terse, got {value}");
                assert_eq!(label, "terse");
            }
            other => panic!("expected scalar, got {other:?}"),
        }
        assert!(v.confidence > 0.3);
        assert!(!v.basis.is_empty());
    }

    #[test]
    fn language_vietnamese_choice() {
        let entries = vec![fact(
            "f1",
            "please reply in vietnamese",
            ProvenanceKind::Manual,
            2,
        )];
        let p = build(&entries, TODAY, 30.0);
        match &p.dim(ProfileDim::Language).unwrap().verdict {
            Verdict::Choice { value, .. } => assert_eq!(value, "vietnamese"),
            other => panic!("expected choice, got {other:?}"),
        }
    }

    #[test]
    fn tooling_prefers_pnpm_excludes_avoided() {
        let entries = vec![
            fact(
                "f1",
                "I prefer pnpm as my package manager",
                ProvenanceKind::Manual,
                4,
            ),
            fact(
                "f2",
                "avoid yarn in this repo",
                ProvenanceKind::UserExplicit,
                1,
            ),
        ];
        let p = build(&entries, TODAY, 30.0);
        match &p.dim(ProfileDim::Tooling).unwrap().verdict {
            Verdict::Ranked { items } => {
                assert!(items.iter().any(|(t, _)| t == "pnpm"), "pnpm preferred");
                assert!(
                    !items.iter().any(|(t, _)| t == "yarn"),
                    "avoided yarn must not rank as preferred"
                );
            }
            other => panic!("expected ranked, got {other:?}"),
        }
    }

    #[test]
    fn tooling_directional_pref_demotes_the_loser() {
        // "prefer pnpm over npm" → pnpm preferred, npm NOT (and npm⊂pnpm must not leak).
        let entries = vec![fact(
            "f1",
            "prefer pnpm over npm",
            ProvenanceKind::Manual,
            2,
        )];
        let p = build(&entries, TODAY, 30.0);
        match &p.dim(ProfileDim::Tooling).unwrap().verdict {
            Verdict::Ranked { items } => {
                assert!(items.iter().any(|(t, _)| t == "pnpm"), "pnpm preferred");
                assert!(
                    !items.iter().any(|(t, _)| t == "npm"),
                    "npm is the loser, must not rank"
                );
            }
            other => panic!("expected ranked, got {other:?}"),
        }
    }

    #[test]
    fn stack_ranks_terms() {
        let entries = vec![fact(
            "f1",
            "the backend is rust with tokio and axum",
            ProvenanceKind::Manual,
            1,
        )];
        let p = build(&entries, TODAY, 30.0);
        match &p.dim(ProfileDim::Stack).unwrap().verdict {
            Verdict::Ranked { items } => assert!(items.iter().any(|(t, _)| t == "rust")),
            other => panic!("expected ranked, got {other:?}"),
        }
    }

    #[test]
    fn frustrations_surface_negatives() {
        let entries = vec![fact(
            "f1",
            "never force-push to main",
            ProvenanceKind::UserExplicit,
            2,
        )];
        let p = build(&entries, TODAY, 30.0);
        match &p.dim(ProfileDim::Frustrations).unwrap().verdict {
            Verdict::Ranked { items } => assert!(!items.is_empty()),
            other => panic!("expected ranked, got {other:?}"),
        }
    }

    #[test]
    fn empty_store_is_insufficient_everywhere() {
        let p = build(&[], TODAY, 30.0);
        assert!(p
            .dims
            .iter()
            .all(|d| matches!(d.verdict, Verdict::Insufficient)));
    }

    #[test]
    fn thin_evidence_lowers_confidence() {
        // one weak inferred fact → low confidence (not a settled preference)
        let weak = vec![fact("f1", "terse", ProvenanceKind::Inferred, 0)];
        let strong = vec![
            fact("f1", "terse", ProvenanceKind::Manual, 5),
            fact("f2", "concise and brief", ProvenanceKind::Manual, 5),
        ];
        let cw = {
            let mut e = weak[0].clone();
            e.confidence = 0.5;
            build(std::slice::from_ref(&e), TODAY, 30.0)
                .dim(ProfileDim::Verbosity)
                .unwrap()
                .confidence
        };
        let cs = build(&strong, TODAY, 30.0)
            .dim(ProfileDim::Verbosity)
            .unwrap()
            .confidence;
        assert!(
            cs > cw,
            "strong corroborated evidence must beat one weak inferred fact ({cs} vs {cw})"
        );
    }
}
