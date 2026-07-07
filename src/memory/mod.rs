//! Standalone, best-for-CLI memory brain. See `.claude/plans/linked-riding-mochi.md`.
//!
//! P1: markdown store + lexical retrieval. Later phases add frozen-core/search tool (P2),
//! learning (P3), anti-bloat (P4), dense semantic tier (P5).

pub mod bloat;
pub mod category;
pub mod dialectic;
pub mod dimension;
pub mod embed;
pub mod frontmatter;
pub mod frozen_core;
pub mod fuse;
pub mod graph;
pub mod learning;
pub mod model_dl;
pub mod profile;
pub mod provenance;
pub mod render;
pub mod score;
pub mod store;
pub mod tokenize;

use crate::core::config::{self, MemorySettings};
use crate::memory::dimension::Dimension;
use crate::memory::learning::{LearnOptions, LearnReport};
use crate::memory::provenance::ProvenanceKind;
use crate::memory::score::Bm25Index;
use crate::memory::store::{MemoryEntry, MemoryType};
use crate::memory::tokenize::tokenize;
use anyhow::Result;
use std::cmp::Ordering;
use std::collections::HashSet;

/// A scored search hit.
pub struct Hit {
    pub entry: MemoryEntry,
    pub score: f64,
}

/// Which workspace zones a read (frozen core, search) should see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScopeSel {
    /// The default working view: global facts + the current project's zone.
    Current,
    /// Everything, all zones (CLI inspection; the `NG_NO_SCOPE` kill-switch).
    All,
    /// Global facts only.
    Global,
    /// One specific zone by slug.
    Project(String),
}

impl ScopeSel {
    /// Does an entry carrying `scope` pass this selector? `current` = the resolved current slug.
    pub fn admits(&self, scope: Option<&str>, current: &str) -> bool {
        match self {
            ScopeSel::All => true,
            ScopeSel::Global => scope.is_none(),
            ScopeSel::Current => scope.is_none() || scope == Some(current),
            ScopeSel::Project(s) => scope == Some(s.as_str()),
        }
    }

    /// The production default view: `Current`, or `All` when scoping is killed via `NG_NO_SCOPE`
    /// (reads collapse back to the single pre-scoping pool; writes keep tagging either way).
    pub fn default_view() -> ScopeSel {
        if config::scope_disabled() {
            ScopeSel::All
        } else {
            ScopeSel::Current
        }
    }
}

/// Live lazy search over the long-tail store for INJECTION INTO CONTEXT (the agent's
/// `memory_search` tool). Same ranking as `search_filtered`, **plus** it records implicit-reuse
/// reinforcement (the P8 evolution spine): every fact it returns into context is reinforced at
/// most once/day. `cmd_search` (human inspection) and the bench deliberately use the read-only
/// paths so only genuine context-injection grows the reuse signal.
pub fn search_scoped(query: &str, k: usize, sel: &ScopeSel) -> Result<Vec<Hit>> {
    let mut hits = search_filtered_scoped(query, k, None, sel)?;
    // Graph EXPANSION (P5, bench-gated): pull in strong co-retrieval neighbors this lexical/dense
    // query missed, before the reuse signal is recorded — so an expanded neighbor also counts as
    // co-fired. Default-OFF (`NG_GRAPH_EXPAND`); the recording spine below always runs.
    if graph::expansion_enabled() {
        expand_with_graph(&mut hits, query, k, sel);
    }
    record_reuse(&hits); // best-effort; never fails the search
    Ok(hits)
}

/// Reinforce every returned fact (per-day deduped) — implicit-reuse signal — AND record the
/// co-retrieval event into the Hebbian graph (P5): the set of facts recalled together this turn
/// gets its pairwise associations strengthened. Both are best-effort: a read-only store just means
/// no signal this turn, never a failed retrieval.
fn record_reuse(hits: &[Hit]) {
    if hits.is_empty() {
        return;
    }
    let today = bloat::decay::today();
    for h in hits {
        let _ = store::record_retrieval(&h.entry, &today);
    }
    // "Neurons that fire together wire together": one search that surfaced ≥2 facts is one co-fire
    // event. Per-day-deduped per pair inside `record_coretrieval`, so a chatty session can't inflate
    // a link. Best-effort — a graph write failure never breaks the search. Skipped when the
    // `NG_NO_GRAPH` kill-switch is set (collapse to the pre-P5 path).
    if hits.len() >= 2 && graph::recording_enabled() {
        let ids: Vec<&str> = hits.iter().map(|h| h.entry.id.as_str()).collect();
        let _ = graph::record_coretrieval(&ids, &today);
    }
}

/// Widen `hits` with the strongest co-retrieval neighbors of the facts already retrieved (P5
/// expansion). A neighbor is appended only if it isn't already present, is admitted by the scope
/// selector, and is still an active fact; its score is the seed hit's score scaled by the (decayed)
/// edge weight, so a graph-surfaced fact never outranks a genuine lexical/dense match. The list is
/// re-truncated to `k`. Best-effort and cheap: skips entirely when the graph is empty.
fn expand_with_graph(hits: &mut Vec<Hit>, _query: &str, k: usize, sel: &ScopeSel) {
    if hits.is_empty() {
        return;
    }
    let today = bloat::decay::today();
    let present: HashSet<String> = hits.iter().map(|h| h.entry.id.clone()).collect();
    // Gather candidate neighbor ids (id → best incoming edge weight) from every seed hit.
    let mut cand: std::collections::HashMap<String, f64> = std::collections::HashMap::new();
    for h in hits.iter() {
        for (nid, w) in graph::neighbors(&h.entry.id, &today, k, GRAPH_EDGE_FLOOR) {
            if present.contains(&nid) {
                continue;
            }
            // Weight the neighbor by the seed's own score so it slots in below real matches.
            let contributed = h.score * w;
            cand.entry(nid).and_modify(|e| *e = e.max(contributed)).or_insert(contributed);
        }
    }
    if cand.is_empty() {
        return;
    }
    // Resolve candidate ids to active, scope-admitted entries.
    let all = match store::load_all() {
        Ok(a) => a,
        Err(_) => return,
    };
    let active = bloat::supersede::active(&all);
    let current = config::project_slug();
    for e in active {
        if let Some(&score) = cand.get(&e.id) {
            if sel.admits(e.scope.as_deref(), &current) {
                hits.push(Hit { entry: e, score });
            }
        }
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then(b.entry.mtime_ms.cmp(&a.entry.mtime_ms))
    });
    hits.truncate(k);
}

/// Minimum decayed edge weight for a neighbor to be considered live (below this a link has faded).
const GRAPH_EDGE_FLOOR: f64 = 0.35;

/// The full retrieval pipeline with dimension + workspace-zone selection. The zone filter runs
/// BEFORE the BM25 index is built (the index is corpus-relative — IDF/avgdl over exactly the
/// candidate set), so scoping *improves* ranking statistics instead of just masking rows.
pub fn search_filtered_scoped(
    query: &str,
    k: usize,
    dim: Option<Dimension>,
    sel: &ScopeSel,
) -> Result<Vec<Hit>> {
    search_filtered_scoped_cat(query, k, dim, None, sel)
}

/// As [`search_filtered_scoped`], plus an optional content-category filter (P3 CoALA typing) —
/// e.g. show only `bug-history` or `security-rule` facts. Category is derived on load, orthogonal
/// to the dimension filter, so the two compose (a `tooling` fact that is also a `command`).
pub fn search_filtered_scoped_cat(
    query: &str,
    k: usize,
    dim: Option<Dimension>,
    cat: Option<crate::memory::category::Category>,
    sel: &ScopeSel,
) -> Result<Vec<Hit>> {
    let all = store::load_all()?;
    let mut active = bloat::supersede::active(&all);
    let cfg = settings();
    // The exclusion set mirrors what the served core actually holds (build() applies its own
    // workspace view), so "long tail = whatever the always-on block doesn't carry" stays true.
    let core = frozen_core::build(&active, load_style().as_deref(), cfg.frozen_core_max_tokens);
    let exclude: HashSet<String> = core.source_ids.into_iter().collect();
    let current = config::project_slug();
    active.retain(|e| sel.admits(e.scope.as_deref(), &current));

    // Default path is the exact-BM25 lexical floor. `enable_dense` fuses a dense tier (RRF) with a
    // persistent per-fact embedding cache; `enable_fuzzy` adds the Jaro-Winkler bridge. Both OFF by
    // default — the moat tiers are now REACHABLE + integration-tested, not bench-only scaffolding.
    let mut hits = if cfg.enable_dense {
        let inner = embed::default_dense_embedder();
        let caching = CachingEmbedder {
            inner: inner.as_ref(),
            cache: std::cell::RefCell::new(embed::EmbeddingCache::load(&inner.id())),
        };
        // Query-level gated fusion: dense joins only when BM25 is ambiguous (low query coverage),
        // so a confident literal match keeps its lexical precision (see the bench in P6).
        let h = search_hybrid_gated_in(
            query,
            usize::MAX,
            active,
            &exclude,
            &caching,
            cfg.dense_gate_coverage,
        );
        caching.cache.borrow().save();
        h
    } else {
        rank_lexical(query, usize::MAX, active, &exclude, cfg.enable_fuzzy)
    };
    if let Some(d) = dim {
        hits.retain(|h| h.entry.dimension == d);
    }
    if let Some(c) = cat {
        hits.retain(|h| h.entry.category == c);
    }
    let today = bloat::decay::today();
    let half_life = cfg.recency_half_life_days;
    let cur_sub = config::current_subpath();
    for h in &mut hits {
        // final = bm25 · decay · salience — facts rise/sink on reuse + reinforcement (P8).
        h.score = bloat::decay::evolved_score(h.score, &h.entry, &today, half_life);
        // Soft region boost: a current-project fact tagged with the subpath the user is working
        // under right now edges out its zone-mates (never a hard partition — see the plan).
        if h.entry.scope.as_deref() == Some(current.as_str()) {
            if let (Some(tag), Some(cur)) = (h.entry.subpath.as_deref(), cur_sub.as_deref()) {
                if frozen_core::subpath_matches(tag, cur) {
                    h.score *= 1.15;
                }
            }
        }
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then(b.entry.mtime_ms.cmp(&a.entry.mtime_ms))
    });
    hits.truncate(k);
    Ok(hits)
}

/// Session-start: rebuild the frozen core from the current store and adopt it (a fresh prefix for
/// the new session), then return the rendered `<memory>` block. This is what makes a `type=user`
/// fact added since the last run actually appear in the prompt. Empty string if no core-eligible facts.
pub fn refresh_frozen_core() -> String {
    let entries = match store::load_all() {
        Ok(e) => e,
        Err(_) => return frozen_core::read_active(),
    };
    let active = bloat::supersede::active(&entries);
    frozen_core::refresh_active(&active, load_style().as_deref(), settings().frozen_core_max_tokens)
}

/// Load the user-style profile body from `~/.nextgen/cli-memory/STYLE.md`, if present.
pub fn load_style() -> Option<String> {
    let raw = std::fs::read_to_string(config::style_path()).ok()?;
    let fm = frontmatter::parse(&raw);
    let body = if fm.had_frontmatter { fm.body } else { raw };
    let body = body.trim().to_string();
    if body.is_empty() {
        None
    } else {
        Some(body)
    }
}

/// Pure search over a supplied entry set (used by the bench + tests).
pub fn search_in(query: &str, k: usize, entries: Vec<MemoryEntry>) -> Vec<Hit> {
    search_excluding(query, k, entries, &HashSet::new())
}

/// Pure search over a supplied entry set WITH the Jaro-Winkler fuzzy bridge (the `enable_fuzzy`
/// production path). Used by the bench's `--fuzzy` measurement to quantify the typo/morphology
/// recall gain vs. the exact-BM25 floor before flipping the default (W24).
pub fn search_in_fuzzy(query: &str, k: usize, entries: Vec<MemoryEntry>) -> Vec<Hit> {
    rank_lexical(query, k, entries, &HashSet::new(), true)
}

/// Pure search excluding ids already served in the frozen core (the lazy long tail).
/// Ranking is BM25 (P7) — IDF + length normalization computed over the candidate set.
/// Lexical-only; the dense path is `search_hybrid_in`.
///
/// Exact BM25 (`score`, not `score_fuzzy`) is the shipped floor: it returns the same nonzero
/// candidate SET as a token-overlap scorer (so injected-noise is unchanged) while ranking it
/// better via IDF + length normalization. The fuzzy bridge is implemented + unit-proven but
/// default-OFF — **measured** (W24, `ng bench memory --split all --fuzzy`) on the current bench
/// corpus: recall@5 delta is +0.000 on both the literal and paraphrase tune slices (the corpus is
/// already lexically saturated — no query in the fixture set actually has a typo to bridge), while
/// noise_rate rises (gate 0.497→0.580) and precision@5 drops (0.503→0.420). Net loss, not a wash:
/// stays OFF until a typo-heavy fixture proves a real recall gain. Same posture as the dense tier.
pub fn search_excluding(
    query: &str,
    k: usize,
    entries: Vec<MemoryEntry>,
    exclude: &HashSet<String>,
) -> Vec<Hit> {
    rank_lexical(query, k, entries, exclude, false)
}

/// Lexical ranking with an optional fuzzy (Jaro-Winkler) bridge. `fuzzy=false` is the shipped
/// exact-BM25 floor (`search_excluding`); `fuzzy=true` (production opt-in `enable_fuzzy`) also
/// bridges typo'd/near-miss query terms via `score_fuzzy`.
fn rank_lexical(
    query: &str,
    k: usize,
    entries: Vec<MemoryEntry>,
    exclude: &HashSet<String>,
    fuzzy: bool,
) -> Vec<Hit> {
    let q = tokenize(query);
    let candidates: Vec<MemoryEntry> =
        entries.into_iter().filter(|e| !exclude.contains(&e.id)).collect();
    // IDF + avgdl are corpus-relative, so build the index over exactly the candidate set we rank.
    let idx = Bm25Index::build(candidates.iter().map(|e| e.tokens.as_slice()));
    let mut hits: Vec<Hit> = candidates
        .into_iter()
        .filter_map(|e| {
            let s = if fuzzy { idx.score_fuzzy(&q, &e.tokens) } else { idx.score(&q, &e.tokens) };
            if s > 0.0 {
                Some(Hit { entry: e, score: s })
            } else {
                None
            }
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(b.entry.mtime_ms.cmp(&a.entry.mtime_ms))
    });
    hits.truncate(k);
    hits
}

/// Hybrid search: fuse the lexical ranking with a dense (embedding) ranking via RRF.
/// Pure over a supplied entry set (used by the bench + tests). If the dense list adds
/// nothing the result degrades to the lexical order — lexical is always the floor.
pub fn search_hybrid_in(
    query: &str,
    k: usize,
    entries: Vec<MemoryEntry>,
    exclude: &HashSet<String>,
    embedder: &dyn embed::Embedder,
) -> Vec<Hit> {
    let candidates: Vec<MemoryEntry> =
        entries.into_iter().filter(|e| !exclude.contains(&e.id)).collect();

    // lexical ranking (ids best→worst)
    let lexical: Vec<String> = search_in(query, usize::MAX, candidates.clone())
        .into_iter()
        .map(|h| h.entry.id)
        .collect();

    // dense ranking (cosine of query vs each entry body, best→worst)
    let qv = embedder.embed(query);
    let mut dense: Vec<(String, f32)> = candidates
        .iter()
        .map(|e| (e.id.clone(), embed::cosine(&qv, &embedder.embed(&e.body))))
        .filter(|(_, s)| *s > 0.0)
        .collect();
    dense.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    let dense_ids: Vec<String> = dense.into_iter().map(|(id, _)| id).collect();

    let fused = fuse::rrf(&[lexical, dense_ids], settings().rrf_k);
    let by_id: std::collections::HashMap<String, MemoryEntry> =
        candidates.into_iter().map(|e| (e.id.clone(), e)).collect();
    fused
        .into_iter()
        .filter_map(|(id, score)| by_id.get(&id).map(|e| Hit { entry: e.clone(), score }))
        .take(k)
        .collect()
}

/// Fraction of the query's DISTINCT tokens that appear in `hit_tokens` — a cheap proxy for "how
/// confidently did BM25 match this?". A literal query fully covered by its top hit → 1.0; a
/// paraphrase / cross-lingual query whose wording barely overlaps the stored fact → low. Empty
/// query → 0.0 (nothing to cover, so the gate opens and lets dense try).
fn lexical_coverage(query_tokens: &HashSet<String>, hit_tokens: &[String]) -> f64 {
    if query_tokens.is_empty() {
        return 0.0;
    }
    let hit: HashSet<&String> = hit_tokens.iter().collect();
    let covered = query_tokens.iter().filter(|t| hit.contains(t)).count();
    covered as f64 / query_tokens.len() as f64
}

/// Hybrid search with a QUERY-LEVEL GATE (P6). Runs the lexical floor first; the dense tier is
/// fused in **only when BM25 is ambiguous** — i.e. the best lexical hit covers fewer than
/// `gate_coverage` of the query's tokens (paraphrase / cross-lingual), OR the query returned no
/// lexical hit at all. A confident, high-coverage literal match returns the pure lexical order,
/// preserving its precision (the bench showed always-on fusion lifts paraphrase recall but wrecks
/// literal-slice precision/noise). `gate_coverage >= 1.0` ⇒ always fuse (the always-on ceiling);
/// `<= 0.0` ⇒ never fuse. Pure over a supplied entry set (used by the bench + tests).
pub fn search_hybrid_gated_in(
    query: &str,
    k: usize,
    entries: Vec<MemoryEntry>,
    exclude: &HashSet<String>,
    embedder: &dyn embed::Embedder,
    gate_coverage: f64,
) -> Vec<Hit> {
    let candidates: Vec<MemoryEntry> =
        entries.into_iter().filter(|e| !exclude.contains(&e.id)).collect();

    // Lexical ranking first — it is always the floor.
    let lex_hits = search_in(query, usize::MAX, candidates.clone());

    // Gate decision: fuse dense unless the top lexical hit already covers the query confidently.
    let qtoks: HashSet<String> = tokenize(query).into_iter().collect();
    let top_coverage = lex_hits
        .first()
        .map(|h| lexical_coverage(&qtoks, &h.entry.tokens))
        .unwrap_or(0.0);
    let gate_open = top_coverage < gate_coverage;

    if !gate_open {
        // BM25 is confident (high-coverage literal match) → skip dense, keep lexical precision.
        return lex_hits.into_iter().take(k).collect();
    }

    // Gate open → fuse the dense ranking in via RRF (the paraphrase / cross-lingual path).
    let lexical: Vec<String> = lex_hits.into_iter().map(|h| h.entry.id).collect();
    let qv = embedder.embed(query);
    let mut dense: Vec<(String, f32)> = candidates
        .iter()
        .map(|e| (e.id.clone(), embed::cosine(&qv, &embedder.embed(&e.body))))
        .filter(|(_, s)| *s > 0.0)
        .collect();
    dense.sort_by(|a, b| {
        b.1.partial_cmp(&a.1).unwrap_or(Ordering::Equal).then(a.0.cmp(&b.0))
    });
    let dense_ids: Vec<String> = dense.into_iter().map(|(id, _)| id).collect();

    let fused = fuse::rrf(&[lexical, dense_ids], settings().rrf_k);
    let by_id: std::collections::HashMap<String, MemoryEntry> =
        candidates.into_iter().map(|e| (e.id.clone(), e)).collect();
    fused
        .into_iter()
        .filter_map(|(id, score)| by_id.get(&id).map(|e| Hit { entry: e.clone(), score }))
        .take(k)
        .collect()
}

/// An `Embedder` decorator that memoizes through the persistent [`embed::EmbeddingCache`] so each
/// fact body is embedded once across runs (the production dense path wraps the real embedder in
/// this). `embed` is `&self`, so the cache lives behind a `RefCell`; call `.cache.borrow().save()`
/// after the search to persist new vectors. Keeps the pure `search_hybrid_in` (bench) cache-free.
struct CachingEmbedder<'a> {
    inner: &'a dyn embed::Embedder,
    cache: std::cell::RefCell<embed::EmbeddingCache>,
}
impl embed::Embedder for CachingEmbedder<'_> {
    fn id(&self) -> String {
        self.inner.id()
    }
    fn dim(&self) -> usize {
        self.inner.dim()
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        self.cache.borrow_mut().get_or_compute(text, self.inner)
    }
}

// ── CLI command handlers ────────────────────────────────────────────────

/// Explicit user capture — the REPL `#text` affordance. A `#`-prefixed line is the user *directly*
/// stating a fact to remember: the highest-confidence signal, so it's stored straight into the
/// long-tail store as durable `feedback` (the agent's `memory_search` reads it next turn), bypassing
/// the implicit free-extractor's confidence gate. Returns the new entry id.
///
/// Scope: a fact typed while standing in a project is usually ABOUT that project, so it lands in
/// the current workspace zone by default; `#remember global: <text>` (or `g:`) pins it global.
/// (Feedback never enters the frozen core, so scope only steers the default search visibility.)
pub fn remember(text: &str) -> Result<String> {
    let text = text.trim();
    let (scope, text) = match strip_global_prefix(text) {
        Some(rest) => (None, rest),
        None => (Some(config::project_slug()), text),
    };
    let text = text.trim();
    if text.is_empty() {
        anyhow::bail!("nothing to remember");
    }
    let slug = remember_slug(text);
    let desc: String = text.chars().take(80).collect();
    store::add_scoped(&slug, &desc, MemoryType::Feedback, text, scope.as_deref())
}

/// `global: <text>` / `g: <text>` → the text with the marker stripped; `None` when unmarked.
/// `.get` (not a raw slice) so a leading multi-byte char (`#gõ …`) can never panic mid-codepoint.
fn strip_global_prefix(text: &str) -> Option<&str> {
    for p in ["global:", "g:"] {
        if text.get(..p.len()).is_some_and(|head| head.eq_ignore_ascii_case(p)) {
            return Some(&text[p.len()..]);
        }
    }
    None
}

/// A short kebab id from the first few words of a `#remember` capture (the store also disambiguates
/// on collision). Letters/digits kept; runs of anything else collapse to a single `-`.
fn remember_slug(text: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true; // suppress a leading dash
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
        if slug.split('-').filter(|s| !s.is_empty()).count() >= 6 && c.is_whitespace() {
            break; // ~6 words is enough for a readable id
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        "note".to_string()
    } else {
        slug.chars().take(60).collect()
    }
}

pub fn cmd_add(name: &str, description: &str, mtype: &str, body: &str) -> Result<()> {
    let t = MemoryType::parse(mtype);
    let id = store::add(name, description, t, body)?;
    println!("saved memory '{id}' (type={})", t.as_str());
    Ok(())
}

/// Parse a CLI `--scope` value into a workspace view. `None` → All (human inspection sees
/// everything); named views: all | global | current (global + this project) | project (this
/// project's zone only) | any literal zone slug.
pub fn parse_scope_sel(scope: Option<&str>) -> ScopeSel {
    match scope.map(str::trim).map(str::to_lowercase).as_deref() {
        None | Some("") | Some("all") => ScopeSel::All,
        Some("global") => ScopeSel::Global,
        Some("current") => ScopeSel::Current,
        Some("project") => ScopeSel::Project(config::project_slug()),
        Some(slug) => ScopeSel::Project(slug.to_string()),
    }
}

/// `[p:slug]` display tag for a zoned entry; empty for global.
fn zone_tag(e: &MemoryEntry) -> String {
    match e.scope.as_deref() {
        Some(z) => format!(" [p:{z}]"),
        None => String::new(),
    }
}

pub fn cmd_list(scope: Option<&str>) -> Result<()> {
    let sel = parse_scope_sel(scope);
    let current = config::project_slug();
    let mut entries = store::load_all()?;
    let superseded = entries.iter().filter(|e| !e.is_active()).count();
    entries.retain(|e| e.is_active() && sel.admits(e.scope.as_deref(), &current));
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    if entries.is_empty() {
        println!("(no active memories in this view — `aizen memory add ...`, or `--scope all`)");
        return Ok(());
    }
    for e in &entries {
        let desc = if e.description.is_empty() {
            String::new()
        } else {
            format!(" — {}", e.description)
        };
        println!("[{}]{} {}{}", e.mtype.as_str(), zone_tag(e), e.name, desc);
    }
    println!("\n{} memories", entries.len());
    if superseded > 0 {
        println!("({superseded} superseded — hidden; `aizen memory as-of <date>` to view history)");
    }
    Ok(())
}

pub fn cmd_show(id_or_name: &str) -> Result<()> {
    let entries = store::load_all()?;
    let key = id_or_name.to_lowercase();
    let found = entries
        .iter()
        .find(|e| e.id == key || e.name.to_lowercase() == key);
    match found {
        Some(e) => {
            println!("# {} ({})", e.name, e.mtype.as_str());
            if !e.description.is_empty() {
                println!("{}", e.description);
            }
            if let Some(c) = &e.created {
                println!("created: {c}");
            }
            println!("\n{}", e.body);
            Ok(())
        }
        None => anyhow::bail!("no memory matching '{id_or_name}'"),
    }
}

pub fn cmd_search(
    query: &str,
    k: usize,
    dimension: Option<String>,
    category: Option<String>,
    scope: Option<&str>,
) -> Result<()> {
    let dim = match &dimension {
        Some(s) => Some(Dimension::parse(s).ok_or_else(|| {
            anyhow::anyhow!("unknown dimension '{s}' (style|tooling|workflow|stack|other)")
        })?),
        None => None,
    };
    let cat = match &category {
        Some(s) => Some(crate::memory::category::Category::parse(s).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown category '{s}' (bug-history|failed-attempt|success-pattern|arch-decision|command|security-rule|deploy-note|codebase|none)"
            )
        })?),
        None => None,
    };
    let sel = parse_scope_sel(scope);
    let hits = search_filtered_scoped_cat(query, k, dim, cat, &sel)?;
    if hits.is_empty() {
        let d = dim.map(|d| format!(" in dimension '{}'", d.as_str())).unwrap_or_default();
        let c = cat.map(|c| format!(" in category '{}'", c.as_str())).unwrap_or_default();
        println!("(no matches for '{query}'{d}{c})");
        return Ok(());
    }
    for h in &hits {
        println!(
            "{:.3}  [{}/{}]{}{} {}",
            h.score,
            h.entry.mtype.as_str(),
            h.entry.dimension.as_str(),
            zone_tag(&h.entry),
            cat_tag(&h.entry),
            h.entry.name
        );
    }
    Ok(())
}

/// `[c:bug-history]` display tag for a categorized entry; empty for the `None` (uncategorized) case.
fn cat_tag(e: &MemoryEntry) -> String {
    match e.category {
        crate::memory::category::Category::None => String::new(),
        c => format!(" [c:{}]", c.as_str()),
    }
}

/// Show the derived user profile (B2): a deterministic, free/local rollup of the user's
/// working preferences from the fact store + STYLE.md. `--json` for machine output.
pub fn cmd_profile(json: bool) -> Result<()> {
    let profile = build_profile()?;
    if json {
        println!("{}", serde_json::to_string_pretty(&profile)?);
        return Ok(());
    }
    render_profile(&profile);
    Ok(())
}

/// Load the store (+ STYLE.md as a high-trust synthetic fact) and roll up the profile.
/// Shared by `ng memory profile` and the `memory_profile` agent tool.
pub fn build_profile() -> Result<profile::UserProfile> {
    let mut entries = store::load_all()?;
    if let Some(style) = load_style() {
        let today = bloat::decay::today();
        entries.push(MemoryEntry {
            id: "style".into(),
            name: "user-style".into(),
            body: style.clone(),
            mtype: MemoryType::User,
            source: ProvenanceKind::Manual,
            confidence: 1.0,
            dimension: dimension::classify(&style),
            created: Some(today.clone()),
            updated: Some(today),
            ..Default::default()
        });
    }
    Ok(profile::build(&entries, &bloat::decay::today(), settings().recency_half_life_days))
}

fn render_profile(p: &profile::UserProfile) {
    println!("# user profile — derived, free/local (cite-backed)\n");
    for d in &p.dims {
        let conf = format!("conf {:.0}%", d.confidence * 100.0);
        let line = match &d.verdict {
            profile::Verdict::Insufficient => "(insufficient evidence)".to_string(),
            profile::Verdict::Scalar { value, label } => format!("{label} ({value:+.2})  {conf}"),
            profile::Verdict::Choice { value, runner_up, margin } => {
                let ru = runner_up.as_deref().map(|r| format!(" vs {r}")).unwrap_or_default();
                format!("{value}{ru}  margin {margin:.2}  {conf}")
            }
            profile::Verdict::Ranked { items } => {
                let top: Vec<String> = items.iter().take(5).map(|(t, _)| t.clone()).collect();
                format!("{}  {conf}", top.join(", "))
            }
        };
        println!("{:<13} {line}", d.dim.as_str());
        if !matches!(d.verdict, profile::Verdict::Insufficient) && !d.basis.is_empty() {
            let cited: Vec<String> = d.basis.iter().take(3).map(|b| b.name.clone()).collect();
            println!("              ↳ {}", cited.join("; "));
        }
    }
}

/// Answer a natural-language question ABOUT the user (B3 dialectic). Free/local; abstains
/// rather than guessing. Shared by `ng memory ask` and the `memory_ask` agent tool.
pub fn cmd_ask(query: &str, json: bool) -> Result<()> {
    let answer = answer_about_user(query)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&answer)?);
        return Ok(());
    }
    println!("{}", answer.text);
    if !answer.basis.is_empty() {
        let cited: Vec<String> = answer.basis.iter().take(3).map(|b| b.name.clone()).collect();
        println!("  ↳ from: {}", cited.join("; "));
    }
    Ok(())
}

/// Build the profile + store and answer one dialectic question. Shared by CLI + the tool.
pub fn answer_about_user(query: &str) -> Result<dialectic::Answer> {
    let profile = build_profile()?;
    let entries = store::load_all()?;
    Ok(dialectic::answer(&profile, &entries, query))
}

/// Show the frozen core, refreshed from the current store (the same block a new REPL/agent session
/// injects into the prompt). `_rebuild` is accepted for back-compat but is now a no-op — the core is
/// always rebuilt + adopted at session start (see `refresh_frozen_core`).
pub fn cmd_frozen(_rebuild: bool) -> Result<()> {
    let served = refresh_frozen_core();
    if served.trim().is_empty() {
        println!("(frozen core empty — add `type=user` memories or a STYLE.md, e.g. `aizen memory add me -t user -b \"...\"`)");
        return Ok(());
    }
    let entries = store::load_all()?;
    let active = bloat::supersede::active(&entries);
    let fresh = frozen_core::build(&active, load_style().as_deref(), settings().frozen_core_max_tokens);
    println!(
        "frozen core: ~{} tok · {} entries · {} spilled to retrieval (refreshed from the current store)\n",
        crate::memory::render::est_tokens(&served),
        fresh.source_ids.len(),
        fresh.spilled_ids.len()
    );
    println!("{served}");
    Ok(())
}

/// Ingest a user turn through the learning pipeline and print a report.
pub fn cmd_learn(user_text: &str, yes: bool, dry_run: bool) -> Result<()> {
    let opts = LearnOptions {
        session_id: learning::default_session_id(),
        auto_confirm_core: if yes { Some(true) } else { None },
        dry_run,
    };
    let report = learning::ingest(user_text, &opts)?;
    print_learn_report(&report, dry_run);
    Ok(())
}

fn print_learn_report(r: &LearnReport, dry_run: bool) {
    let tag = if dry_run { " (dry-run, nothing written)" } else { "" };
    if !r.changed() && r.rejected.is_empty() && r.dropped == 0 {
        if r.skipped_passive {
            println!("no learnable signal in this turn (passive — zero cost){tag}");
        } else {
            println!("nothing to learn{tag}");
        }
        return;
    }
    for id in &r.added {
        println!("+ added      {id}{tag}");
    }
    for id in &r.reinforced {
        println!("↑ reinforced {id}{tag}");
    }
    for f in &r.core_promoted {
        println!("★ core       {f}{tag}");
    }
    for id in &r.queued_review {
        println!("? review     {id} (run `aizen memory review`){tag}");
    }
    for (fact, why) in &r.rejected {
        println!("✗ rejected   {fact}  — {why}");
    }
    for id in &r.archived {
        println!("⌁ archived   {id} (over inferred-cap; `aizen memory restore {id}`)");
    }
    if r.dropped > 0 {
        println!("  ({} low-confidence candidate(s) dropped)", r.dropped);
    }
}

/// Show the always-on user-style profile (`STYLE.md`).
pub fn cmd_style() -> Result<()> {
    match load_style() {
        Some(body) => {
            println!("# user style ({})", config::style_path().display());
            println!("\n{body}");
        }
        None => println!("(no STYLE.md yet — learned via `aizen memory learn` core-promotion, or edit {} directly)", config::style_path().display()),
    }
    Ok(())
}

/// Manage the review queue (mid-confidence learned candidates awaiting a human gate).
pub fn cmd_review(promote: Option<String>, clear: bool) -> Result<()> {
    let dir = config::review_dir();
    let queued = store::load_from(&dir)?;

    if clear {
        let n = queued.len();
        let _ = std::fs::remove_dir_all(&dir);
        println!("cleared {n} review item(s).");
        return Ok(());
    }

    if let Some(key) = promote {
        let key = key.to_lowercase();
        let item = queued
            .iter()
            .find(|e| e.id == key || e.name.to_lowercase() == key)
            .ok_or_else(|| anyhow::anyhow!("no review item matching '{key}'"))?;
        let w = store::LearnedWrite {
            name: &item.name,
            description: &item.description,
            mtype: item.mtype,
            body: &item.body,
            source: item.source,
            confidence: item.confidence,
            session_id: &learning::default_session_id(),
            no_core: false, // accepting a reviewed item → eligible for the core like any user fact
            scope: item.scope.clone(),
            subpath: item.subpath.clone(),
        };
        let id = store::add_learned(&w)?;
        let _ = std::fs::remove_file(&item.path);
        println!("promoted review item '{}' → store entry '{id}'", item.id);
        return Ok(());
    }

    if queued.is_empty() {
        println!("(review queue empty)");
        return Ok(());
    }
    for e in &queued {
        println!("[{:.2}] {} — {}", e.confidence, e.id, e.body);
    }
    println!(
        "\n{} item(s). Promote: `aizen memory review --promote <id>`; discard all: `aizen memory review --clear`",
        queued.len()
    );
    Ok(())
}

/// Show what was valid on a given `YYYY-MM-DD` (bi-temporal history view).
pub fn cmd_as_of(date: &str) -> Result<()> {
    let all = store::load_all()?;
    let snap = bloat::supersede::as_of(&all, date);
    if snap.is_empty() {
        println!("(no memories were valid as of {date})");
        return Ok(());
    }
    let mut snap = snap;
    snap.sort_by(|a, b| a.id.cmp(&b.id));
    for e in &snap {
        println!("[{}] {} — {}", e.mtype.as_str(), e.name, e.body);
    }
    println!("\n{} memories valid as of {date}", snap.len());
    Ok(())
}

/// Supersede `old` with `new` (bi-temporal): `old` is marked no-longer-valid but kept for
/// history. Both are matched by id or name.
pub fn cmd_supersede(old: &str, new: &str) -> Result<()> {
    let all = store::load_all()?;
    let find = |key: &str| {
        let k = key.to_lowercase();
        all.iter().find(|e| e.id == k || e.name.to_lowercase() == k).cloned()
    };
    let old_e = find(old).ok_or_else(|| anyhow::anyhow!("no memory matching '{old}'"))?;
    let new_e = find(new).ok_or_else(|| anyhow::anyhow!("no memory matching '{new}'"))?;
    if old_e.id == new_e.id {
        anyhow::bail!("'{old}' and '{new}' are the same memory");
    }
    store::mark_superseded(&old_e, &new_e.id)?;
    println!("superseded '{}' → '{}' (history kept; see `aizen memory as-of <date>`)", old_e.id, new_e.id);
    Ok(())
}

/// Show the Hebbian co-retrieval neighbors of a fact (P5): the other facts most often recalled
/// together with it, ranked by decayed edge weight. Matches `id` by id or name. Inspection-only —
/// reads the graph, never records a co-fire (so `ng memory neighbors` can't pollute its own signal).
pub fn cmd_neighbors(id_or_name: &str, k: usize) -> Result<()> {
    let all = store::load_all()?;
    let key = id_or_name.to_lowercase();
    let seed = all
        .iter()
        .find(|e| e.id == key || e.name.to_lowercase() == key)
        .ok_or_else(|| anyhow::anyhow!("no memory matching '{id_or_name}'"))?;
    let today = bloat::decay::today();
    let neigh = graph::neighbors(&seed.id, &today, k, 0.0);
    if neigh.is_empty() {
        println!("({} has no co-retrieval associations yet)", seed.id);
        return Ok(());
    }
    // Resolve neighbor ids to names/bodies for a legible listing (a dangling id is shown raw).
    println!("neighbors of '{}' (co-recalled together, strongest first):\n", seed.id);
    for (nid, w) in &neigh {
        match all.iter().find(|e| &e.id == nid) {
            Some(e) => {
                let body: String = e.body.chars().take(70).collect();
                println!("  {w:.3}  {}{} — {}", e.id, cat_tag(e), body.replace('\n', " "));
            }
            None => println!("  {w:.3}  {nid} (fact no longer present)"),
        }
    }
    Ok(())
}

/// Restore an archived memory back into the live store.
pub fn cmd_restore(id: &str) -> Result<()> {
    let restored = bloat::caps::restore(id)?;
    println!("restored '{restored}' from the archive");
    Ok(())
}

/// List archived (LRU-evicted) memories.
pub fn cmd_archive_list() -> Result<()> {
    let arch = bloat::caps::list_archive()?;
    if arch.is_empty() {
        println!("(archive empty)");
        return Ok(());
    }
    let mut arch = arch;
    arch.sort_by(|a, b| a.id.cmp(&b.id));
    for e in &arch {
        println!("[{}] {} — {}", e.mtype.as_str(), e.id, e.body);
    }
    println!("\n{} archived. Restore: `aizen memory restore <id>`", arch.len());
    Ok(())
}

/// Run a maintenance compaction pass (enforce the inferred LRU cap → archive victims).
pub fn cmd_compact() -> Result<()> {
    let report = bloat::compact()?;
    if report.archived.is_empty() {
        println!("nothing to compact (under cap).");
    } else {
        println!("archived {} over-cap inferred fact(s):", report.archived.len());
        for id in &report.archived {
            println!("  ⌁ {id}");
        }
    }
    // P5: the same pass prunes co-retrieval edges to facts that no longer exist.
    if report.edges_pruned > 0 {
        println!("pruned {} dangling graph edge(s).", report.edges_pruned);
    }
    Ok(())
}

/// Settings accessor (verified-good defaults). `AIZEN_MEM_FUZZY=1` opts the Jaro-Winkler bridge in
/// (default OFF). The dense tier (P6) is ON by default on a `--features dense` build (the one with a
/// real semantic backend, where the bench proved gated fusion wins) and OFF on a default build (only
/// the non-semantic `HashEmbedder` → enabling it there is pure overhead). `AIZEN_MEM_DENSE` overrides
/// either way (`=0/off` disables, `=1/on` forces on).
pub fn settings() -> MemorySettings {
    let mut s = MemorySettings::default();
    if env_on("AIZEN_MEM_FUZZY") {
        s.enable_fuzzy = true;
    }
    // Dense tier (P6): ON by default ONLY on a `--features dense` build — that's the one carrying a
    // real semantic backend, and the bench proved gated fusion earns its keep there (paraphrase
    // recall +0.231 with negligible literal-slice noise). A default build has only the non-semantic
    // `HashEmbedder`, so enabling dense there is pure overhead for no gain → it stays OFF. Either
    // way `AIZEN_MEM_DENSE` overrides: `=0/off` kills it on a dense build, `=1` forces the plumbing
    // path on a default build (integration tests). The gate itself (`dense_gate_coverage`) still
    // means a confident literal query never pays the dense cost.
    s.enable_dense = cfg!(feature = "dense");
    if let Some(v) = env_flag("AIZEN_MEM_DENSE") {
        s.enable_dense = v;
    }
    s
}

/// Parse a truthy/falsy env toggle. `Some(true)` for `1/true/yes/on`, `Some(false)` for
/// `0/false/no/off`, `None` when unset/empty/unrecognized (caller keeps its default).
fn env_flag(key: &str) -> Option<bool> {
    match std::env::var(key).ok().as_deref().map(str::trim) {
        Some("1") | Some("true") | Some("yes") | Some("on") => Some(true),
        Some("0") | Some("false") | Some("no") | Some("off") => Some(false),
        _ => None,
    }
}

fn env_on(key: &str) -> bool {
    matches!(std::env::var(key).ok().as_deref(), Some("1") | Some("true") | Some("yes") | Some("on"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::store::{MemoryEntry, MemoryType};
    use std::path::PathBuf;

    #[test]
    fn settings_dense_defaults_to_the_feature_and_env_overrides_both_ways() {
        // P6: `settings().enable_dense` tracks the `dense` cargo feature by default (ON only where a
        // real semantic backend exists), and `AIZEN_MEM_DENSE` overrides in EITHER direction. Guard
        // the process-global env with the home lock so we don't race other env-touching tests.
        let _g = config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("AIZEN_MEM_DENSE");
        // unset → mirrors the build: ON with `--features dense`, OFF otherwise.
        assert_eq!(settings().enable_dense, cfg!(feature = "dense"));
        // explicit ON forces the plumbing path even on a default build.
        std::env::set_var("AIZEN_MEM_DENSE", "1");
        assert!(settings().enable_dense);
        // explicit OFF kills it even on a `--features dense` build.
        std::env::set_var("AIZEN_MEM_DENSE", "off");
        assert!(!settings().enable_dense);
        std::env::remove_var("AIZEN_MEM_DENSE");
    }

    #[test]
    fn remember_defaults_to_project_zone_and_global_prefix_overrides() {
        let _g = config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-remember-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("NEXTGEN_HOME", &dir);
        std::env::set_var("NG_PROJECT_ROOT", &dir);

        let id = remember("the api base is internal dot example").unwrap();
        let find = |id: &str| store::load_all().unwrap().into_iter().find(|e| e.id == id).unwrap();
        assert_eq!(
            find(&id).scope,
            Some(config::project_slug()),
            "a fact typed inside a project lands in its zone"
        );

        let gid = remember("GLOBAL: reply tersely everywhere").unwrap();
        let g = find(&gid);
        assert!(g.scope.is_none(), "global: prefix pins the fact global");
        assert!(!g.body.to_lowercase().contains("global:"), "marker stripped from the body");

        std::env::remove_var("NG_PROJECT_ROOT");
        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn strip_global_prefix_matches_markers_only_and_is_utf8_safe() {
        assert_eq!(strip_global_prefix("global: x").map(str::trim), Some("x"));
        assert_eq!(strip_global_prefix("G: x").map(str::trim), Some("x"));
        assert!(strip_global_prefix("globally speaking").is_none());
        assert!(strip_global_prefix("gõ tiếng Việt nhanh").is_none(), "multi-byte head must not panic");
    }

    #[test]
    fn search_scoped_current_hides_foreign_zones() {
        let _g = config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-scope-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("NEXTGEN_HOME", &dir);
        std::env::set_var("NG_PROJECT_ROOT", &dir);
        let cur = config::project_slug();

        store::add_scoped("here fact", "", MemoryType::Reference, "the deploy pipeline uses fly", Some(&cur)).unwrap();
        store::add_scoped("foreign fact", "", MemoryType::Reference, "the deploy pipeline uses render", Some("otherproj-11111111")).unwrap();
        store::add("global fact", "", MemoryType::Feedback, "always deploy carefully").unwrap();

        let current = search_scoped("deploy pipeline", 10, &ScopeSel::Current).unwrap();
        assert!(current.iter().any(|h| h.entry.scope.as_deref() == Some(cur.as_str())), "current zone visible");
        assert!(current.iter().any(|h| h.entry.scope.is_none()), "global visible");
        assert!(
            current.iter().all(|h| h.entry.scope.as_deref() != Some("otherproj-11111111")),
            "another project's facts are invisible in the working view"
        );

        let all = search_filtered_scoped("deploy pipeline", 10, None, &ScopeSel::All).unwrap(); // human view
        assert!(all.iter().any(|h| h.entry.scope.as_deref() == Some("otherproj-11111111")), "All sees every zone");

        let global_only = search_scoped("deploy", 10, &ScopeSel::Global).unwrap();
        assert!(!global_only.is_empty() && global_only.iter().all(|h| h.entry.scope.is_none()));

        std::env::remove_var("NG_PROJECT_ROOT");
        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remember_slug_is_kebab_and_bounded() {
        assert_eq!(remember_slug("Prefer pnpm over npm"), "prefer-pnpm-over-npm");
        let s = remember_slug("  the API base is https://x/v1  ");
        assert!(s.starts_with("the-api-base-is"), "got {s}");
        assert!(!s.contains(' ') && !s.starts_with('-') && !s.ends_with('-'), "got {s}");
        assert_eq!(remember_slug("!!!"), "note"); // no alphanumerics → fallback
        assert!(remember_slug(&"word ".repeat(50)).chars().count() <= 60);
    }

    fn entry(id: &str, text: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.to_string(),
            path: PathBuf::from(format!("{id}.md")),
            name: id.to_string(),
            description: String::new(),
            mtype: MemoryType::Reference,
            created: None,
            body: text.to_string(),
            mtime_ms: 0,
            tokens: tokenize(text),
            ..Default::default()
        }
    }

    #[test]
    fn search_ranks_by_overlap() {
        let entries = vec![
            entry("a", "postgres index tuning and query plans"),
            entry("b", "react suspense and tanstack query"),
            entry("c", "auth login oauth jwt session refresh"),
        ];
        let hits = search_in("oauth jwt login", 5, entries);
        assert_eq!(hits[0].entry.id, "c");
        // unrelated docs filtered out (score 0)
        assert!(hits.iter().all(|h| h.score > 0.0));
    }

    #[test]
    fn search_respects_k() {
        let entries = vec![
            entry("a", "auth login"),
            entry("b", "auth session"),
            entry("c", "auth token"),
        ];
        let hits = search_in("auth", 2, entries);
        assert_eq!(hits.len(), 2);
    }

    #[test]
    fn fuzzy_ranking_bridges_a_typo() {
        // enable_fuzzy production path (rank_lexical fuzzy=true) recalls a typo'd term the exact
        // BM25 floor misses, while never dropping below the exact score.
        let entries = vec![
            entry("a", "postgres index tuning and query plans"),
            entry("b", "react suspense and tanstack query"),
        ];
        let ex = HashSet::new();
        let exact = rank_lexical("postgers tuning", 5, entries.clone(), &ex, false);
        let fuzzy = rank_lexical("postgers tuning", 5, entries, &ex, true);
        let score_of = |hits: &[Hit]| hits.iter().find(|h| h.entry.id == "a").map(|h| h.score).unwrap_or(0.0);
        assert!(score_of(&fuzzy) > score_of(&exact), "fuzzy bridges the typo → higher score on 'a'");
    }

    #[test]
    fn search_in_fuzzy_is_the_public_bench_entry_for_the_bridge() {
        // The bench's --fuzzy measurement (W24) calls this exact function; pin its wiring to
        // rank_lexical(fuzzy=true) so the bench numbers can't silently drift from production.
        let entries = vec![
            entry("a", "postgres index tuning and query plans"),
            entry("b", "react suspense and tanstack query"),
        ];
        let via_public = search_in_fuzzy("postgers tuning", 5, entries.clone());
        let via_private = rank_lexical("postgers tuning", 5, entries, &HashSet::new(), true);
        assert_eq!(
            via_public.iter().map(|h| h.score).collect::<Vec<_>>(),
            via_private.iter().map(|h| h.score).collect::<Vec<_>>()
        );
    }

    #[test]
    fn dense_caching_embedder_runs_and_persists() {
        // enable_dense production path: the CachingEmbedder memoizes through the on-disk
        // EmbeddingCache, the hybrid (lexical⊕dense RRF) fusion returns the right doc, and the
        // cache file is written under the home dir. Exercises the whole dense tier end-to-end.
        let _g = config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-densecache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);

        let entries = vec![
            entry("a", "postgres index tuning and query plans"),
            entry("b", "auth login oauth jwt session refresh"),
        ];
        let inner = embed::default_dense_embedder();
        let caching = CachingEmbedder {
            inner: inner.as_ref(),
            cache: std::cell::RefCell::new(embed::EmbeddingCache::load(&inner.id())),
        };
        let hits = search_hybrid_in("oauth login", usize::MAX, entries, &HashSet::new(), &caching);
        caching.cache.borrow().save();
        assert!(!hits.is_empty(), "dense⊕lexical fusion returns hits");
        assert_eq!(hits[0].entry.id, "b", "the auth doc wins for an oauth query");
        assert!(
            config::embed_cache_dir().join(format!("{}.json", inner.id())).exists(),
            "embedding cache persisted to disk"
        );

        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn lexical_coverage_is_full_for_a_literal_match_and_low_for_paraphrase() {
        // A query whose every token is present in the hit → coverage 1.0; a query sharing only
        // one of three tokens → ~0.33; an empty query → 0.0 (gate opens, lets dense try).
        let hit = tokenize("auth login oauth jwt session refresh");
        let full: HashSet<String> = tokenize("oauth login").into_iter().collect();
        assert!((lexical_coverage(&full, &hit) - 1.0).abs() < 1e-9);
        let partial: HashSet<String> = tokenize("oauth deploy pipeline").into_iter().collect();
        assert!(lexical_coverage(&partial, &hit) < 0.5, "only 1/3 tokens covered");
        assert_eq!(lexical_coverage(&HashSet::new(), &hit), 0.0);
    }

    #[test]
    fn gate_closed_on_a_confident_literal_match_skips_dense() {
        // When the top lexical hit fully covers the query, the gate stays CLOSED: the result is the
        // pure lexical order and the (adversarial) embedder is never allowed to reorder it. Uses an
        // embedder that would rank the WRONG doc first if consulted, so a closed gate is observable.
        struct AdversarialEmbedder;
        impl embed::Embedder for AdversarialEmbedder {
            fn id(&self) -> String { "adversarial".into() }
            fn dim(&self) -> usize { 4 }
            // Constant vector → cosine identical for every doc, so dense contributes a stable but
            // meaningless ranking; if it were fused it would inject the non-matching doc as noise.
            fn embed(&self, _t: &str) -> Vec<f32> { vec![1.0, 0.0, 0.0, 0.0] }
        }
        let entries = vec![
            entry("hit", "auth login oauth jwt session refresh"),
            entry("noise", "postgres index tuning and query plans"),
        ];
        // "oauth login" is fully covered by "hit" → coverage 1.0 ≥ gate 0.6 → gate CLOSED.
        let gated = search_hybrid_gated_in(
            "oauth login", usize::MAX, entries.clone(), &HashSet::new(), &AdversarialEmbedder, 0.6,
        );
        assert_eq!(gated[0].entry.id, "hit", "confident literal match keeps lexical order");
        assert_eq!(gated.len(), 1, "only the lexically-matching doc is returned (dense not fused)");
    }

    #[test]
    fn gate_open_on_low_coverage_fuses_the_dense_neighbor() {
        // A query the lexical floor barely covers opens the gate, so a dense-only hit surfaces.
        // The embedder returns the SAME vector for the query and the target doc (cosine 1.0) and an
        // orthogonal vector otherwise, so dense uniquely favors the intended doc.
        struct TargetedEmbedder;
        impl embed::Embedder for TargetedEmbedder {
            fn id(&self) -> String { "targeted".into() }
            fn dim(&self) -> usize { 2 }
            fn embed(&self, t: &str) -> Vec<f32> {
                // "paraphrase" query and the deploy doc share the semantic axis; others are orthogonal.
                if t.contains("ship") || t.contains("release") || t.contains("deploy") {
                    vec![1.0, 0.0]
                } else {
                    vec![0.0, 1.0]
                }
            }
        }
        let entries = vec![
            entry("deploy", "deploy release to production on fridays"),
            entry("other", "postgres index tuning notes"),
        ];
        // "ship" shares no token with any doc → lexical coverage 0.0 < gate → gate OPEN → dense fuses
        // and surfaces the deploy doc (its vector matches the query's).
        let gated = search_hybrid_gated_in(
            "ship", usize::MAX, entries, &HashSet::new(), &TargetedEmbedder, 0.6,
        );
        assert!(!gated.is_empty(), "gate open → dense surfaces a semantic neighbor lexical missed");
        assert_eq!(gated[0].entry.id, "deploy", "the dense-favored doc wins when the gate is open");
    }

    #[test]
    fn category_filter_separates_bug_from_decision_end_to_end() {
        // P3: content-category is derived on load, so a search filtered to one category must return
        // only facts whose text classifies there — proven through the real store, not just classify().
        let _g = config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-cat-filter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("NEXTGEN_HOME", &dir);

        use crate::memory::category::Category;
        // non-user types so neither lands in the always-on frozen core (which search excludes).
        store::add("parser crash", "", MemoryType::Project, "the parser hit a null pointer panic on empty input").unwrap();
        store::add("store design", "", MemoryType::Project, "we decided the architecture uses one store per zone by convention").unwrap();

        let unfiltered = search_filtered_scoped_cat("parser store", 10, None, None, &ScopeSel::All).unwrap();
        assert!(unfiltered.len() >= 2, "both facts match unfiltered");

        let bugs = search_filtered_scoped_cat("parser store", 10, None, Some(Category::BugHistory), &ScopeSel::All).unwrap();
        assert!(!bugs.is_empty() && bugs.iter().all(|h| h.entry.category == Category::BugHistory), "bug filter returns only bug-history");
        assert!(bugs.iter().any(|h| h.entry.body.contains("panic")));

        let decisions = search_filtered_scoped_cat("parser store", 10, None, Some(Category::ArchDecision), &ScopeSel::All).unwrap();
        assert!(!decisions.is_empty() && decisions.iter().all(|h| h.entry.category == Category::ArchDecision), "decision filter returns only arch-decision");
        assert!(decisions.iter().all(|h| !h.entry.body.contains("panic")), "the bug is excluded from the decision view");

        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scoped_search_separates_same_keyword_by_dimension() {
        // B1 falsification: two facts both lexically match "package", but one is tooling
        // (pnpm) and one is stack (rust) — dimension scoping must return only the right one.
        let _g = config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-scoped-dim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("NEXTGEN_HOME", &dir);

        // non-user types so neither lands in the always-on frozen core (which search excludes).
        store::add("prefer pnpm", "", MemoryType::Feedback, "I prefer pnpm as my package manager").unwrap();
        store::add("project stack", "", MemoryType::Project, "the package is built with rust and tokio").unwrap();

        let unscoped = search_scoped("package", 10, &ScopeSel::All).unwrap();
        assert!(unscoped.len() >= 2, "both facts match 'package' unscoped");

        let tooling = search_filtered_scoped("package", 10, Some(Dimension::Tooling), &ScopeSel::All).unwrap();
        assert!(!tooling.is_empty() && tooling.iter().all(|h| h.entry.dimension == Dimension::Tooling));
        assert!(tooling.iter().any(|h| h.entry.body.contains("pnpm")));

        let stack = search_filtered_scoped("package", 10, Some(Dimension::Stack), &ScopeSel::All).unwrap();
        assert!(!stack.is_empty() && stack.iter().all(|h| h.entry.dimension == Dimension::Stack));
        assert!(stack.iter().all(|h| !h.entry.body.contains("pnpm")), "stack scope excludes the tooling fact");

        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn precision_and_token_budget_hold_at_scale() {
        // 2000 unrelated distractors should not crowd a relevant fact out of the top-K,
        // and the injected slice stays under the token budget regardless of corpus size.
        let mut entries = Vec::with_capacity(2002);
        for i in 0..2000 {
            entries.push(entry(&format!("d{i}"), &format!("distractor entry {i} about miscellaneous unrelated chores")));
        }
        entries.push(entry("target", "prefers pnpm over npm for package management"));
        let hits = search_in("pnpm package management preference", 5, entries);
        assert!(hits.len() <= 5);
        assert!(hits.iter().any(|h| h.entry.id == "target"), "relevant fact must survive 2k distractors");

        let top: Vec<MemoryEntry> = hits.into_iter().map(|h| h.entry).collect();
        let (block, _inc, _sp) = render::render_block("search", &top, settings().search_max_tokens);
        assert!(
            render::est_tokens(&block) <= settings().search_max_tokens,
            "per-turn injected tokens must stay bounded as the corpus grows"
        );
    }
}
