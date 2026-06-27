//! Write-time near-duplicate detection via MinHash over character 5-grams.
//!
//! The P3 lexical consolidator already catches token-level dups; MinHash adds a
//! character-level signal that survives reordering, minor edits, and typos (e.g.
//! "prefers pnpm" vs "prefer pnpm."). It's a cheap second guard before an insert.
//! Semantic-level dedup (cosine on embeddings) folds in with the dense tier (P5).

use once_cell::sync::Lazy;

const NUM_PERM: usize = 64;
const SHINGLE_K: usize = 5;

/// Fixed (a,b) pairs for `num_perm` universal hash permutations — deterministic so a
/// signature is stable across runs (no RNG, which would break reproducibility).
static PERMS: Lazy<Vec<(u64, u64)>> = Lazy::new(|| {
    let mut v = Vec::with_capacity(NUM_PERM);
    // a simple SplitMix64 sequence seeded by a constant → deterministic odd multipliers
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        x = x.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    };
    for _ in 0..NUM_PERM {
        let a = next() | 1; // odd multiplier
        let b = next();
        v.push((a, b));
    }
    v
});

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// Hash the normalized text's character 5-grams. Short text → one shingle of the whole.
fn shingles(text: &str) -> Vec<u64> {
    let norm: String = text.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    let chars: Vec<char> = norm.chars().collect();
    let mut out = Vec::new();
    if chars.len() < SHINGLE_K {
        if !chars.is_empty() {
            out.push(fnv1a(norm.as_bytes()));
        }
        return out;
    }
    let mut buf = String::new();
    for w in chars.windows(SHINGLE_K) {
        buf.clear();
        buf.extend(w.iter());
        out.push(fnv1a(buf.as_bytes()));
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// MinHash signature of `text` (length `NUM_PERM`). Empty text → empty signature.
pub fn signature(text: &str) -> Vec<u64> {
    let sh = shingles(text);
    if sh.is_empty() {
        return Vec::new();
    }
    PERMS
        .iter()
        .map(|&(a, b)| {
            sh.iter()
                .map(|&h| h.wrapping_mul(a).wrapping_add(b))
                .min()
                .unwrap_or(u64::MAX)
        })
        .collect()
}

/// Estimated Jaccard similarity in [0,1] from two signatures.
pub fn similarity(a: &[u64], b: &[u64]) -> f64 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let eq = a.iter().zip(b).filter(|(x, y)| x == y).count();
    eq as f64 / a.len() as f64
}

/// True if `a` and `b` are near-duplicates at `threshold` (estimated Jaccard).
pub fn is_near_duplicate(a: &str, b: &str, threshold: f64) -> bool {
    similarity(&signature(a), &signature(b)) >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_text_is_dup() {
        assert!(is_near_duplicate("prefers pnpm over npm", "prefers pnpm over npm", 0.8));
        assert!((similarity(&signature("abc def ghij"), &signature("abc def ghij")) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn minor_edits_stay_dup() {
        // typo / punctuation / casing — token dedup might miss these, MinHash catches them
        assert!(is_near_duplicate("prefers pnpm over npm", "Prefers pnpm over npm.", 0.7));
    }

    #[test]
    fn different_facts_not_dup() {
        assert!(!is_near_duplicate("prefers pnpm over npm", "deploys on fridays only", 0.7));
        assert!(!is_near_duplicate("reply in vietnamese", "use tabs not spaces", 0.7));
    }

    #[test]
    fn empty_is_not_dup() {
        assert!(!is_near_duplicate("", "anything", 0.5));
        assert_eq!(signature("").len(), 0);
    }
}
