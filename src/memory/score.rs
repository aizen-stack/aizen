//! Lexical relevance scoring.
//!
//! Two distinct jobs, two scorers — they are NOT interchangeable:
//!
//! 1. **Retrieval ranking** = [`Bm25Index`] (P7). BM25 (k1=1.2, b=0.75) with a session-start
//!    IDF over the active corpus + length normalization. Corpus-relative, unbounded, sensitive
//!    to term rarity + doc length — the right tool for "which of these N docs best answers the
//!    query". A pure-Rust [`strsim`] fuzzy fallback bridges near-miss query terms (typos /
//!    morphology the diacritic fix doesn't cover) without breaking the static binary.
//! 2. **Pairwise dedup similarity** = [`lexical_score`] / [`lexical_score_tokens`]. The
//!    extension's verified blend `0.7*jaccard + 0.3*(W/(W+1))`, `W = Σ log1p(docTermFreq)`,
//!    **bounded [0,1]**. Consolidation compares ONE candidate against ONE existing fact and
//!    UPDATE-reinforces past a fixed threshold (0.78); a corpus-relative unbounded BM25 score
//!    has no meaning for a 2-document comparison, so this stays a normalized similarity.
//!
//! `recency_factor` is a SEPARATE, opt-in multiply (NOT part of either scorer) used only by
//! the learned store so stale facts sink below the top-K cut (anti-bloat, P4).

use crate::memory::tokenize::tokenize;
use std::collections::{HashMap, HashSet};

/// BM25 tuning. Defaults are the bench-confirmed Okapi values; exposed for a grid-search.
#[derive(Debug, Clone, Copy)]
pub struct Bm25Params {
    pub k1: f64,
    pub b: f64,
    /// Fuzzy bridge: a query term with no exact doc match is matched against the doc's terms
    /// by Jaro-Winkler; a best sim ≥ this floor counts as a damped hit. Conservative (0.92) so
    /// fuzzy never out-scores an exact hit and false-bridges stay rare. `0.0` disables fuzzy.
    pub fuzzy_min_sim: f64,
    /// Only query terms at least this long are eligible for the fuzzy bridge (short tokens are
    /// noisy under edit-distance, e.g. `test`/`text`).
    pub fuzzy_min_len: usize,
}

impl Default for Bm25Params {
    fn default() -> Self {
        Bm25Params { k1: 1.2, b: 0.75, fuzzy_min_sim: 0.92, fuzzy_min_len: 4 }
    }
}

/// A BM25 index over a corpus of pre-tokenized docs: document frequencies + average doc length,
/// computed once per search (the personal corpus is small — O(N) build is < 1ms at 1k facts).
pub struct Bm25Index {
    df: HashMap<String, u32>,
    n_docs: f64,
    avgdl: f64,
    params: Bm25Params,
}

impl Bm25Index {
    /// Build from a corpus of token lists (one per doc). Empty corpus → a valid empty index.
    pub fn build<'a, I>(docs: I) -> Bm25Index
    where
        I: IntoIterator<Item = &'a [String]>,
    {
        Bm25Index::build_with(docs, Bm25Params::default())
    }

    pub fn build_with<'a, I>(docs: I, params: Bm25Params) -> Bm25Index
    where
        I: IntoIterator<Item = &'a [String]>,
    {
        let mut df: HashMap<String, u32> = HashMap::new();
        let mut n_docs = 0u64;
        let mut total_len = 0u64;
        for doc in docs {
            n_docs += 1;
            total_len += doc.len() as u64;
            let mut seen: HashSet<&str> = HashSet::new();
            for t in doc {
                if seen.insert(t.as_str()) {
                    *df.entry(t.clone()).or_insert(0) += 1;
                }
            }
        }
        let avgdl = if n_docs == 0 { 1.0 } else { (total_len as f64 / n_docs as f64).max(1.0) };
        Bm25Index { df, n_docs: n_docs as f64, avgdl, params }
    }

    /// Lucene/Okapi floored IDF: `ln(1 + (N - df + 0.5)/(df + 0.5))` — always ≥ 0, stable on a
    /// tiny personal corpus (a term appearing in every doc still scores a small positive, so an
    /// overlapping doc is never filtered out as a zero).
    fn idf(&self, term: &str) -> f64 {
        let df = *self.df.get(term).unwrap_or(&0) as f64;
        (1.0 + (self.n_docs - df + 0.5) / (df + 0.5)).ln()
    }

    /// The length-normalization denominator factor `k1·(1 - b + b·|D|/avgdl)`.
    fn norm(&self, doc_len: f64) -> f64 {
        self.params.k1 * (1.0 - self.params.b + self.params.b * (doc_len / self.avgdl))
    }

    /// BM25 relevance of one doc to the query. ≥ 0; 0 means no overlapping term.
    pub fn score(&self, q_tokens: &[String], doc_tokens: &[String]) -> f64 {
        self.score_inner(q_tokens, doc_tokens, false)
    }

    /// BM25 + fuzzy fallback (the production retrieval path).
    pub fn score_fuzzy(&self, q_tokens: &[String], doc_tokens: &[String]) -> f64 {
        self.score_inner(q_tokens, doc_tokens, true)
    }

    fn score_inner(&self, q_tokens: &[String], doc_tokens: &[String], fuzzy: bool) -> f64 {
        if q_tokens.is_empty() || doc_tokens.is_empty() {
            return 0.0;
        }
        let mut tf: HashMap<&str, u32> = HashMap::new();
        for t in doc_tokens {
            *tf.entry(t.as_str()).or_insert(0) += 1;
        }
        let norm = self.norm(doc_tokens.len() as f64);
        let k1 = self.params.k1;
        let q_set: HashSet<&str> = q_tokens.iter().map(String::as_str).collect();

        let mut score = 0.0;
        for t in &q_set {
            if let Some(&f) = tf.get(*t) {
                let f = f as f64;
                score += self.idf(t) * (f * (k1 + 1.0)) / (f + norm);
            } else if fuzzy
                && self.params.fuzzy_min_sim > 0.0
                && t.chars().count() >= self.params.fuzzy_min_len
            {
                // No exact hit — bridge to the closest doc term (typo / morphology).
                if let Some((best_term, sim)) = best_fuzzy(t, &tf, self.params.fuzzy_min_sim) {
                    // Damped single-occurrence hit, scored against the matched term's rarity.
                    score += self.idf(best_term) * sim * (1.0 * (k1 + 1.0)) / (1.0 + norm);
                }
            }
        }
        score
    }
}

/// Best fuzzy match for `q_term` among the doc's terms by Jaro-Winkler, gated by `min_sim`.
fn best_fuzzy<'a>(q_term: &str, doc_tf: &HashMap<&'a str, u32>, min_sim: f64) -> Option<(&'a str, f64)> {
    let mut best: Option<(&str, f64)> = None;
    for &dt in doc_tf.keys() {
        // skip equal-or-trivial-length doc terms that can't be a meaningful near-match
        if dt.chars().count() < 3 {
            continue;
        }
        let sim = strsim::jaro_winkler(q_term, dt);
        if sim >= min_sim && best.map(|(_, bs)| sim > bs).unwrap_or(true) {
            best = Some((dt, sim));
        }
    }
    best
}

/// Lexical score in [0,1]. `doc_tokens` is the entry's precomputed token list. Convenience wrapper
/// (re-tokenizes the query) exercised by tests; consolidation uses `lexical_score_tokens` directly.
#[allow(dead_code)]
pub fn lexical_score(query: &str, doc_tokens: &[String]) -> f64 {
    lexical_score_tokens(&tokenize(query), doc_tokens)
}

/// Same, but with a pre-tokenized query (avoids re-tokenizing across many docs).
pub fn lexical_score_tokens(q_tokens: &[String], doc_tokens: &[String]) -> f64 {
    if q_tokens.is_empty() || doc_tokens.is_empty() {
        return 0.0;
    }
    let q_set: HashSet<&str> = q_tokens.iter().map(String::as_str).collect();

    let mut doc_freq: HashMap<&str, u32> = HashMap::new();
    for t in doc_tokens {
        *doc_freq.entry(t.as_str()).or_insert(0) += 1;
    }

    let mut inter = 0u32;
    let mut weighted = 0f64;
    for t in &q_set {
        if let Some(&f) = doc_freq.get(*t) {
            if f > 0 {
                inter += 1;
                weighted += (f as f64).ln_1p();
            }
        }
    }
    if inter == 0 {
        return 0.0;
    }

    let doc_set_size = doc_freq.len() as f64;
    let union = q_set.len() as f64 + doc_set_size - inter as f64;
    let jaccard = if union > 0.0 { inter as f64 / union } else { 0.0 };
    let tf_boost = weighted / (weighted + 1.0);
    0.7 * jaccard + 0.3 * tf_boost
}

/// Recency multiplier `exp(-ageDays / half_life)` (default half_life 30d → ~21d half-life).
/// Opt-in; applied to the learned store rank only.
pub fn recency_factor(age_days: f64, half_life_days: f64) -> f64 {
    let age = age_days.max(0.0);
    (-age / half_life_days).exp()
}

/// Salience (P8 evolution): how much a fact has *earned* its rank through reuse, bounded
/// **[0.5, 1.0]** so it can lift a heavily-reused fact by at most 2× but never let a fresh weak
/// lexical match vault a strong old one (BM25 stays dominant). `final = bm25 · decay · salience`.
///
/// `salience = 0.5 + 0.3·(r/(r+3)) + 0.2·retrieved_recency`, where `r = reinforced` (saturating —
/// the first few reuses matter most, diminishing after) and `retrieved_recency ∈ [0,1]` is the
/// decay of `last_retrieved` (a fact used yesterday is more salient than one used a year ago).
/// `r=0, recency=0 → 0.5` (neutral); maxes at `1.0`.
pub fn salience(reinforced: u32, retrieved_recency: f64) -> f64 {
    let r = reinforced as f64;
    let reuse = 0.3 * (r / (r + 3.0));
    let recency = 0.2 * retrieved_recency.clamp(0.0, 1.0);
    (0.5 + reuse + recency).clamp(0.5, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::tokenize::tokenize;

    #[test]
    fn no_overlap_is_zero() {
        let doc = tokenize("completely unrelated content about cats");
        assert_eq!(lexical_score("postgres index tuning", &doc), 0.0);
    }

    #[test]
    fn full_overlap_scores_high() {
        let doc = tokenize("auth login flow");
        let s = lexical_score("auth login flow", &doc);
        assert!(s > 0.5, "expected strong score, got {s}");
    }

    #[test]
    fn partial_overlap_between() {
        let doc = tokenize("auth login flow with oauth and jwt sessions");
        let full = lexical_score("auth login flow with oauth and jwt sessions", &doc);
        let part = lexical_score("auth flow", &doc);
        assert!(part > 0.0 && part < full, "part={part} full={full}");
    }

    #[test]
    fn matches_reference_formula() {
        // doc tokens: ["auth","auth","login"] (auth appears twice)
        let doc = vec!["auth".to_string(), "auth".to_string(), "login".to_string()];
        // query: ["auth","login"]  -> inter=2
        // weighted = ln1p(2) + ln1p(1) = 1.0986123 + 0.6931472 = 1.7917595
        // doc_set_size = 2 ; union = 2 + 2 - 2 = 2 ; jaccard = 1.0
        // tf = 1.79175947 / 2.79175947 = 0.64180295
        // score = 0.7*1.0 + 0.3*0.64180295 = 0.89254089
        let q = vec!["auth".to_string(), "login".to_string()];
        let s = lexical_score_tokens(&q, &doc);
        assert!((s - 0.89254089).abs() < 1e-6, "got {s}");
    }

    #[test]
    fn recency_decays() {
        assert!((recency_factor(0.0, 30.0) - 1.0).abs() < 1e-9);
        assert!((recency_factor(30.0, 30.0) - std::f64::consts::E.recip()).abs() < 1e-9);
    }

    // ── BM25 (P7) ───────────────────────────────────────────────────────────

    fn corpus3() -> Vec<Vec<String>> {
        vec![
            tokenize("postgres index tuning and query plans"),
            tokenize("react suspense and tanstack query"),
            tokenize("auth login oauth jwt session refresh"),
        ]
    }

    #[test]
    fn bm25_ranks_rarer_term_higher() {
        let docs = corpus3();
        let idx = Bm25Index::build(docs.iter().map(Vec::as_slice));
        // "oauth" is rare (1 doc) → the auth doc must win for an oauth query.
        let s_auth = idx.score(&tokenize("oauth login"), &docs[2]);
        let s_pg = idx.score(&tokenize("oauth login"), &docs[0]);
        assert!(s_auth > s_pg, "auth={s_auth} pg={s_pg}");
        assert_eq!(s_pg, 0.0, "no overlap → zero");
    }

    #[test]
    fn bm25_idf_is_non_negative_on_ubiquitous_term() {
        // "query" appears in 2 of 3 docs; IDF stays ≥0 (floored form) so an overlap never zeroes.
        let docs = corpus3();
        let idx = Bm25Index::build(docs.iter().map(Vec::as_slice));
        let s = idx.score(&tokenize("query"), &docs[0]);
        assert!(s > 0.0, "ubiquitous-but-present term still scores positive, got {s}");
    }

    #[test]
    fn bm25_empty_query_or_doc_is_zero() {
        let docs = corpus3();
        let idx = Bm25Index::build(docs.iter().map(Vec::as_slice));
        assert_eq!(idx.score(&[], &docs[0]), 0.0);
        assert_eq!(idx.score(&tokenize("auth"), &[]), 0.0);
    }

    #[test]
    fn bm25_length_norm_prefers_concise_match() {
        // Same single hit, but a shorter doc should score higher (length normalization).
        let short = tokenize("kubernetes deployment");
        let long = tokenize(&format!("kubernetes deployment {}", "unrelated filler words ".repeat(20)));
        let docs = vec![short.clone(), long.clone()];
        let idx = Bm25Index::build(docs.iter().map(Vec::as_slice));
        let s_short = idx.score(&tokenize("kubernetes"), &short);
        let s_long = idx.score(&tokenize("kubernetes"), &long);
        assert!(s_short > s_long, "short={s_short} long={s_long}");
    }

    #[test]
    fn fuzzy_bridges_a_typo_that_exact_bm25_misses() {
        let docs = corpus3();
        let idx = Bm25Index::build(docs.iter().map(Vec::as_slice));
        // "postgers" is a typo of "postgres" — exact BM25 scores 0, fuzzy bridges it.
        let exact = idx.score(&tokenize("postgers tuning"), &docs[0]);
        let fuzzy = idx.score_fuzzy(&tokenize("postgers tuning"), &docs[0]);
        // "tuning" hits exactly in both; the typo only contributes under fuzzy.
        assert!(fuzzy > exact, "fuzzy={fuzzy} should beat exact={exact}");
    }

    #[test]
    fn fuzzy_never_out_scores_an_exact_hit() {
        let docs = corpus3();
        let idx = Bm25Index::build(docs.iter().map(Vec::as_slice));
        let exact = idx.score_fuzzy(&tokenize("postgres"), &docs[0]);
        let typo = idx.score_fuzzy(&tokenize("postgers"), &docs[0]);
        assert!(exact > typo, "exact={exact} must beat fuzzy={typo}");
    }

    // ── salience (P8) ────────────────────────────────────────────────────────

    #[test]
    fn salience_is_bounded_and_neutral_at_zero() {
        assert!((salience(0, 0.0) - 0.5).abs() < 1e-9, "no reuse → neutral 0.5");
        assert!(salience(1_000_000, 1.0) <= 1.0, "bounded above by 1.0");
        assert!(salience(0, 1.0) >= 0.5 && salience(0, 1.0) <= 1.0);
    }

    #[test]
    fn salience_rises_with_reinforcement_but_saturates() {
        let s0 = salience(0, 0.0);
        let s1 = salience(1, 0.0);
        let s10 = salience(10, 0.0);
        assert!(s1 > s0 && s10 > s1, "more reuse → more salient");
        // diminishing returns: the 0→1 step beats the 9→10 step.
        assert!(s1 - s0 > salience(10, 0.0) - salience(9, 0.0));
    }

    #[test]
    fn salience_recency_adds_within_cap() {
        // fresh retrieval is more salient than stale, all else equal.
        assert!(salience(3, 1.0) > salience(3, 0.0));
        // but never exceeds the [0.5,1.0] cap even maxed out.
        assert!(salience(1000, 1.0) <= 1.0 + 1e-9);
    }
}
