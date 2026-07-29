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
use std::path::{Path, PathBuf};

// ── local model discovery (pure FS — feature-agnostic, so the default build + tests can run it) ──
//
// The dense tier used to look at exactly ONE path: `models_dir()/<configured name>`. If that dir
// was absent it bailed, even when a perfectly good model2vec model was sitting right next to it
// (e.g. a user who downloaded `potion-multilingual-128M` but whose default is `potion-base-8M`).
// This block makes the loader look AROUND instead of giving up: the configured name is still the
// first choice, but when it's missing we scan `~/.aizen/models/` AND the Hugging Face hub cache
// for any directory that is genuinely a model2vec StaticModel, and use the best one — preferring
// models we've benched well, then any `potion-*`, then anything else model2vec-shaped.
//
// A directory is "model2vec-shaped" only when its `config.json` carries `model_type:"model2vec"`
// AND `architectures:["StaticModel"]`. That signature is what tells a real model2vec model apart
// from a sentence-transformers model that happens to also live in the HF cache (e.g.
// `all-MiniLM-L6-v2`, whose `model_type` is `bert` and would fail `from_bytes` mid-load with a
// missing-`embeddings`-tensor error). Reading the config first means we never hand the loader a
// model that will crash.

/// Where a discovered model came from — surfaced in the `[dense]` note so the user knows it was
/// auto-detected, not loaded from the configured name, and can see WHICH dir aizen chose.
#[derive(Debug, Clone)]
pub struct ModelCandidate {
    pub name: String,
    pub dir: PathBuf,
    /// "aizen" = under `~/.aizen/models/<name>/`; "hf" = under the HF hub cache snapshot dir.
    pub source: &'static str,
}

/// Models we've benchmarked (P6 dense-bench), best first. A discovered candidate matching one of
/// these wins over a same-tier arbitrary name — the bench is the only evidence we have that a
/// model actually helps recall, so a known-good name outranks an unknown one of equal recency.
const PREFERRED: &[&str] = &["potion-base-8M", "potion-multilingual-128M"];

/// Find a usable model2vec model on this machine, without the network.
///
/// Order of preference:
/// 0. `AIZEN_EMBED_MODEL` as an ABSOLUTE PATH to a model dir — an escape hatch for a model that
///    lives outside both trees below (a shared drive, a build artifact, a manual clone).
/// 1. The configured name (`config::embed_model_name()`) at `models_dir()/<name>/` — the user's
///    explicit choice always wins when present.
/// 2. Any model2vec model under `~/.aizen/models/` — the aizen-managed dir.
/// 3. Any model2vec model under the Hugging Face hub cache — models other tooling downloaded.
///
/// Within steps 2–3, a name in [`PREFERRED`] outranks one not in it, and `potion-*` outranks other
/// families; ties break by directory name so the result is deterministic. Returns `None` only when
/// no model2vec-shaped directory exists anywhere we looked.
///
/// This is pure FS work (a few `read_dir`s and small `config.json` reads), deliberately NOT cached:
/// a model downloaded mid-session must become visible without restarting. The expensive half —
/// loading the weights — is what gets memoized (see `model2vec::load_shared`).
pub fn discover_local_model() -> Option<ModelCandidate> {
    let preferred_name = config::embed_model_name();

    // 0. An absolute path in AIZEN_EMBED_MODEL points AT the model dir rather than naming a subdir
    //    of `models_dir()`. Checked first: an explicit path is the least ambiguous thing a user can
    //    say, and joining it onto `models_dir()` (step 1) would produce nonsense on Windows anyway.
    let as_path = Path::new(&preferred_name);
    if as_path.is_absolute() {
        if is_model2vec_dir(as_path) {
            return Some(ModelCandidate {
                // The dir name is the model's identity for cache-namespacing purposes; fall back to
                // the whole string if the path somehow has no final component.
                name: as_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&preferred_name)
                    .to_string(),
                dir: as_path.to_path_buf(),
                source: "path",
            });
        }
        // A path that isn't a model is a dead end, not a name to look up in the trees below.
        return None;
    }

    // 1. The configured name, verbatim. This is the only step that honors AIZEN_EMBED_MODEL exactly.
    let aizen_dir = config::models_dir();
    let exact = aizen_dir.join(&preferred_name);
    if is_model2vec_dir(&exact) {
        return Some(ModelCandidate {
            name: preferred_name,
            dir: exact,
            source: "aizen",
        });
    }

    // 2–3. Scan the aizen models dir, then the HF hub cache, and take the best-ranked candidate.
    list_local_models().into_iter().next()
}

/// Every model2vec model this machine has, best-ranked first — the listing behind
/// `aizen memory model-list`, and the candidate pool [`discover_local_model`] picks from.
///
/// Ranking: a name in [`PREFERRED`] first (by its position there), then any other `potion-*`, then
/// anything else model2vec-shaped; ties break alphabetically so the order is deterministic. The
/// aizen-managed dir is scanned before the HF cache, so an aizen copy wins a same-name tie.
pub fn list_local_models() -> Vec<ModelCandidate> {
    let mut all = scan_dir_for_model2vec(&config::models_dir(), "aizen");
    all.append(&mut scan_hf_cache("hf"));
    all.sort_by(|a, b| {
        tier_of(&a.name)
            .cmp(&tier_of(&b.name))
            .then_with(|| a.name.cmp(&b.name))
    });
    all
}

/// Lower tier number = more preferred. 0 = a name in PREFERRED (by its position, so the FIRST
/// listed preferred name beats the second); 1 = any other `potion-*`; 2 = anything else.
fn tier_of(name: &str) -> u8 {
    if let Some(idx) = PREFERRED.iter().position(|p| *p == name) {
        return idx as u8;
    }
    if name.starts_with("potion") {
        return 1;
    }
    2
}

/// Is `dir` a loadable model2vec StaticModel? Requires the three files `from_pretrained` reads AND
/// a `config.json` whose `model_type` is `model2vec` with a `StaticModel` architecture. The config
/// check is what filters out non-model2vec HF cache entries (bert/sentence-transformers) that would
/// otherwise be selected by filename alone and then fail mid-load.
pub fn is_model2vec_dir(dir: &Path) -> bool {
    let config = dir.join("config.json");
    let tokenizer = dir.join("tokenizer.json");
    let model = dir.join("model.safetensors");
    if !(config.exists() && tokenizer.exists() && model.exists()) {
        return false;
    }
    let Ok(bytes) = std::fs::read(&config) else {
        return false;
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    let mt = v.get("model_type").and_then(|t| t.as_str());
    if mt != Some("model2vec") {
        return false;
    }
    // `architectures` is a JSON array; "StaticModel" must be among its entries.
    let archs = v.get("architectures").and_then(|a| a.as_array());
    archs.is_some_and(|arr| arr.iter().any(|a| a.as_str() == Some("StaticModel")))
}

/// Scan one root dir for immediate children that are model2vec models. Each child is `<root>/<name>/`
/// with the three files inside. Returns candidates tagged with `source`.
fn scan_dir_for_model2vec(root: &Path, source: &'static str) -> Vec<ModelCandidate> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(root) else {
        return out;
    };
    for e in rd.flatten() {
        let path = e.path();
        if !path.is_dir() {
            continue;
        }
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
        else {
            continue;
        };
        if is_model2vec_dir(&path) {
            out.push(ModelCandidate {
                name,
                dir: path,
                source,
            });
        }
    }
    out
}

/// Scan the Hugging Face hub cache for model2vec snapshots. Layout:
/// `<root>/models--<org>--<name>/snapshots/<sha>/` with the files directly inside the snapshot dir.
///
/// Root precedence mirrors what `huggingface_hub` itself documents, highest first:
/// `HF_HUB_CACHE` → `HF_HOME/hub` → `XDG_CACHE_HOME/huggingface/hub` → `<home>/.cache/huggingface/hub`.
/// Note that the default is `~/.cache/huggingface/hub` on ALL THREE platforms — `huggingface_hub`
/// does not use `%LOCALAPPDATA%` on Windows, so the previous `$LOCALAPPDATA/huggingface/hub` entry
/// pointed at a directory nothing ever creates.
///
/// The home lookup is `USERPROFILE` then `HOME`, matching `config::aizen_home()`. Order matters:
/// `HOME` is unset on Windows outside a POSIX shell, so a `HOME`-only lookup meant this whole scan
/// was dead there — a user's real cache at `C:\Users\<u>\.cache\huggingface\hub` was never seen.
fn scan_hf_cache(source: &'static str) -> Vec<ModelCandidate> {
    /// A non-empty, trimmed env var as a path.
    fn env_path(key: &str) -> Option<PathBuf> {
        let v = std::env::var(key).ok()?;
        let v = v.trim();
        (!v.is_empty()).then(|| PathBuf::from(v))
    }

    let mut roots = Vec::new();
    // Highest precedence first — HF_HUB_CACHE names the hub dir itself.
    if let Some(c) = env_path("HF_HUB_CACHE") {
        roots.push(c);
    }
    if let Some(h) = env_path("HF_HOME") {
        roots.push(h.join("hub"));
    }
    if let Some(x) = env_path("XDG_CACHE_HOME") {
        roots.push(x.join("huggingface/hub"));
    }
    // The cross-platform default. USERPROFILE covers Windows; HOME covers unix (and Git Bash).
    if let Some(home) = env_path("USERPROFILE").or_else(|| env_path("HOME")) {
        roots.push(home.join(".cache/huggingface/hub"));
    }
    let mut out = Vec::new();
    for root in roots {
        let Ok(rd) = std::fs::read_dir(&root) else {
            continue;
        };
        for repo in rd.flatten() {
            let repo_dir = repo.path();
            // Repo dirs are named `models--<org>--<name>`. The human name is the part after the
            // second `--`; fall back to the whole stem if that parse fails.
            let stem = repo_dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let name = stem
                .strip_prefix("models--")
                .and_then(|s| s.split("--").last())
                .unwrap_or(stem)
                .to_string();
            // Each repo has a `snapshots/<sha>/` subdir holding the real files.
            let snapshots = repo_dir.join("snapshots");
            let Ok(srd) = std::fs::read_dir(&snapshots) else {
                continue;
            };
            for snap in srd.flatten() {
                let snap_dir = snap.path();
                if is_model2vec_dir(&snap_dir) {
                    out.push(ModelCandidate {
                        name: name.clone(),
                        dir: snap_dir,
                        source,
                    });
                }
            }
        }
    }
    out
}

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
/// `dense` cargo feature. Loads a locally-downloaded model from `~/.aizen/models/<name>`.
#[cfg(feature = "dense")]
pub mod model2vec {
    use super::{Embedder, ModelCandidate};
    use crate::core::config;
    use anyhow::{Context, Result};
    use model2vec_rs::model::StaticModel;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex, OnceLock};

    pub struct Model2VecEmbedder {
        /// Shared with every other embedder built from the same weights file this process — see
        /// [`load_shared`]. `StaticModel` is read-only once built, so sharing it is free.
        model: Arc<StaticModel>,
        name: String,
        dim: usize,
        /// "path" / "aizen" / "hf" — where the model was loaded from, so the note can say it was
        /// auto-detected (vs. the configured name) and from which dir tree.
        source: &'static str,
    }

    /// Identity of a loaded model: which weights file, as of which version of it. `mtime`+`len` mean
    /// a model REPLACED in place (a re-download, a different snapshot moved into the same dir) is a
    /// different key and gets reloaded, rather than silently serving the old weights forever.
    type LoadKey = (PathBuf, Option<std::time::SystemTime>, u64);

    fn load_key(dir: &Path) -> LoadKey {
        let weights = dir.join("model.safetensors");
        let md = std::fs::metadata(&weights).ok();
        (
            dir.to_path_buf(),
            md.as_ref().and_then(|m| m.modified().ok()),
            md.map(|m| m.len()).unwrap_or(0),
        )
    }

    /// Load `cand`'s weights, reusing an already-loaded copy when this process has one for the same
    /// [`LoadKey`].
    ///
    /// Why this exists: `StaticModel::from_pretrained` `fs::read`s the ENTIRE `model.safetensors`
    /// (512 MB for `potion-multilingual-128M`) and parses it. `default_dense_embedder` is called per
    /// search — memory retrieval AND `codebase::fuse_dense` — so without memoization every turn paid
    /// that twice. The OS page cache hid most of the I/O, but the parse and the resident copy were
    /// real.
    ///
    /// Only the LOAD is cached. `discover_local_model()` still runs fresh each call, so a model
    /// downloaded mid-session is still picked up: it produces a new key, which misses this cache.
    ///
    /// One entry only. Holding every model ever loaded would pin hundreds of MB per model for a
    /// switch that happens at most once or twice in a session; the common case is the same key every
    /// time, and a genuine switch just pays one reload.
    fn load_shared(cand: &ModelCandidate) -> Result<(Arc<StaticModel>, usize)> {
        static CACHE: OnceLock<Mutex<Option<(LoadKey, Arc<StaticModel>, usize)>>> = OnceLock::new();
        let cache = CACHE.get_or_init(|| Mutex::new(None));
        let key = load_key(&cand.dir);

        // Held across the load deliberately: two threads racing the same cold model should have one
        // of them wait and reuse, not both read 512 MB and one throw its copy away.
        let mut slot = cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((k, m, d)) = slot.as_ref() {
            if *k == key {
                return Ok((Arc::clone(m), *d));
            }
        }

        let path = cand.dir.to_str().context("model path is not valid UTF-8")?;
        // normalize=true so cosine == dot product, matching the cache/fusion assumptions.
        let model = StaticModel::from_pretrained(path, None, Some(true), None)
            .map_err(|e| anyhow::anyhow!("loading model2vec model at {path}: {e}"))?;
        let dim = model
            .encode(&["dim probe".to_string()])
            .into_iter()
            .next()
            .map(|v| v.len())
            .unwrap_or(0);
        let model = Arc::new(model);
        *slot = Some((key, Arc::clone(&model), dim));
        Ok((model, dim))
    }

    impl Model2VecEmbedder {
        /// Load a usable model2vec model from this machine — the configured name first, then any
        /// model2vec model found under `~/.aizen/models/` or the HF hub cache (see
        /// [`super::discover_local_model`]). No network at retrieval time (`local-only`).
        pub fn load_default() -> Result<Self> {
            let Some(cand) = super::discover_local_model() else {
                anyhow::bail!(
                    "dense model '{}' not found, and no model2vec model in ~/.aizen/models or the HF cache — run `aizen memory model-download`",
                    config::embed_model_name()
                );
            };
            let (model, dim) = load_shared(&cand)?;
            Ok(Self {
                model,
                name: cand.name,
                dim,
                source: cand.source,
            })
        }
    }

    impl Model2VecEmbedder {
        /// Where the loaded model came from (`"aizen"` / `"hf"`), for the boot note.
        #[allow(dead_code)] // read by `default_dense_embedder`'s note below
        pub fn source(&self) -> &'static str {
            self.source
        }
        /// The model's directory name — compared against the configured name so the note can say
        /// whether this was an exact match or an auto-detected substitute.
        #[allow(dead_code)]
        pub fn name(&self) -> &str {
            &self.name
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
    let configured = config::embed_model_name();
    match model2vec::Model2VecEmbedder::load_default() {
        Ok(e) => {
            // Distinguish "loaded the exact name you asked for" from "loaded something we found" —
            // the latter is the auto-detect path, and the user deserves to know WHICH model won so
            // a surprising pick is traceable rather than silent.
            let note = if e.name() == configured {
                format!("[dense] loaded {} (dim {})", e.id(), e.dim())
            } else {
                format!(
                    "[dense] loaded {} (dim {}) — auto-detected from {} (configured '{}' not present; set AIZEN_EMBED_MODEL to pin)",
                    e.id(),
                    e.dim(),
                    e.source(),
                    configured
                )
            };
            note_once(&note);
            Box::new(e)
        }
        Err(err) => {
            note_once(&format!(
                "[dense] {err}; falling back to the hashing embedder"
            ));
            Box::new(HashEmbedder::default())
        }
    }
}

/// Report an embedder-backend note AT MOST ONCE per distinct message per process, through the TUI
/// funnel.
///
/// Two separate bugs are being fixed here, and both matter:
///
/// 1. **Never print raw.** This runs deep inside retrieval, which on a `--features dense` build
///    happens on EVERY turn (`settings().enable_dense` mirrors the cargo feature) and also from
///    `codebase.rs`. A raw `eprintln!` writes straight into the terminal while the retained render
///    thread believes it owns every cell, so ratatui's diff leaves the injected text embedded in
///    later frames — that is exactly the interleaved/doubled transcript reported against 0.5.0.
/// 2. **Say it once.** Even routed correctly, a per-turn "model not found" line is noise: the
///    condition is static for the life of the process (a missing model dir does not appear
///    mid-session). Keyed by message, so a genuine change of outcome still gets reported.
#[cfg(feature = "dense")]
fn note_once(msg: &str) {
    if note_is_fresh(msg) {
        crate::ui::tui::note_line(msg);
    }
}

/// The dedupe decision behind [`note_once`], split out so it is compiled — and unit-tested — in the
/// DEFAULT build too. The bug it guards (a per-turn note repeated forever) only reproduces on a
/// `--features dense` build, which is exactly the configuration CI does not run tests in; keeping the
/// registry here means the "say it once" contract is covered by the ordinary test run.
///
/// Returns true the first time it sees `msg` and false for every repeat, for the life of the process.
#[allow(dead_code)] // only the `dense` cfg calls it outside tests
fn note_is_fresh(msg: &str) -> bool {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static SEEN: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    let seen = SEEN.get_or_init(|| Mutex::new(HashSet::new()));
    let mut g = seen.lock().unwrap_or_else(|e| e.into_inner());
    g.insert(msg.to_string())
}

#[cfg(not(feature = "dense"))]
pub fn default_dense_embedder() -> Box<dyn Embedder> {
    Box::new(HashEmbedder::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every env var that can steer model discovery to a directory on the real machine. `pin` sets
    /// the first two at temp dirs and CLEARS the rest.
    ///
    /// Clearing matters and is easy to get wrong: `scan_hf_cache` scans EVERY root it can name, not
    /// just the highest-precedence one, so pinning `HF_HUB_CACHE` alone still let `USERPROFILE` drag
    /// the real `~/.cache/huggingface/hub` into a test that asserts "nothing found". Today that
    /// happens to hold only a non-model2vec model, so such a test would pass by luck and fail the
    /// day a real model2vec model got cached there.
    const DISCOVERY_ENV: &[&str] = &[
        "AIZEN_HOME",
        "HF_HUB_CACHE",
        "HF_HOME",
        "XDG_CACHE_HOME",
        "USERPROFILE",
        "HOME",
        "AIZEN_EMBED_MODEL",
    ];

    /// A hermetic env for one discovery test: temp `AIZEN_HOME` + `HF_HUB_CACHE`, every other
    /// discovery input cleared, all of it restored on Drop so a panicking test can't leak. Serialized
    /// on `TEST_HOME_LOCK` because the env is process-global.
    struct IsolatedModelEnv {
        _guard: std::sync::MutexGuard<'static, ()>,
        saved: Vec<(&'static str, Option<String>)>,
    }

    impl IsolatedModelEnv {
        fn pin(aizen_home: &Path, hf_cache: &Path) -> Self {
            let guard = crate::core::config::TEST_HOME_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let saved: Vec<(&'static str, Option<String>)> = DISCOVERY_ENV
                .iter()
                .map(|k| (*k, std::env::var(k).ok()))
                .collect();
            for k in DISCOVERY_ENV {
                std::env::remove_var(k);
            }
            std::env::set_var("AIZEN_HOME", aizen_home);
            std::env::set_var("HF_HUB_CACHE", hf_cache);
            Self {
                _guard: guard,
                saved,
            }
        }

        /// Point `AIZEN_EMBED_MODEL` at an absolute path (the step-0 escape hatch). Restored with the
        /// rest on Drop.
        fn set_embed_model(&self, value: &Path) {
            std::env::set_var("AIZEN_EMBED_MODEL", value);
        }
    }

    impl Drop for IsolatedModelEnv {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(v) => std::env::set_var(k, v),
                    None => std::env::remove_var(k),
                }
            }
        }
    }

    /// Write a fake model2vec dir: the three files `from_pretrained` would read, plus a
    /// `config.json` carrying the model2vec signature. The safetensors + tokenizer contents don't
    /// matter for discovery (we only check existence + config) — keep them tiny.
    fn write_model2vec_dir(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("config.json"),
            serde_json::json!({
                "model_type": "model2vec",
                "architectures": ["StaticModel"],
                "normalize": true
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(dir.join("tokenizer.json"), b"{}").unwrap();
        std::fs::write(dir.join("model.safetensors"), b"\x00").unwrap();
        let _ = name; // name is the dir's own stem; nothing extra to write
    }

    /// Write a NON-model2vec dir (e.g. a sentence-transformers / bert model in the HF cache) so the
    /// filter has something to reject. Same files present, but `model_type` is wrong.
    fn write_non_model2vec_dir(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("config.json"),
            serde_json::json!({
                "model_type": "bert",
                "architectures": ["BertModel"]
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(dir.join("tokenizer.json"), b"{}").unwrap();
        std::fs::write(dir.join("model.safetensors"), b"\x00").unwrap();
    }

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

    // ── model discovery ──────────────────────────────────────────────────────

    #[test]
    fn discover_picks_configured_name_when_present() {
        let tmp = std::env::temp_dir().join(format!("aizen-disc-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let aizen = tmp.join("aizen");
        let hf = tmp.join("hf");
        let models = aizen.join("models"); // models_dir() = AIZEN_HOME/models
                                           // Configured default is potion-base-8M; put it AND a less-preferred model in the aizen dir.
        write_model2vec_dir(&models.join("potion-base-8M"), "potion-base-8M");
        write_model2vec_dir(&models.join("zzz-other"), "zzz-other");
        let _env = IsolatedModelEnv::pin(&aizen, &hf);
        let cand = discover_local_model().expect("configured model should win");
        assert_eq!(cand.name, "potion-base-8M");
        assert_eq!(cand.source, "aizen");
    }

    #[test]
    fn discover_falls_back_to_a_neighbor_when_configured_missing() {
        let tmp = std::env::temp_dir().join(format!("aizen-disc-nb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let aizen = tmp.join("aizen");
        let hf = tmp.join("hf");
        let models = aizen.join("models");
        // No potion-base-8M; only potion-multilingual-128M (a PREFERRED name) and a random one.
        write_model2vec_dir(
            &models.join("potion-multilingual-128M"),
            "potion-multilingual-128M",
        );
        write_model2vec_dir(&models.join("random-model2vec"), "random-model2vec");
        let _env = IsolatedModelEnv::pin(&aizen, &hf);
        let cand = discover_local_model().expect("a neighbor should be found");
        // PREFERRED (tier 0/1) beats arbitrary (tier 2), so the multilingual model wins even
        // though its name sorts AFTER the random one alphabetically.
        assert_eq!(cand.name, "potion-multilingual-128M");
        assert_eq!(cand.source, "aizen");
    }

    #[test]
    fn discover_skips_non_model2vec_dirs() {
        let tmp = std::env::temp_dir().join(format!("aizen-disc-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let aizen = tmp.join("aizen");
        let hf = tmp.join("hf");
        let models = aizen.join("models");
        // A bert model (wrong signature) + one real model2vec. Only the real one is usable.
        write_non_model2vec_dir(&models.join("all-MiniLM-L6-v2"));
        write_model2vec_dir(&models.join("potion-base-8M"), "potion-base-8M");
        let _env = IsolatedModelEnv::pin(&aizen, &hf);
        let cand = discover_local_model().expect("real model present");
        assert_eq!(cand.name, "potion-base-8M");
        assert!(
            !is_model2vec_dir(&models.join("all-MiniLM-L6-v2")),
            "bert dir must be rejected by the signature check"
        );
    }

    #[test]
    fn discover_finds_model_in_hf_cache_snapshot() {
        let tmp = std::env::temp_dir().join(format!("aizen-disc-hf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let aizen = tmp.join("aizen");
        let hf = tmp.join("hf");
        // Empty aizen dir; a model2vec model sits in the HF cache under the standard layout.
        std::fs::create_dir_all(&aizen).unwrap();
        let snap = hf.join("models--minishlab--potion-base-8M/snapshots/abc123");
        write_model2vec_dir(&snap, "potion-base-8M");
        let _env = IsolatedModelEnv::pin(&aizen, &hf);
        let cand = discover_local_model().expect("HF cache model should be found");
        assert_eq!(cand.name, "potion-base-8M");
        assert_eq!(cand.source, "hf");
        assert!(
            cand.dir.ends_with("abc123"),
            "should resolve to the snapshot dir"
        );
    }

    #[test]
    fn discover_returns_none_when_nothing_usable_anywhere() {
        let tmp = std::env::temp_dir().join(format!("aizen-disc-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let aizen = tmp.join("aizen");
        let hf = tmp.join("hf");
        std::fs::create_dir_all(&aizen).unwrap();
        std::fs::create_dir_all(&hf).unwrap();
        let _env = IsolatedModelEnv::pin(&aizen, &hf);
        assert!(discover_local_model().is_none());
    }

    #[test]
    fn absolute_path_in_embed_model_is_used_directly() {
        // Step 0: a model that lives in NEITHER tree (a shared drive, a build artifact) is reachable
        // by pointing AIZEN_EMBED_MODEL at it. Joining an absolute path onto `models_dir()` would
        // produce nonsense, so it has to be recognized as a path rather than a name.
        let tmp = std::env::temp_dir().join(format!("aizen-disc-abs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let aizen = tmp.join("aizen");
        let hf = tmp.join("hf");
        let elsewhere = tmp.join("elsewhere/my-model");
        std::fs::create_dir_all(&aizen).unwrap();
        // A model in the managed dir too, to prove the explicit path OUTRANKS the normal scan.
        write_model2vec_dir(&aizen.join("models/potion-base-8M"), "potion-base-8M");
        write_model2vec_dir(&elsewhere, "my-model");
        let env = IsolatedModelEnv::pin(&aizen, &hf);
        env.set_embed_model(&elsewhere);
        let cand = discover_local_model().expect("absolute path should resolve");
        assert_eq!(cand.name, "my-model", "dir name is the model identity");
        assert_eq!(cand.source, "path");
        assert_eq!(cand.dir, elsewhere);
    }

    #[test]
    fn absolute_path_that_is_not_a_model_finds_nothing() {
        // A path is an assertion about WHERE the model is. If it's wrong, silently falling back to a
        // scan would load a different model than the one named — so this is a dead end, and the
        // resulting `[dense]` note is what tells the user their path is bad.
        let tmp = std::env::temp_dir().join(format!("aizen-disc-abs-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let aizen = tmp.join("aizen");
        let hf = tmp.join("hf");
        std::fs::create_dir_all(&aizen).unwrap();
        // A perfectly good model in the managed dir that must NOT be substituted in.
        write_model2vec_dir(&aizen.join("models/potion-base-8M"), "potion-base-8M");
        let env = IsolatedModelEnv::pin(&aizen, &hf);
        env.set_embed_model(&tmp.join("no-such-model"));
        assert!(discover_local_model().is_none());
    }

    #[test]
    fn hf_cache_is_found_via_each_documented_root() {
        // The Windows hole this closes: `scan_hf_cache` used to reach the default cache ONLY through
        // `HOME`, which is unset on Windows outside a POSIX shell — so a model at
        // `C:\Users\<u>\.cache\huggingface\hub` was never seen. Each root is exercised alone, with all
        // others cleared, so a regression in any one of them fails rather than being masked.
        for (idx, key) in ["HF_HOME", "XDG_CACHE_HOME", "USERPROFILE", "HOME"]
            .iter()
            .enumerate()
        {
            let tmp =
                std::env::temp_dir().join(format!("aizen-disc-root-{}-{idx}", std::process::id()));
            let _ = std::fs::remove_dir_all(&tmp);
            let aizen = tmp.join("aizen");
            std::fs::create_dir_all(&aizen).unwrap();
            let base = tmp.join("base");
            // Where each env var implies the hub dir lives.
            let hub = match *key {
                "HF_HOME" => base.join("hub"),
                "XDG_CACHE_HOME" => base.join("huggingface/hub"),
                _ => base.join(".cache/huggingface/hub"),
            };
            write_model2vec_dir(
                &hub.join("models--minishlab--potion-base-8M/snapshots/deadbeef"),
                "potion-base-8M",
            );
            // pin() clears every discovery var including HF_HUB_CACHE; set only the one under test.
            let _env = IsolatedModelEnv::pin(&aizen, &tmp.join("unused-hub"));
            std::env::set_var(key, &base);
            let cand = discover_local_model();
            std::env::remove_var(key);
            let cand = cand.unwrap_or_else(|| panic!("{key} root should be scanned"));
            assert_eq!(cand.name, "potion-base-8M");
            assert_eq!(cand.source, "hf");
        }
    }

    #[test]
    fn is_model2vec_dir_requires_the_signature() {
        let tmp = std::env::temp_dir().join(format!("aizen-disc-sig-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let good = tmp.join("good");
        let bad_sig = tmp.join("bad-sig");
        let missing = tmp.join("missing");
        write_model2vec_dir(&good, "good");
        write_non_model2vec_dir(&bad_sig);
        std::fs::create_dir_all(&missing).unwrap(); // no files at all
        assert!(is_model2vec_dir(&good));
        assert!(
            !is_model2vec_dir(&bad_sig),
            "wrong model_type must be rejected"
        );
        assert!(
            !is_model2vec_dir(&missing),
            "missing files must be rejected"
        );
    }

    #[test]
    fn dim_is_respected() {
        let e = HashEmbedder::new(128);
        assert_eq!(e.embed("anything at all").len(), 128);
        assert_eq!(e.dim(), 128);
    }

    #[test]
    fn backend_note_is_reported_once_per_distinct_message() {
        // The 0.5.0 UI report: on a `--features dense` build the "model not found" note fired from
        // retrieval on EVERY turn. Routing it through the TUI funnel stopped it corrupting the frame;
        // this guards the second half — it must be said ONCE, because the condition is static for the
        // life of the process. Distinct messages are still independent, so a genuine change of outcome
        // (fallback → loaded) is not swallowed. Unique strings keep the process-global registry from
        // coupling this test to any other.
        let miss = "[dense] model 'probe-a' not found; falling back";
        let load = "[dense] loaded probe-b (dim 256)";
        assert!(note_is_fresh(miss), "first sighting must report");
        assert!(!note_is_fresh(miss), "repeat must be suppressed");
        assert!(!note_is_fresh(miss), "still suppressed on later turns");
        assert!(
            note_is_fresh(load),
            "a different message is independent of the suppressed one"
        );
        assert!(!note_is_fresh(load));
    }
}
