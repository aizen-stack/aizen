//! Recall-bench metrics (anti-oracle: scored against a human-labeled ACCEPTABLE SET,
//! never a single oracle id). Binary relevance: a returned id is relevant iff it is in
//! the query's acceptable set.

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

/// recall@k = |acceptable ∩ top-k| / |acceptable|.
pub fn recall_at_k(ranked: &[String], acceptable: &HashSet<String>, k: usize) -> f64 {
    if acceptable.is_empty() {
        return 1.0; // no_hit query: nothing to recall
    }
    let hit = ranked
        .iter()
        .take(k)
        .filter(|id| acceptable.contains(*id))
        .count();
    hit as f64 / acceptable.len() as f64
}

/// precision@k = |acceptable ∩ top-k| / min(k, returned).
pub fn precision_at_k(ranked: &[String], acceptable: &HashSet<String>, k: usize) -> f64 {
    let denom = ranked.len().min(k);
    if denom == 0 {
        return 0.0;
    }
    let hit = ranked
        .iter()
        .take(k)
        .filter(|id| acceptable.contains(*id))
        .count();
    hit as f64 / denom as f64
}

/// Mean reciprocal rank: 1/(rank of first acceptable), else 0.
pub fn reciprocal_rank(ranked: &[String], acceptable: &HashSet<String>) -> f64 {
    for (i, id) in ranked.iter().enumerate() {
        if acceptable.contains(id) {
            return 1.0 / (i as f64 + 1.0);
        }
    }
    0.0
}

/// nDCG@k with binary relevance.
pub fn ndcg_at_k(ranked: &[String], acceptable: &HashSet<String>, k: usize) -> f64 {
    if acceptable.is_empty() {
        return 1.0;
    }
    let mut dcg = 0.0;
    for (i, id) in ranked.iter().take(k).enumerate() {
        if acceptable.contains(id) {
            dcg += 1.0 / ((i as f64 + 2.0).log2());
        }
    }
    let ideal = acceptable.len().min(k);
    let mut idcg = 0.0;
    for i in 0..ideal {
        idcg += 1.0 / ((i as f64 + 2.0).log2());
    }
    if idcg > 0.0 {
        dcg / idcg
    } else {
        0.0
    }
}

/// noise@k = fraction of the returned top-k that is NOT acceptable.
pub fn noise_at_k(ranked: &[String], acceptable: &HashSet<String>, k: usize) -> f64 {
    let denom = ranked.len().min(k);
    if denom == 0 {
        return 0.0;
    }
    let noise = ranked
        .iter()
        .take(k)
        .filter(|id| !acceptable.contains(*id))
        .count();
    noise as f64 / denom as f64
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BenchMetrics {
    pub query_count: usize,
    pub recall_at_5: f64,
    pub recall_at_10: f64,
    pub mrr: f64,
    pub precision_at_5: f64,
    pub ndcg_at_5: f64,
    pub noise_rate: f64,
}

/// Aggregate per-query (ranked, acceptable) evaluations into mean metrics.
pub fn aggregate(evals: &[(Vec<String>, HashSet<String>)]) -> BenchMetrics {
    let n = evals.len().max(1) as f64;
    let mut m = BenchMetrics {
        query_count: evals.len(),
        recall_at_5: 0.0,
        recall_at_10: 0.0,
        mrr: 0.0,
        precision_at_5: 0.0,
        ndcg_at_5: 0.0,
        noise_rate: 0.0,
    };
    for (ranked, acc) in evals {
        m.recall_at_5 += recall_at_k(ranked, acc, 5);
        m.recall_at_10 += recall_at_k(ranked, acc, 10);
        m.mrr += reciprocal_rank(ranked, acc);
        m.precision_at_5 += precision_at_k(ranked, acc, 5);
        m.ndcg_at_5 += ndcg_at_k(ranked, acc, 5);
        m.noise_rate += noise_at_k(ranked, acc, 5);
    }
    m.recall_at_5 /= n;
    m.recall_at_10 /= n;
    m.mrr /= n;
    m.precision_at_5 /= n;
    m.ndcg_at_5 /= n;
    m.noise_rate /= n;
    m
}

/// Compare current vs baseline; return human-readable regressions (drop > eps on a
/// quality metric, or noise up by > eps). Empty = gate passes.
pub fn regressions(baseline: &BenchMetrics, current: &BenchMetrics, eps: f64) -> Vec<String> {
    let mut out = Vec::new();
    let drop = |name: &str, b: f64, c: f64, v: &mut Vec<String>| {
        if c + eps < b {
            v.push(format!("{name} regressed: {b:.4} -> {c:.4} (eps {eps})"));
        }
    };
    drop("recall@5", baseline.recall_at_5, current.recall_at_5, &mut out);
    drop("recall@10", baseline.recall_at_10, current.recall_at_10, &mut out);
    drop("mrr", baseline.mrr, current.mrr, &mut out);
    drop("ndcg@5", baseline.ndcg_at_5, current.ndcg_at_5, &mut out);
    if current.noise_rate > baseline.noise_rate + eps {
        out.push(format!(
            "noise_rate worsened: {:.4} -> {:.4} (eps {eps})",
            baseline.noise_rate, current.noise_rate
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }
    fn rank(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn recall_basic() {
        let r = rank(&["a", "b", "c"]);
        assert_eq!(recall_at_k(&r, &set(&["a"]), 5), 1.0);
        assert_eq!(recall_at_k(&r, &set(&["z"]), 5), 0.0);
        assert!((recall_at_k(&r, &set(&["a", "z"]), 5) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn mrr_first_position() {
        assert_eq!(reciprocal_rank(&rank(&["a", "b"]), &set(&["a"])), 1.0);
        assert_eq!(reciprocal_rank(&rank(&["x", "a"]), &set(&["a"])), 0.5);
        assert_eq!(reciprocal_rank(&rank(&["x", "y"]), &set(&["a"])), 0.0);
    }

    #[test]
    fn ndcg_perfect_and_zero() {
        assert!((ndcg_at_k(&rank(&["a"]), &set(&["a"]), 5) - 1.0).abs() < 1e-9);
        assert_eq!(ndcg_at_k(&rank(&["x"]), &set(&["a"]), 5), 0.0);
    }

    #[test]
    fn noise_counts_nonacceptable() {
        // top-5 returns 1 good + 1 bad -> noise 0.5
        assert!((noise_at_k(&rank(&["a", "b"]), &set(&["a"]), 5) - 0.5).abs() < 1e-9);
    }

    #[test]
    fn regressions_detected() {
        let base = BenchMetrics {
            query_count: 10,
            recall_at_5: 0.9,
            recall_at_10: 0.95,
            mrr: 0.8,
            precision_at_5: 0.2,
            ndcg_at_5: 0.85,
            noise_rate: 0.8,
        };
        let mut cur = base.clone();
        cur.recall_at_5 = 0.7; // big drop
        let regs = regressions(&base, &cur, 0.02);
        assert!(regs.iter().any(|r| r.contains("recall@5")));
        // within eps = no regression
        let regs2 = regressions(&base, &base, 0.02);
        assert!(regs2.is_empty());
    }
}
