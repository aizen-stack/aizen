//! Dense semantic tier (P5) — the EMBEDDER SEAM + a pure-Rust cache + fusion-ready cosine.
//!
//! The retrieval design is hybrid: a lexical floor (always on, pure Rust, nails literal
//! tokens) fused with a dense tier (catches paraphrase). This module defines the embedder
//! trait everything else fuses against, plus a content-hash cache so each fact is embedded
//! once. A real semantic backend (model2vec-rs / Candle bge-small) plugs in behind a cargo
//! feature — see the dependency note below — WITHOUT changing any caller.
//!
//! ## Backend status (owner decision, plan risk #1 — RESOLVED P6)
//! The default build stays a **pure-Rust single static binary** and ships the lexical floor;
//! the real model2vec backend lands behind the opt-in `dense` feature. The no-C-dep risk is
//! resolved: `tokenizers` has a `compile_error!` requiring `onig` OR `fancy-regex`, and
//! model2vec-rs's own `fancy-regex`/`onig` features BOTH hard-wire `tokenizers/esaxx_fast`
//! (→ `esaxx-rs/cpp` → a C++ `cc` build step). We sidestep that by depending on `tokenizers`
//! DIRECTLY with only its pure-Rust `fancy-regex` backend (no `esaxx_fast`); Cargo feature
//! unification then satisfies the compile-gate WITHOUT the C++ step. `cargo tree` confirms
//! `esaxx-rs/cpp` is off and the only `cc` build-dep left is `ring` (already in the default
//! build via rustls) — so `--features dense` adds NO new C/C++ toolchain requirement.
//! `HashEmbedder` below is a deterministic, dependency-free embedder that exercises the full
//! hybrid pipeline (fusion, cache, bench, graceful fallback) in tests — it is NOT semantic and
//! will not beat the lexical floor on paraphrase; that's what the real backend is for.

use crate::core::config;
use crate::memory::tokenize::tokenize;
use std::collections::HashMap;
use std::path::PathBuf;

/// Anything that turns text into a fixed-dim vector. The hybrid retriever only ever sees
/// this trait, so the backend (hashing / model2vec / Candle) is swappable.
pub trait Embedder {
    /// A stable id (model name + dim) — used to namespace the on-disk cache so swapping
    /// backends never mixes incompatible vectors.
    fn id(&self) -> String;
    /// Vector dimensionality — reported by real backends + used by the `dense` feature; the default
    /// build never queries it, so allow it to go unread there.
    #[allow(dead_code)]
    fn dim(&self) -> usize;
    /// Embed one string. Implementations should L2-normalize so cosine == dot product.
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// Cosine similarity of two equal-length vectors (0 if degenerate).
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

fn fnv1a64(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in s.as_bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

/// A deterministic, dependency-free hashing embedder (token uni/bi-grams → fixed dims,
/// L2-normalized). Proves the hybrid plumbing end to end; it is lexical-ish, NOT semantic.
pub struct HashEmbedder {
    dim: usize,
}

impl HashEmbedder {
    pub fn new(dim: usize) -> Self {
        HashEmbedder { dim: dim.max(8) }
    }
}

impl Default for HashEmbedder {
    fn default() -> Self {
        HashEmbedder::new(256)
    }
}

impl Embedder for HashEmbedder {
    fn id(&self) -> String {
        format!("hash-{}", self.dim)
    }
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        let toks = tokenize(text);
        let mut v = vec![0f32; self.dim];
        let mut bump = |feat: &str| {
            let h = fnv1a64(feat);
            let idx = (h % self.dim as u64) as usize;
            // sign hashing to reduce collisions cancelling out
            let sign = if (h >> 63) & 1 == 1 { 1.0 } else { -1.0 };
            v[idx] += sign;
        };
        for w in &toks {
            bump(w);
        }
        for pair in toks.windows(2) {
            bump(&format!("{}_{}", pair[0], pair[1]));
        }
        let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        if norm > 0.0 {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }
}

/// Content-hash → vector cache (namespaced by embedder id), persisted as JSON so each fact
/// is embedded once across runs. Best-effort: a load/save failure degrades to recompute.
pub struct EmbeddingCache {
    embedder_id: String,
    map: HashMap<u64, Vec<f32>>,
    dirty: bool,
}

impl EmbeddingCache {
    fn path_for(id: &str) -> PathBuf {
        config::embed_cache_dir().join(format!("{id}.json"))
    }

    /// Load the cache for `embedder_id` (empty if absent/corrupt).
    pub fn load(embedder_id: &str) -> Self {
        let map = std::fs::read_to_string(Self::path_for(embedder_id))
            .ok()
            .and_then(|s| serde_json::from_str::<HashMap<u64, Vec<f32>>>(&s).ok())
            .unwrap_or_default();
        EmbeddingCache {
            embedder_id: embedder_id.to_string(),
            map,
            dirty: false,
        }
    }

    /// Embedding for `text`, computing+caching on miss.
    pub fn get_or_compute(&mut self, text: &str, e: &dyn Embedder) -> Vec<f32> {
        let key = fnv1a64(text);
        if let Some(v) = self.map.get(&key) {
            return v.clone();
        }
        let v = e.embed(text);
        self.map.insert(key, v.clone());
        self.dirty = true;
        v
    }

    /// Persist if changed (best-effort).
    pub fn save(&self) {
        if !self.dirty {
            return;
        }
        let path = Self::path_for(&self.embedder_id);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(json) = serde_json::to_string(&self.map) {
            let _ = std::fs::write(path, json);
        }
    }
}

/// The real semantic embedder (model2vec static embeddings) — present only with the
/// `dense` cargo feature. Loads a locally-downloaded model from `~/.nextgen/models/<name>`.
#[cfg(feature = "dense")]
pub mod model2vec {
    use super::Embedder;
    use crate::core::config;
    use anyhow::{Context, Result};
    use model2vec_rs::model::StaticModel;

    pub struct Model2VecEmbedder {
        model: StaticModel,
        name: String,
        dim: usize,
    }

    impl Model2VecEmbedder {
        /// Load the configured model from the local model dir (no network — `local-only`).
        pub fn load_default() -> Result<Self> {
            let name = config::embed_model_name();
            let dir = config::models_dir().join(&name);
            if !dir.join("config.json").exists() {
                anyhow::bail!(
                    "dense model '{}' not found at {} — download it first",
                    name,
                    dir.display()
                );
            }
            let path = dir.to_str().context("model path is not valid UTF-8")?;
            // normalize=true so cosine == dot product, matching the cache/fusion assumptions.
            let model = StaticModel::from_pretrained(path, None, Some(true), None)
                .map_err(|e| anyhow::anyhow!("loading model2vec model at {path}: {e}"))?;
            let dim = model
                .encode(&["dim probe".to_string()])
                .into_iter()
                .next()
                .map(|v| v.len())
                .unwrap_or(0);
            Ok(Self { model, name, dim })
        }
    }

    impl Embedder for Model2VecEmbedder {
        fn id(&self) -> String {
            format!("model2vec-{}", self.name)
        }
        fn dim(&self) -> usize {
            self.dim
        }
        fn embed(&self, text: &str) -> Vec<f32> {
            self.model
                .encode(&[text.to_string()])
                .into_iter()
                .next()
                .unwrap_or_default()
        }
    }
}

/// The embedder used by the hybrid path / `bench --hybrid`. With the `dense` feature it loads
/// the real model2vec model (falling back to the hashing embedder if the model is missing);
/// without the feature it is the pure-Rust hashing embedder.
#[cfg(feature = "dense")]
pub fn default_dense_embedder() -> Box<dyn Embedder> {
    match model2vec::Model2VecEmbedder::load_default() {
        Ok(e) => {
            eprintln!("[dense] loaded {} (dim {})", e.id(), e.dim());
            Box::new(e)
        }
        Err(err) => {
            eprintln!("[dense] {err}; falling back to the hashing embedder");
            Box::new(HashEmbedder::default())
        }
    }
}

#[cfg(not(feature = "dense"))]
pub fn default_dense_embedder() -> Box<dyn Embedder> {
    Box::new(HashEmbedder::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cosine_self_is_one() {
        let e = HashEmbedder::default();
        let v = e.embed("prefers pnpm over npm");
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn related_text_more_similar_than_unrelated() {
        let e = HashEmbedder::default();
        let a = e.embed("prefers pnpm over npm for packages");
        let b = e.embed("prefers pnpm over npm always"); // shares tokens
        let c = e.embed("deploys to production on fridays"); // unrelated
        assert!(cosine(&a, &b) > cosine(&a, &c));
    }

    #[test]
    fn dim_is_respected() {
        let e = HashEmbedder::new(128);
        assert_eq!(e.embed("anything at all").len(), 128);
        assert_eq!(e.dim(), 128);
    }
}
