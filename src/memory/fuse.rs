//! Reciprocal Rank Fusion — combine the lexical and dense rankings into one.
//!
//! `RRF(d) = Σ_lists 1/(k + rank_d)` over the lists `d` appears in (1-based rank). We use
//! `k = 10` (the bench-tuned value the repo re-confirmed, not the literature default 60).
//! RRF fuses by RANK not score, so the lexical blend and cosine never need to be on the
//! same scale — the property that makes a lexical-floor + dense-tier hybrid robust.

use std::collections::HashMap;

/// Fuse ranked id lists (each already in best→worst order). Returns ids by fused score desc,
/// ties broken by the id for determinism.
pub fn rrf(lists: &[Vec<String>], k: f64) -> Vec<(String, f64)> {
    let mut score: HashMap<String, f64> = HashMap::new();
    for list in lists {
        for (rank0, id) in list.iter().enumerate() {
            let rank = rank0 as f64 + 1.0; // 1-based
            *score.entry(id.clone()).or_insert(0.0) += 1.0 / (k + rank);
        }
    }
    let mut out: Vec<(String, f64)> = score.into_iter().collect();
    out.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(v: &[(String, f64)]) -> Vec<String> {
        v.iter().map(|(i, _)| i.clone()).collect()
    }

    #[test]
    fn agreement_wins() {
        // an item ranked highly in BOTH lists beats items strong in only one
        let lexical = vec!["a".into(), "b".into(), "c".into()];
        let dense = vec!["a".into(), "c".into(), "b".into()];
        let fused = rrf(&[lexical, dense], 10.0);
        assert_eq!(fused[0].0, "a");
    }

    #[test]
    fn dense_only_hit_still_surfaces() {
        // an item ONLY the dense tier found (paraphrase) still enters the fused list
        let lexical = vec!["a".into(), "b".into()];
        let dense = vec!["z".into(), "a".into()];
        let fused = rrf(&[lexical, dense], 10.0);
        assert!(ids(&fused).contains(&"z".to_string()));
    }

    #[test]
    fn empty_lists_fuse_to_empty() {
        assert!(rrf(&[], 10.0).is_empty());
        assert!(rrf(&[vec![], vec![]], 10.0).is_empty());
    }
}
