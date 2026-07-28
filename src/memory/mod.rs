//! Standalone, best-for-CLI memory brain. See `.claude/plans/linked-riding-mochi.md`.
//!
//! P1: markdown store + lexical retrieval. Later phases add frozen-core/search tool (P2),
//! learning (P3), anti-bloat (P4), dense semantic tier (P5).

pub mod bloat;
pub mod category;
pub mod dialectic;
pub mod dimension;
pub mod doctor;
pub mod embed;
pub mod frontmatter;
pub mod frozen_core;
pub mod fuse;
pub mod graph;
pub mod learning;
pub mod model_dl;
pub mod path_scope;
pub mod pending;
pub mod profile;
pub mod provenance;
pub mod render;
pub mod score;
pub mod session_mem;
pub mod stats;
pub mod store;
pub mod tokenize;

use crate::core::config::{self, MemorySettings};
use crate::memory::dimension::Dimension;
use crate::memory::learning::{LearnOptions, LearnReport};
use crate::memory::provenance::ProvenanceKind;
use crate::memory::score::Bm25Index;
use crate::memory::store::{MemoryEntry, MemoryType};
use crate::memory::tokenize::tokenize;
use crate::ui::tui;
use anyhow::{Context, Result};
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
    /// Does this entry pass the selector, from where we are standing (`lin`)?
    ///
    /// **The one read predicate** (invariant I4): search, graph expansion and inventory all funnel
    /// through here, so "what can be recalled here" has exactly one definition.
    ///
    /// Keyed on the tier axis, not on `scope`. It has to be: the learning path now writes
    /// `scope: None` and expresses placement as `tier`/`anchor`, so a `scope`-based filter would
    /// admit every place fact everywhere — the pollution the anchor axis exists to prevent.
    pub fn admits(&self, e: &MemoryEntry, lin: &path_scope::Lineage) -> bool {
        match self {
            ScopeSel::All => true,
            // `specificity` is `None` for a fact that does not apply here: another machine's
            // device fact, a place fact anchored elsewhere, or an orphan place (a legacy zone slug
            // we cannot resolve back to a directory — fail-closed).
            ScopeSel::Current => lin.specificity(e).is_some(),
            ScopeSel::Global => e.tier == path_scope::Tier::User,
            // Legacy zone inspection (`aizen memory list --scope <slug>`): the slug only ever
            // existed on disk, so this stays a `scope` comparison. It is how an orphan place fact
            // remains reachable to a human even though no lineage will admit it.
            ScopeSel::Project(s) => e.scope.as_deref() == Some(s.as_str()),
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
            cand.entry(nid)
                .and_modify(|e| *e = e.max(contributed))
                .or_insert(contributed);
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
    let lin = path_scope::Lineage::current();
    for e in active {
        if let Some(&score) = cand.get(&e.id) {
            if sel.admits(&e, &lin) {
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
    let lin = path_scope::Lineage::current();
    active.retain(|e| sel.admits(e, &lin));

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
    // Where the project as a whole sits: an anchor DEEPER than this is a fact about a region
    // inside the project rather than about the project at large.
    let project_depth = path_scope::depth(&config::anchor_of(&config::project_root()));
    for h in &mut hits {
        // final = bm25 · decay · salience — facts rise/sink on reuse + reinforcement (P8).
        h.score = bloat::decay::evolved_score(h.score, &h.entry, &today, half_life);
        // Soft region boost: of the facts that apply here, the ones anchored most specifically to
        // where the user is working edge out the ones that merely cover the whole tree. Replaces
        // the `subpath` tag boost — the anchor already encodes the region, so `subpath` would be a
        // second, less precise copy of it. Still a nudge, never a partition.
        if h.entry.tier == path_scope::Tier::Place {
            if let Some(spec) = lin.specificity(&h.entry) {
                if spec as usize > project_depth {
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

/// Read the already-adopted frozen core for the current device without rebuilding it.
///
/// Ordinary turns use this path so retrieval/reinforcement during a conversation cannot rewrite the
/// cached system lane or consume `core.next.md` early. `refresh_frozen_core` remains the explicit
/// session-boundary operation (startup, a fresh thread, or a one-shot run).
pub fn active_frozen_core() -> String {
    frozen_core::read_active()
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
    frozen_core::refresh_active(
        &active,
        load_style().as_deref(),
        settings().frozen_core_max_tokens,
    )
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

/// How many candidates the recall block ranks before packing (the block itself is budget-bound,
/// so this only caps ranking work).
const RECALL_CANDIDATES: usize = 12;

/// Minimum lexical coverage for the TOP hit before a recall block is injected at all.
///
/// Without a gate, BM25 keeps anything scoring above zero, so one shared common word ("file",
/// "run", "lỗi") drags a block into nearly every turn — spending tokens and, worse, teaching the
/// model to ignore the block because it is usually irrelevant. Mirrors the codebase-retrieval gate
/// (`agent::codebase::gate_passes`), which had the same problem for the same reason.
const RECALL_GATE_COVERAGE: f64 = 0.34;

/// The marker every recall block starts with. Fixed so callers can strip stale blocks out of old
/// user turns by prefix match rather than by re-deriving what was injected.
pub const RECALL_MARKER: &str = "Recalled memory";

/// A user message with any recall block we folded in removed.
///
/// The REPL pushes the FOLDED text into history, so anything reading history back — compaction, the
/// end-of-turn secretary — sees our own injected block as if the user had typed it. For the
/// secretary that is an echo chamber: it would re-emit the fact it was just shown, local
/// reconciliation would score it `Same`, and the fact would earn a confirmation for being repeated
/// back rather than for being useful. Exactly the self-reinforcing loop that dropping `reinforced`
/// was meant to end.
///
/// The marker must be at position 0 — only our own folding puts it there, so a message that merely
/// mentions the phrase is returned untouched.
pub fn strip_recall_prefix(content: &str) -> &str {
    if !content.starts_with(RECALL_MARKER) {
        return content;
    }
    match content.split_once("\n\n") {
        Some((_, rest)) => rest,
        None => content,
    }
}

/// Build the per-turn recall block for `query`, or `None` to inject nothing.
///
/// Returns the rendered block AND the handle→id pairs it used, so the caller can seat them in the
/// [`pending`] ledger — `used` reports from the end-of-turn secretary can then only ever name a
/// fact that was actually shown.
///
/// `None` (cheap passthrough) when: the query is empty, nothing matches, the top hit fails the
/// relevance gate, or the same selection was already injected on the previous turn.
///
/// Budget packing **breaks** rather than continues when a line does not fit. The selection must be
/// monotone in the budget: `self_block` continues past an oversized line, which is right there (it
/// is choosing what to say about a persona) but wrong here, because the handles are positional —
/// skipping `[m2]` and still emitting `[m3]` would hand the model a numbering with a hole in it.
pub fn recall_block(query: &str, budget_tokens: usize) -> Option<(String, Vec<pending::Pending>)> {
    if query.trim().is_empty() {
        return None;
    }
    // The working view: only facts true HERE (see `ScopeSel::admits`). Read-only — this is not the
    // agent's `memory_search` tool, so it must NOT record reuse: the fact was offered, not used.
    // Phase 3's `used` report is what earns a confirmation.
    let hits = search_filtered_scoped(query, RECALL_CANDIDATES, None, &ScopeSel::default_view())
        .ok()
        .filter(|h| !h.is_empty())?;

    let q: HashSet<String> = tokenize(query).into_iter().collect();
    if lexical_coverage(&q, &hits[0].entry.tokens) < RECALL_GATE_COVERAGE {
        return None;
    }

    let header = format!(
        "{RECALL_MARKER} (may be stale; verify before relying on it. \
         Cite the handle, e.g. [m1], if you use one):"
    );
    let mut budget = budget_tokens.saturating_sub(render::est_tokens(&header) + 1);
    let mut lines: Vec<String> = Vec::new();
    let mut pairs: Vec<pending::Pending> = Vec::new();

    for h in &hits {
        let body = render::sanitize_body(h.entry.body.trim());
        if body.is_empty() {
            continue;
        }
        let handle = format!("m{}", pairs.len() + 1);
        let line = format!("[{handle}] ({}) {body}", tier_label(&h.entry));
        let cost = render::est_tokens(&line) + 1;
        if cost > budget {
            break; // positional handles must stay contiguous — see the note above
        }
        budget -= cost;
        lines.push(line);
        pairs.push(pending::Pending {
            handle,
            id: h.entry.id.clone(),
        });
    }
    if pairs.is_empty() {
        return None;
    }

    // Delta check LAST: the selection has to be known before we can tell whether it changed. An
    // unchanged block is already in the transcript, so re-folding it pays twice for one sentence and
    // puts the same claim in context two ways with nothing marking which is newer.
    let ids: Vec<String> = pairs.iter().map(|p| p.id.clone()).collect();
    if pending::is_same_as_last(&ids) {
        return None;
    }

    Some((format!("{header}\n{}", lines.join("\n")), pairs))
}

/// How a recalled fact is attributed in the block: the axis it lives on, in the user's terms.
fn tier_label(e: &MemoryEntry) -> &'static str {
    match e.tier {
        path_scope::Tier::User => "about you",
        path_scope::Tier::Device => "this machine",
        path_scope::Tier::Place => "here",
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
    let candidates: Vec<MemoryEntry> = entries
        .into_iter()
        .filter(|e| !exclude.contains(&e.id))
        .collect();
    // IDF + avgdl are corpus-relative, so build the index over exactly the candidate set we rank.
    let idx = Bm25Index::build(candidates.iter().map(|e| e.tokens.as_slice()));
    let mut hits: Vec<Hit> = candidates
        .into_iter()
        .filter_map(|e| {
            let s = if fuzzy {
                idx.score_fuzzy(&q, &e.tokens)
            } else {
                idx.score(&q, &e.tokens)
            };
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
    let candidates: Vec<MemoryEntry> = entries
        .into_iter()
        .filter(|e| !exclude.contains(&e.id))
        .collect();

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
        .filter_map(|(id, score)| {
            by_id.get(&id).map(|e| Hit {
                entry: e.clone(),
                score,
            })
        })
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
    let candidates: Vec<MemoryEntry> = entries
        .into_iter()
        .filter(|e| !exclude.contains(&e.id))
        .collect();

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
        .filter_map(|(id, score)| {
            by_id.get(&id).map(|e| Hit {
                entry: e.clone(),
                score,
            })
        })
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
        if text
            .get(..p.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(p))
        {
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
    tui::emit_line(&format!("saved memory '{id}' (type={})", t.as_str()));
    Ok(())
}

/// Resolve one entry by id or (case-insensitive) name, over the live store.
///
/// The store's ids are slugs, so `id == name.to_lowercase()` for most entries and an exact match on
/// either is the common path. When neither matches exactly we fall back to a UNIQUE prefix/substring
/// match, and report the candidates rather than picking one when it's ambiguous — a silent
/// wrong-entry edit or delete is the failure mode worth spending an error message on.
pub fn resolve_entry(id_or_name: &str) -> Result<MemoryEntry> {
    let all = store::load_all()?;
    resolve_in(all, id_or_name)
}

/// `resolve_entry` over an already-loaded set (so a caller with the entries in hand doesn't re-read
/// the whole store, and tests can drive it without a temp home).
pub fn resolve_in(entries: Vec<MemoryEntry>, id_or_name: &str) -> Result<MemoryEntry> {
    let key = id_or_name.trim().to_lowercase();
    if key.is_empty() {
        anyhow::bail!("no memory id given");
    }
    if let Some(e) = entries
        .iter()
        .find(|e| e.id == key || e.name.to_lowercase() == key)
    {
        return Ok(e.clone());
    }
    let mut near: Vec<&MemoryEntry> = entries
        .iter()
        .filter(|e| e.id.contains(&key) || e.name.to_lowercase().contains(&key))
        .collect();
    match near.len() {
        0 => anyhow::bail!("no memory matching '{id_or_name}' (see `aizen memory list`)"),
        1 => Ok(near.remove(0).clone()),
        _ => {
            let names: Vec<&str> = near.iter().take(6).map(|e| e.id.as_str()).collect();
            anyhow::bail!(
                "'{id_or_name}' matches {} memories ({}) — use the full id",
                near.len(),
                names.join(", ")
            )
        }
    }
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

/// Enumerate what is stored, WITHOUT a search query — the answer to "what do you actually know
/// about me?". Rendered as text (shared by the CLI's `memory list` and the agent's `memory_list`
/// tool) so the human and the model always see the same inventory, addressed by the same ids.
///
/// `mtype` filters to one kind; `archived` lists the recoverable archive instead of the live store.
/// Entries are grouped by type and each line leads with the ID, because every write path
/// (`edit`/`forget`/`supersede`) addresses the id, not the display name.
pub fn inventory(
    sel: &ScopeSel,
    mtype: Option<MemoryType>,
    limit: usize,
    archived: bool,
) -> Result<String> {
    let lin = path_scope::Lineage::current();
    let all = if archived {
        bloat::caps::list_archive()?
    } else {
        store::load_all()?
    };
    let superseded = all.iter().filter(|e| !e.is_active()).count();
    let mut entries: Vec<MemoryEntry> = all
        .into_iter()
        // The archive is a graveyard: filtering it by `is_active` would hide superseded rows, which
        // are exactly what someone inspecting the archive is looking for.
        .filter(|e| archived || e.is_active())
        .filter(|e| sel.admits(e, &lin))
        .filter(|e| mtype.is_none_or(|t| e.mtype == t))
        .collect();
    let total = entries.len();
    if total == 0 {
        let where_ = if archived { "the archive" } else { "this view" };
        return Ok(format!("(nothing stored in {where_})"));
    }
    // Most-recently-touched first: what the store learned lately is what a "what do you know" question
    // is usually about, and it keeps the truncated tail the least interesting part.
    entries.sort_by(|a, b| {
        let key = |e: &MemoryEntry| {
            e.updated
                .clone()
                .or_else(|| e.created.clone())
                .unwrap_or_default()
        };
        key(b).cmp(&key(a)).then_with(|| a.id.cmp(&b.id))
    });
    let shown = limit.min(total);
    let mut out = String::new();
    let mut last_type: Option<MemoryType> = None;
    for e in entries.iter().take(shown) {
        if last_type != Some(e.mtype) {
            out.push_str(&format!("\n[{}]\n", e.mtype.as_str()));
            last_type = Some(e.mtype);
        }
        let desc = e.description_or_body_head();
        let sup = match &e.superseded_by {
            Some(by) => format!(" (superseded by {by})"),
            None => String::new(),
        };
        out.push_str(&format!(
            "  {}{}{} — {desc}{sup}\n",
            e.id,
            zone_tag(e),
            cat_tag(e)
        ));
    }
    if shown < total {
        out.push_str(&format!(
            "\n(+{} more — raise `limit` or filter by type/scope)\n",
            total - shown
        ));
    }
    out.push_str(&format!("\n{total} stored"));
    if !archived && superseded > 0 {
        out.push_str(&format!(
            "; {superseded} superseded (hidden — `memory as-of <date>`)"
        ));
    }
    Ok(out.trim_start().to_string())
}

pub fn cmd_list(scope: Option<&str>) -> Result<()> {
    // Human listing: no cap (an explicit `memory list` should show everything it has).
    tui::emit_line(&inventory(
        &parse_scope_sel(scope),
        None,
        usize::MAX,
        false,
    )?);
    Ok(())
}

/// Show one entry in full, INCLUDING its metadata. The id is printed separately from the display
/// name because every write command (`edit`/`forget`/`supersede`) addresses the id, and the two
/// differ whenever a slug collided (`fact`, `fact-2`) — printing only the name left no way to tell
/// two same-named entries apart.
pub fn cmd_show(id_or_name: &str) -> Result<()> {
    let e = resolve_entry(id_or_name)?;
    tui::emit_line(&format!("# {} ({})", e.name, e.mtype.as_str()));
    tui::emit_line(&format!("id: {}", e.id));
    if !e.description.is_empty() {
        tui::emit_line(&e.description);
    }
    tui::emit_line(&meta_line(&e));
    if let Some(by) = &e.superseded_by {
        let to = e.valid_to.as_deref().unwrap_or("?");
        tui::emit_line(&format!("superseded: {to} → '{by}' (kept for history)"));
    }
    tui::emit_line(&format!("file: {}", e.path.display()));
    tui::emit_line(&format!("\n{}", e.body));
    Ok(())
}

/// One-line provenance/lifecycle summary shared by `show` and the write commands' confirmations:
/// where the fact came from, how sure, how often reused, and when.
fn meta_line(e: &MemoryEntry) -> String {
    let mut parts = vec![format!("source: {}", e.source.as_str())];
    if e.source != ProvenanceKind::Manual {
        parts.push(format!("confidence {:.2}", e.confidence));
    }
    // `confirmations` is the number the ranking and the archive sweep actually read; `reinforced`
    // is the legacy retrieval counter kept for back-compat. Printing both, labelled, so a user
    // debugging "why did this rank low / get swept" sees the input that decided it.
    parts.push(format!("confirmed {}×", e.confirmations));
    if e.reinforced > 0 {
        parts.push(format!(
            "recalled {}× over {} session(s)",
            e.reinforced, e.sessions
        ));
    }
    parts.push(match (e.tier, e.anchor.as_deref()) {
        (crate::memory::path_scope::Tier::Place, Some(a)) => format!("place {a}"),
        (crate::memory::path_scope::Tier::Place, None) => "place (no anchor)".to_string(),
        (crate::memory::path_scope::Tier::Device, _) => match e.device.as_deref() {
            Some(d) => format!("device {d}"),
            None => "device".to_string(),
        },
        (crate::memory::path_scope::Tier::User, _) => "user (everywhere)".to_string(),
    });
    if e.category != crate::memory::category::Category::None {
        parts.push(format!(
            "{}/{}",
            e.category.kind().as_str(),
            e.category.as_str()
        ));
    }
    if let Some(c) = &e.created {
        parts.push(format!("created {c}"));
    }
    if let Some(u) = &e.updated {
        parts.push(format!("updated {u}"));
    }
    if let Some(r) = &e.last_retrieved {
        parts.push(format!("last recalled {r}"));
    }
    if e.core_denied {
        parts.push("core: denied by user".to_string());
    }
    parts.join(" · ")
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
        let d = dim
            .map(|d| format!(" in dimension '{}'", d.as_str()))
            .unwrap_or_default();
        let c = cat
            .map(|c| format!(" in category '{}'", c.as_str()))
            .unwrap_or_default();
        tui::emit_line(&format!("(no matches for '{query}'{d}{c})"));
        return Ok(());
    }
    for h in &hits {
        tui::emit_line(&format!(
            "{:.3}  [{}/{}]{}{} {}",
            h.score,
            h.entry.mtype.as_str(),
            h.entry.dimension.as_str(),
            zone_tag(&h.entry),
            cat_tag(&h.entry),
            h.entry.name
        ));
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
        tui::emit_line(&serde_json::to_string_pretty(&profile)?);
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
    Ok(profile::build(
        &entries,
        &bloat::decay::today(),
        settings().recency_half_life_days,
    ))
}

fn render_profile(p: &profile::UserProfile) {
    tui::emit_line("# user profile — derived, free/local (cite-backed)\n");
    for d in &p.dims {
        let conf = format!("conf {:.0}%", d.confidence * 100.0);
        let line = match &d.verdict {
            profile::Verdict::Insufficient => "(insufficient evidence)".to_string(),
            profile::Verdict::Scalar { value, label } => format!("{label} ({value:+.2})  {conf}"),
            profile::Verdict::Choice {
                value,
                runner_up,
                margin,
            } => {
                let ru = runner_up
                    .as_deref()
                    .map(|r| format!(" vs {r}"))
                    .unwrap_or_default();
                format!("{value}{ru}  margin {margin:.2}  {conf}")
            }
            profile::Verdict::Ranked { items } => {
                let top: Vec<String> = items.iter().take(5).map(|(t, _)| t.clone()).collect();
                format!("{}  {conf}", top.join(", "))
            }
        };
        tui::emit_line(&format!("{:<13} {line}", d.dim.as_str()));
        if !matches!(d.verdict, profile::Verdict::Insufficient) && !d.basis.is_empty() {
            let cited: Vec<String> = d.basis.iter().take(3).map(|b| b.name.clone()).collect();
            tui::emit_line(&format!("              ↳ {}", cited.join("; ")));
        }
    }
}

/// Answer a natural-language question ABOUT the user (B3 dialectic). Free/local; abstains
/// rather than guessing. Shared by `ng memory ask` and the `memory_ask` agent tool.
pub fn cmd_ask(query: &str, json: bool) -> Result<()> {
    let answer = answer_about_user(query)?;
    if json {
        tui::emit_line(&serde_json::to_string_pretty(&answer)?);
        return Ok(());
    }
    tui::emit_line(&answer.text);
    if !answer.basis.is_empty() {
        let cited: Vec<String> = answer
            .basis
            .iter()
            .take(3)
            .map(|b| b.name.clone())
            .collect();
        tui::emit_line(&format!("  ↳ from: {}", cited.join("; ")));
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
        tui::emit_line("(frozen core empty — add `type=user` memories or a STYLE.md, e.g. `aizen memory add me -t user -b \"...\"`)");
        return Ok(());
    }
    let entries = store::load_all()?;
    let active = bloat::supersede::active(&entries);
    let fresh = frozen_core::build(
        &active,
        load_style().as_deref(),
        settings().frozen_core_max_tokens,
    );
    tui::emit_line(&format!(
        "frozen core: ~{} tok · {} entries · {} spilled to retrieval (refreshed from the current store)\n",
        crate::memory::render::est_tokens(&served),
        fresh.source_ids.len(),
        fresh.spilled_ids.len()
    ));
    tui::emit_line(&served);
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
    let tag = if dry_run {
        " (dry-run, nothing written)"
    } else {
        ""
    };
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
    for (old, new) in &r.superseded {
        println!("↻ superseded {old} → {new}{tag}");
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
            tui::emit_line(&format!("# user style ({})", config::style_path().display()));
            tui::emit_line(&format!("\n{body}"));
        }
        None => tui::emit_line(&format!("(no STYLE.md yet — learned via `aizen memory learn` core-promotion, or edit {} directly)", config::style_path().display())),
    }
    Ok(())
}

/// Where discarded review items go. Rejecting a candidate is a judgement call, not a reason to
/// destroy the text — the queue is the only copy of a mid-confidence fact.
fn discarded_dir(review_dir: &std::path::Path) -> std::path::PathBuf {
    review_dir.join(".discarded")
}

/// Move review items out of the live queue into `.discarded/`. Returns how many moved.
fn discard_review_items(
    review_dir: &std::path::Path,
    items: &[store::MemoryEntry],
) -> Result<usize> {
    if items.is_empty() {
        return Ok(0);
    }
    let ddir = discarded_dir(review_dir);
    std::fs::create_dir_all(&ddir).with_context(|| format!("creating {}", ddir.display()))?;
    let mut moved = 0usize;
    for item in items {
        let dest = bloat::caps::unique_in(&ddir, &item.id);
        std::fs::rename(&item.path, &dest)
            .with_context(|| format!("discarding {}", item.path.display()))?;
        moved += 1;
    }
    Ok(moved)
}

/// Manage the review queue (mid-confidence learned candidates awaiting a human gate).
pub fn cmd_review(promote: Option<String>, drop_key: Option<String>, clear: bool) -> Result<()> {
    let dir = config::review_dir();
    let queued = store::load_from(&dir)?;

    if clear {
        let n = discard_review_items(&dir, &queued)?;
        tui::emit_line(&format!(
            "cleared {n} review item(s) → {}",
            discarded_dir(&dir).display()
        ));
        return Ok(());
    }

    if let Some(key) = drop_key {
        let key = key.to_lowercase();
        let item = queued
            .iter()
            .find(|e| e.id == key || e.name.to_lowercase() == key)
            .ok_or_else(|| anyhow::anyhow!("no review item matching '{key}'"))?;
        discard_review_items(&dir, std::slice::from_ref(item))?;
        tui::emit_line(&format!(
            "dropped review item '{}' → {}",
            item.id,
            discarded_dir(&dir).display()
        ));
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
            // Carry the placement the item was QUEUED with, verbatim. Promotion is a verdict on
            // whether the fact is true, not on where it applies — re-deciding the tier here would
            // silently re-anchor the fact to wherever the user happens to be standing when they
            // run `memory review --promote`, which is usually not where it was learned.
            tier: item.tier,
            anchor: item.anchor.clone(),
            device: item.device.clone(),
            supersedes: item.supersedes.clone(),
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
    let old_e = resolve_in(all.clone(), old)?;
    let new_e = resolve_in(all, new)?;
    if old_e.id == new_e.id {
        anyhow::bail!("'{old}' and '{new}' are the same memory");
    }
    store::mark_superseded(&old_e, &new_e.id)?;
    println!(
        "superseded '{}' → '{}' (history kept; see `aizen memory as-of <date>`)",
        old_e.id, new_e.id
    );
    Ok(())
}

/// Edit one stored fact in place. Only the fields passed are touched; the id never changes.
pub fn cmd_edit(
    id_or_name: &str,
    name: Option<String>,
    description: Option<String>,
    mtype: Option<String>,
    body: Option<String>,
    scope: Option<String>,
) -> Result<()> {
    let e = resolve_entry(id_or_name)?;
    let mtype = match mtype.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => Some(MemoryType::parse_strict(s).ok_or_else(|| {
            anyhow::anyhow!("unknown type '{s}' (user|feedback|project|reference)")
        })?),
        None => None,
    };
    // `--scope global` clears the zone; any other value moves the fact to that zone.
    let scope = scope.map(|s| {
        let s = s.trim().to_string();
        if s.is_empty() || s.eq_ignore_ascii_case("global") {
            None
        } else if s.eq_ignore_ascii_case("current") || s.eq_ignore_ascii_case("project") {
            Some(config::project_slug())
        } else {
            Some(s)
        }
    });
    let patch = store::EntryPatch {
        name,
        description,
        mtype,
        body,
        scope,
        preserve_updated: false,
        clear_supersede: false,
    };
    store::update(&e, &patch)?;
    // Re-read so the confirmation shows what is actually on disk now, not what we asked for.
    let after = resolve_entry(&e.id)?;
    tui::emit_line(&format!("updated '{}'", after.id));
    tui::emit_line(&meta_line(&after));
    tui::emit_line(&format!("file: {}", after.path.display()));
    Ok(())
}

/// Retire a fact into the recoverable archive (`aizen memory restore <id>` brings it back).
/// Named `forget` because that is what the user means; it is deliberately NOT a hard delete.
pub fn cmd_forget(id_or_name: &str) -> Result<()> {
    let e = resolve_entry(id_or_name)?;
    // Echo WHAT is being forgotten before doing it: `forget` takes a fuzzy key, so the one
    // failure worth guarding against is retiring a different fact than the user meant.
    tui::emit_line(&format!(
        "forgetting '{}' — {}",
        e.id,
        e.description_or_body_head()
    ));
    let archived = store::retire(&e)?;
    tui::emit_line(&format!(
        "archived as '{archived}' (restore: `aizen memory restore {archived}`)"
    ));
    Ok(())
}

/// Hard-delete an ARCHIVED fact's file. Irreversible, so it only ever touches the archive:
/// a live fact must be `forget`-ed first, which makes "destroy this" a deliberate two-step.
pub fn cmd_purge(id: &str) -> Result<()> {
    store::purge_archived(id)?;
    // The co-retrieval graph may now hold edges to an id that exists in neither the live store nor
    // the archive — the one case where a purge must also touch the graph.
    let pruned = bloat::prune_graph_best_effort();
    println!("purged archived '{id}' (irreversible)");
    if pruned > 0 {
        println!("pruned {pruned} dangling graph edge(s).");
    }
    Ok(())
}

/// Show the Hebbian co-retrieval neighbors of a fact (P5): the other facts most often recalled
/// together with it, ranked by decayed edge weight. Matches `id` by id or name. Inspection-only —
/// reads the graph, never records a co-fire (so `ng memory neighbors` can't pollute its own signal).
pub fn cmd_neighbors(id_or_name: &str, k: usize) -> Result<()> {
    let all = store::load_all()?;
    let seed = &resolve_in(all.clone(), id_or_name)?;
    let today = bloat::decay::today();
    let neigh = graph::neighbors(&seed.id, &today, k, 0.0);
    if neigh.is_empty() {
        println!("({} has no co-retrieval associations yet)", seed.id);
        return Ok(());
    }
    // Resolve neighbor ids to names/bodies for a legible listing (a dangling id is shown raw).
    println!(
        "neighbors of '{}' (co-recalled together, strongest first):\n",
        seed.id
    );
    for (nid, w) in &neigh {
        match all.iter().find(|e| &e.id == nid) {
            Some(e) => {
                let body: String = e.body.chars().take(70).collect();
                println!(
                    "  {w:.3}  {}{} — {}",
                    e.id,
                    cat_tag(e),
                    body.replace('\n', " ")
                );
            }
            None => println!("  {w:.3}  {nid} (fact no longer present)"),
        }
    }
    Ok(())
}

/// `memory list --superseded` — the graveyard. Retired facts are invisible to every other listing
/// (that is the point of the `active()` view), which left the user with no way to learn the id of a
/// fact a bad reconciliation retired — and no id means no `revive`. Visibility is what makes the
/// reverse gear reachable, so it ships alongside it.
pub fn cmd_list_superseded() -> Result<()> {
    let all = store::load_all()?;
    // "Dead" is the complement of the live view, not just `!is_active()`: a fact can also be hidden
    // by another live fact's forward `supersedes:` claim, and that one is the harder case to debug.
    let live: HashSet<String> = bloat::supersede::active(&all)
        .into_iter()
        .map(|e| e.id)
        .collect();
    let dead: Vec<&MemoryEntry> = all.iter().filter(|e| !live.contains(&e.id)).collect();
    if dead.is_empty() {
        tui::emit_line("(nothing superseded — every stored fact is live)");
        return Ok(());
    }
    for e in &dead {
        // Say WHY it is hidden: its own retirement stamp, or another fact's forward claim.
        let why = match (e.valid_to.as_deref(), e.superseded_by.as_deref()) {
            (Some(to), Some(by)) => format!("retired {to} → '{by}'"),
            (Some(to), None) => format!("retired {to}"),
            _ => all
                .iter()
                .find(|o| o.supersedes.as_deref() == Some(e.id.as_str()))
                .map(|o| format!("claimed by '{}'", o.id))
                .unwrap_or_else(|| "hidden".to_string()),
        };
        let body: String = e.body.chars().take(80).collect();
        tui::emit_line(&format!(
            "[{}] {} · {why} — {}",
            e.mtype.as_str(),
            e.id,
            body.replace('\n', " ")
        ));
    }
    tui::emit_line(&format!(
        "\n{} superseded. Bring one back: `aizen memory revive <id>`",
        dead.len()
    ));
    Ok(())
}

/// `memory revive <id>` — undo a supersession, in BOTH directions: clear the retired fact's own
/// `validTo`/`supersededBy`, and drop any live fact's `supersedes:` claim on it. Doing only the
/// first would leave the fact hidden by the survivor's forward pointer, which looks to the user
/// exactly like the revive silently failing.
pub fn cmd_revive(id_or_name: &str) -> Result<()> {
    let all = store::load_all()?;
    let key = id_or_name.trim().to_lowercase();
    let target = all
        .iter()
        .find(|e| e.id.to_lowercase() == key || e.name.to_lowercase() == key)
        .ok_or_else(|| {
            anyhow::anyhow!("no memory '{id_or_name}' (see `aizen memory list --superseded`)")
        })?;

    let mut acted = false;
    if !target.is_active() {
        store::unsupersede(target)?;
        acted = true;
    }
    let cleared = store::clear_supersedes_claims(&all, &target.id)?;
    acted |= !cleared.is_empty();
    if !acted {
        anyhow::bail!("'{}' is already live (nothing to revive)", target.id);
    }
    learning::audit::revive(&learning::default_session_id(), &target.id);
    tui::emit_line(&format!(
        "revived '{}' — {}",
        target.id,
        target.description_or_body_head()
    ));
    for c in &cleared {
        tui::emit_line(&format!("  dropped the supersedes claim on it from '{c}'"));
    }
    Ok(())
}

/// Restore an archived memory back into the live store, keeping its id unless `as_id` renames it.
pub fn cmd_restore(id: &str, as_id: Option<&str>) -> Result<()> {
    let restored = bloat::caps::restore(id, as_id)?;
    if restored == id.to_lowercase() {
        tui::emit_line(&format!("restored '{restored}' from the archive"));
    } else {
        tui::emit_line(&format!(
            "restored '{id}' from the archive as '{restored}' — pointers naming '{id}' were not rewritten"
        ));
    }
    Ok(())
}

/// List archived (LRU-evicted) memories.
pub fn cmd_archive_list() -> Result<()> {
    let arch = bloat::caps::list_archive()?;
    if arch.is_empty() {
        tui::emit_line("(archive empty)");
        return Ok(());
    }
    let mut arch = arch;
    arch.sort_by(|a, b| a.id.cmp(&b.id));
    for e in &arch {
        tui::emit_line(&format!("[{}] {} — {}", e.mtype.as_str(), e.id, e.body));
    }
    tui::emit_line(&format!(
        "\n{} archived. Restore: `aizen memory restore <id>`",
        arch.len()
    ));
    Ok(())
}

/// Run a maintenance compaction pass: enforce the per-partition LRU caps and sweep faded facts,
/// both into the recoverable archive.
pub fn cmd_compact() -> Result<()> {
    let report = bloat::compact()?;
    if report.archived.is_empty() && report.sweep_preview.is_empty() {
        println!("nothing to compact (under cap, nothing faded).");
    } else if !report.archived.is_empty() {
        println!(
            "archived {} fact(s) — `aizen memory restore <id>` brings one back:",
            report.archived.len()
        );
        for id in &report.archived {
            println!("  ⌁ {id}");
        }
    }
    // The first sweep on a store only previews. Say so explicitly, and say what WOULD go — a silent
    // "nothing happened" would leave the user unable to tell an armed no-op from an unarmed one.
    if !report.sweep_preview.is_empty() {
        println!(
            "\ndry run — {} fact(s) have faded below the strength floor. Nothing was moved.\n\
             Run this again to apply (the sweep is now armed for this store):",
            report.sweep_preview.len()
        );
        for id in &report.sweep_preview {
            println!("  ⌁ {id}");
        }
    }
    // P5: the same pass prunes co-retrieval edges to facts that no longer exist.
    if report.edges_pruned > 0 {
        println!("pruned {} dangling graph edge(s).", report.edges_pruned);
    }
    Ok(())
}

// ── reconcile (M2b) + doctor ─────────────────────────────────────────────

/// Everything one batch pass needs, gathered from disk: the pairs to judge and the live pool the
/// writes will act on.
///
/// Candidates are the review queue plus the live facts — the queue because that is where the local
/// pass parks what it could not call, and the live store because a differently-worded contradiction
/// scores BELOW the local band and therefore entered as an ordinary fact. Excluding the live side
/// would mean the one case M2b exists for never reaches it.
pub fn reconcile_inputs() -> Result<(Vec<learning::reconcile::Pair>, Vec<MemoryEntry>)> {
    let all = store::load_all()?;
    let live = bloat::supersede::active(&all);
    let queued = store::load_from(&config::review_dir()).unwrap_or_default();
    let mut candidates = queued;
    candidates.extend(live.iter().cloned());
    let pairs = learning::reconcile::collect_pairs(&candidates, &live);
    Ok((pairs, live))
}

/// Print what a pass did, or would do. A dry run has to be readable enough to consent to: every line
/// names the target, the verdict, the confidence, and — when nothing happened — WHY.
pub fn print_reconcile_report(r: &learning::reconcile::BatchReport) {
    use learning::reconcile::Action;
    if r.pairs_judged == 0 {
        tui::emit_line("no suspicious pairs — nothing to reconcile.");
        return;
    }
    if r.model_calls == 0 {
        tui::emit_line(&format!(
            "{} pair(s) collected, but the judgement call failed — nothing changed.",
            r.pairs_judged
        ));
        return;
    }
    if r.applied.is_empty() {
        tui::emit_line(&format!(
            "judged {} pair(s) in 1 call; the model returned no usable verdict — nothing changed.",
            r.pairs_judged
        ));
        return;
    }
    let head = if r.dry_run { "would" } else { "did" };
    tui::emit_line(&format!(
        "judged {} pair(s) in {} model call — what it {head}:\n",
        r.pairs_judged, r.model_calls
    ));
    for a in &r.applied {
        let verb = match &a.action {
            Action::Confirm { target } => format!("confirm '{target}'"),
            Action::Refine { target, .. } => format!("rewrite '{target}' in place"),
            Action::Supersede { target, .. } => format!("retire '{target}' behind a new fact"),
            Action::Review { target, why } => format!("leave '{target}' alone — {why}"),
        };
        let note = if a.note.is_empty() {
            String::new()
        } else {
            format!(" ({})", a.note)
        };
        tui::emit_line(&format!(
            "  {} [{} {:.2}] {verb}{note}",
            a.candidate_id, a.verdict, a.confidence
        ));
    }
    if r.dry_run {
        tui::emit_line("\nDry run — nothing was written. Re-run with `--apply` to act on this.");
    } else {
        tui::emit_line("\nEvery retirement is reversible: `aizen memory revive <id>`.");
    }
}

/// `memory doctor` — everything about the store that is true but invisible from the other listings.
///
/// The tier/anchor redesign traded a single opaque hash for a two-axis identity, which is easier to
/// reason about and easier to get *quietly* wrong: a place fact whose anchor no longer exists is
/// unreachable but looks perfectly healthy in `memory list`, and a `supersededBy` pointing at a
/// purged id hides a fact behind something that is not there. Both are silent by construction, so
/// something has to go looking.
///
/// Printing only — every judgement lives in [`doctor::diagnose`], which is pure and unit-tested.
/// Keeping the analysis out of here is not tidiness: an inline copy would drift from the collector
/// `reconcile` uses, and then `doctor` would report pairs the pass does not see (or the reverse),
/// which is worse than no report at all.
pub fn cmd_doctor() -> Result<()> {
    let all = store::load_all()?;
    let archived = bloat::caps::list_archive().unwrap_or_default();
    let queued = store::load_from(&config::review_dir()).unwrap_or_default();
    let lin = path_scope::Lineage::current();
    let today = bloat::decay::today();

    let r = doctor::diagnose(&all, &archived, &queued, &lin, &|p| {
        std::path::Path::new(p).exists()
    });
    let c = &r.counts;

    tui::emit_line(&format!(
        "counts: {} live · {} superseded · {} archived · {} awaiting review",
        c.live, c.superseded, c.archived, c.review
    ));
    tui::emit_line(&format!(
        "tiers:  {} user · {} device · {} place ({} anchored outside here)",
        c.user_tier, c.device_tier, c.place_tier, c.inapplicable_here
    ));
    // Device identity, spelled out: a `tier: device` fact is invisible when the id shifts, and the
    // id is derived from hardware, so "which id am I" is the first question a confused store raises.
    tui::emit_line(&format!(
        "device: {} (from {}{})",
        r.device_id,
        r.device_source,
        if r.device_also_read.is_empty() {
            String::new()
        } else {
            format!("; also reading {}", r.device_also_read.join(", "))
        }
    ));
    tui::emit_line(&format!("here:   {}", lin.cwd));

    if r.findings.is_empty() {
        tui::emit_line("\nNo structural problems found.");
    } else {
        tui::emit_line(&format!("\n{} finding(s):", r.findings.len()));
        for f in &r.findings {
            tui::emit_line(&format!("  ⌁ {}", f.describe()));
        }
    }

    if r.pending_pairs > 0 {
        let due =
            learning::reconcile::should_run(r.pending_pairs, r.last_reconcile.as_deref(), &today);
        tui::emit_line(&format!(
            "\n{} — `aizen memory reconcile` (dry run) shows the verdicts.{}",
            if due {
                "A reconciliation pass is due"
            } else {
                "Not yet due to run on its own"
            },
            match r.last_reconcile.as_deref() {
                Some(d) => format!(" Last pass: {d}."),
                None => " No pass has ever run.".to_string(),
            }
        ));
    }

    // A queue nobody reads is the failure mode R7 named, so say it out loud rather than counting it.
    if c.review >= 5 {
        tui::emit_line(&format!(
            "\n{} item(s) waiting in review — `aizen memory review` to work through them.",
            c.review
        ));
    }
    Ok(())
}

/// `memory health` — the three §8 metrics, week by week.
///
/// `doctor` answers "is anything broken right now"; this answers "is the design working over time",
/// which is a different question and needs a different input: a time series nothing in the entries
/// dir keeps. It reads `stats.jsonl` (one cumulative sample per session) and the learning audit, and
/// every number it prints comes from a pure function in [`stats`] — this is a printer.
///
/// Deliberately honest about thin data. Two weeks of history cannot confirm "growth flattens after
/// week 3", so it says so instead of drawing a conclusion from three points. A metric surface that
/// reports a trend it cannot support is worse than one that reports nothing.
pub fn cmd_health() -> Result<()> {
    let samples = stats::load();
    if samples.is_empty() {
        tui::emit_line(
            "No measurements yet. One sample is written per session that ran at least one turn, to",
        );
        tui::emit_line(&format!("  {}", stats::stats_path().display()));
        return Ok(());
    }

    let weeks = stats::weekly(&samples);
    let sat = stats::saturation(&weeks);
    let ratio = stats::use_ratio(&weeks);
    let audit = std::fs::read_to_string(learning::audit::audit_path()).unwrap_or_default();
    let contra = stats::contradictions_weekly(&audit);

    tui::emit_line(&format!(
        "{} sample(s) over {} week(s) of use.\n",
        samples.len(),
        weeks.last().map(|w| w.index).unwrap_or(0)
    ));
    tui::emit_line("week   live  +live/turn   used/injected   contradictions");
    for (i, w) in weeks.iter().enumerate() {
        let rate = match sat[i] {
            Some(v) => format!("{v:>10.3}"),
            None => "         —".to_string(),
        };
        let use_r = match ratio[i] {
            Some(v) => format!("{v:>14.2}"),
            None => "             —".to_string(),
        };
        let n = contra
            .iter()
            .find(|(idx, _)| *idx == w.index)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        tui::emit_line(&format!(
            "{:>4}  {:>5}  {rate}  {use_r}   {n:>13}",
            w.index, w.live_end
        ));
    }

    // Metric 1's verdict, and only when there is enough history to have one. The plan's claim is
    // about week 3 onward, so four weeks is the floor for saying anything at all.
    tui::emit_line("");
    if weeks.len() < 4 {
        tui::emit_line(
            "Too little history to judge saturation — the claim is about week 3 onward. Keep using it.",
        );
    } else if stats::is_flattening(&weeks, 3) {
        tui::emit_line(
            "Saturation: HOLDING — growth per turn is falling while superseded+review keeps rising.",
        );
    } else {
        tui::emit_line(
            "Saturation: NOT holding — the store is still growing per turn, or nothing is being resolved.",
        );
    }

    // Metric 2 against its stated bar. Read over the whole span rather than the last week: one quiet
    // week swings a weekly ratio hard, and the target ("≥0.35 and rising") is about the trend.
    let (inj, used): (u64, u64) = weeks
        .iter()
        .fold((0, 0), |(a, b), w| (a + w.d_injected, b + w.d_used));
    if inj == 0 {
        tui::emit_line("Recall precision: no facts injected yet — nothing to measure.");
    } else {
        let r = used as f64 / inj as f64;
        tui::emit_line(&format!(
            "Recall precision: {r:.2} overall ({used}/{inj}) — target ≥0.35. {}",
            if r >= 0.35 {
                "At bar."
            } else {
                "Below bar: the relevance gate or `k` is too loose."
            }
        ));
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
    matches!(
        std::env::var(key).ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
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
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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

    /// A temp home for the recall tests: they read the real store, so they need one of their own.
    fn with_recall_home<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-recall-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // create_dir_all BEFORE any slug/anchor is computed: canonicalize() of a missing dir fails
        // and yields a different key than once it exists, silently splitting the zone.
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("NEXTGEN_HOME", &dir);
        std::env::set_var("NG_PROJECT_ROOT", &dir);
        pending::clear();
        let out = f();
        pending::clear();
        std::env::remove_var("NG_PROJECT_ROOT");
        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    /// Seed a durable, core-INELIGIBLE user fact, so it stays in the searchable tail where the
    /// recall block reads from (a curated user fact would be resident and excluded from search).
    fn seed_recallable(name: &str, body: &str) {
        store::add_learned(&store::LearnedWrite {
            name,
            body,
            mtype: MemoryType::User,
            source: ProvenanceKind::Inferred, // 1 session → not core-trusted → searchable
            tier: path_scope::Tier::User,
            ..Default::default()
        })
        .unwrap();
    }

    #[test]
    fn recall_block_is_none_below_the_relevance_gate() {
        with_recall_home("gate", || {
            seed_recallable(
                "pnpm",
                "the user prefers pnpm over npm for package installs",
            );

            // A query that shares one incidental token with the fact. BM25 scores it above zero, so
            // without the gate a block would ride along on turns like this — which is most turns.
            assert!(
                recall_block("the user asked about something else entirely", 300).is_none(),
                "a weak lexical brush must not spend tokens"
            );

            // A query that genuinely covers the fact does inject.
            let (block, pairs) = recall_block("does the user prefer pnpm or npm", 300)
                .expect("a confident match injects");
            assert!(
                block.starts_with(RECALL_MARKER),
                "block must be marker-prefixed: {block}"
            );
            assert!(block.contains("pnpm"));
            assert_eq!(pairs.len(), 1);
            assert_eq!(pairs[0].handle, "m1", "handles are positional from m1");
        });
    }

    #[test]
    fn recall_block_breaks_on_budget_so_handles_stay_contiguous() {
        with_recall_home("budget", || {
            // Three facts that all match the query strongly.
            for i in 1..=3 {
                seed_recallable(
                    &format!("deploy-{i}"),
                    &format!("deploy note {i}: the deploy pipeline uses fly for staging and prod"),
                );
            }
            // A budget that fits the header plus roughly one line. The header is a fixed ~26-token
            // cost, so anything much under ~50 leaves room for nothing and returns None.
            let (block, pairs) = recall_block("deploy pipeline fly", 60).expect("something fits");
            assert!(
                !pairs.is_empty() && pairs.len() < 3,
                "budget must bite: {} lines",
                pairs.len()
            );
            // The handles present must be exactly m1..mN with no gap — that is what `break` (rather
            // than `continue`) buys: a hole like [m1] [m3] would make `used: ["m2"]` unresolvable.
            for (i, p) in pairs.iter().enumerate() {
                assert_eq!(
                    p.handle,
                    format!("m{}", i + 1),
                    "handles must be contiguous: {pairs:?}"
                );
                assert!(block.contains(&format!("[{}]", p.handle)));
            }
        });
    }

    #[test]
    fn recall_block_is_skipped_when_the_selection_did_not_change() {
        with_recall_home("delta", || {
            seed_recallable(
                "pnpm",
                "the user prefers pnpm over npm for package installs",
            );
            let q = "does the user prefer pnpm or npm";

            let (_, pairs) = recall_block(q, 300).expect("first turn injects");
            pending::open_turn(pairs); // the caller seats the ledger; do the same here

            // Same question next turn → same facts → the model already has them in the transcript.
            assert!(
                recall_block(q, 300).is_none(),
                "an unchanged selection must not be re-folded"
            );

            // A new fact changes the selection, so the block returns.
            seed_recallable("npm-ban", "npm install is forbidden in this org, use pnpm");
            assert!(
                recall_block(q, 300).is_some(),
                "a changed selection injects again"
            );
        });
    }

    #[test]
    fn recall_block_labels_each_fact_with_its_axis() {
        with_recall_home("labels", || {
            // The label is what tells the model whether a fact is about THEM, this machine, or here
            // — without it, a device-specific path reads as a universal truth.
            let mut e = MemoryEntry {
                id: "x".into(),
                ..Default::default()
            };
            e.tier = path_scope::Tier::User;
            assert_eq!(tier_label(&e), "about you");
            e.tier = path_scope::Tier::Device;
            assert_eq!(tier_label(&e), "this machine");
            e.tier = path_scope::Tier::Place;
            assert_eq!(tier_label(&e), "here");
        });
    }

    #[test]
    fn admits_is_keyed_on_the_tier_axis_not_on_scope() {
        // Invariant I4: ONE read predicate, and it reads `tier`/`anchor`. This has to be asserted
        // directly because the write path now emits `scope: None` for everything — a `scope`-based
        // filter would therefore admit every place fact in every directory (exactly the pollution
        // the anchor axis exists to prevent) while every existing test still passed.
        let lin = path_scope::Lineage {
            cwd: "c:/work/proj/src".into(),
            places: vec!["c:/work/proj/src".into()],
            device: "dev-aaaaaaaa".into(),
            home: Some("c:/users/admin".into()),
        };
        let entry = |tier: path_scope::Tier| MemoryEntry {
            id: "e".into(),
            tier,
            ..Default::default()
        };

        let user = entry(path_scope::Tier::User);
        let mut here = entry(path_scope::Tier::Place);
        here.anchor = Some("c:/work/proj".into());
        let mut elsewhere = entry(path_scope::Tier::Place);
        elsewhere.anchor = Some("c:/work/other".into());
        // A legacy zone slug is a hash of a path, so it cannot be resolved back to a directory.
        let orphan = entry(path_scope::Tier::Place); // Place with anchor: None
        let mut mine = entry(path_scope::Tier::Device);
        mine.device = Some("dev-aaaaaaaa".into());
        let mut theirs = entry(path_scope::Tier::Device);
        theirs.device = Some("dev-bbbbbbbb".into());

        // The working view: everything true HERE, and nothing else.
        let cur = ScopeSel::Current;
        assert!(cur.admits(&user, &lin), "a user fact is true everywhere");
        assert!(cur.admits(&here, &lin), "an ancestor anchor covers the cwd");
        assert!(
            cur.admits(&mine, &lin),
            "this machine's device fact applies"
        );
        assert!(
            !cur.admits(&elsewhere, &lin),
            "another tree's fact must not leak in"
        );
        assert!(
            !cur.admits(&theirs, &lin),
            "another machine's fact must not leak in"
        );
        assert!(
            !cur.admits(&orphan, &lin),
            "an unresolvable place fails CLOSED"
        );

        // `Global` is the frozen-core view: person-level facts only.
        let glob = ScopeSel::Global;
        assert!(glob.admits(&user, &lin));
        assert!(!glob.admits(&here, &lin) && !glob.admits(&mine, &lin));

        // `All` is the human inspection view — it hides nothing, including the orphan.
        let all = ScopeSel::All;
        for e in [&user, &here, &elsewhere, &orphan, &mine, &theirs] {
            assert!(all.admits(e, &lin), "All must hide nothing");
        }

        // A `--scope <slug>` lookup still matches on the legacy tag: that is the ONLY way a human
        // can still reach an orphan place fact, since no lineage will ever admit it.
        let mut tagged = entry(path_scope::Tier::Place);
        tagged.scope = Some("legacy-0a1b2c3d".into());
        let by_zone = ScopeSel::Project("legacy-0a1b2c3d".into());
        assert!(by_zone.admits(&tagged, &lin));
        assert!(!by_zone.admits(&here, &lin));
    }

    #[test]
    fn remember_defaults_to_project_zone_and_global_prefix_overrides() {
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-remember-scope-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("NEXTGEN_HOME", &dir);
        std::env::set_var("NG_PROJECT_ROOT", &dir);

        let id = remember("the api base is internal dot example").unwrap();
        let find = |id: &str| {
            store::load_all()
                .unwrap()
                .into_iter()
                .find(|e| e.id == id)
                .unwrap()
        };
        assert_eq!(
            find(&id).scope,
            Some(config::project_slug()),
            "a fact typed inside a project lands in its zone"
        );

        let gid = remember("GLOBAL: reply tersely everywhere").unwrap();
        let g = find(&gid);
        assert!(g.scope.is_none(), "global: prefix pins the fact global");
        assert!(
            !g.body.to_lowercase().contains("global:"),
            "marker stripped from the body"
        );

        std::env::remove_var("NG_PROJECT_ROOT");
        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inventory_lists_without_a_query_and_edit_forget_restore_round_trip() {
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-inventory-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // create_dir_all BEFORE project_slug() is ever computed: canonicalize() of a missing dir
        // fails and yields a DIFFERENT slug than once it exists, which silently splits the zone.
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("NEXTGEN_HOME", &dir);
        std::env::set_var("NG_PROJECT_ROOT", &dir);

        store::add(
            "pnpm over npm",
            "package manager",
            MemoryType::User,
            "the user prefers pnpm",
        )
        .unwrap();
        store::add(
            "deploys from ci",
            "",
            MemoryType::Project,
            "release runs in github actions",
        )
        .unwrap();

        // The whole point: enumerate what is stored with NO search query.
        let inv = inventory(&ScopeSel::All, None, 50, false).unwrap();
        assert!(
            inv.contains("pnpm-over-npm"),
            "inventory addresses entries by id: {inv}"
        );
        assert!(inv.contains("deploys-from-ci"));
        assert!(
            inv.contains("[user]") && inv.contains("[project]"),
            "grouped by type: {inv}"
        );
        assert!(inv.contains("2 stored"));
        // an id-less entry still shows a body head rather than an empty line
        assert!(
            inv.contains("release runs in github actions"),
            "empty description falls back to body"
        );

        // type filter narrows it
        let only_user = inventory(&ScopeSel::All, Some(MemoryType::User), 50, false).unwrap();
        assert!(only_user.contains("pnpm-over-npm") && !only_user.contains("deploys-from-ci"));

        // edit: named fields change, unnamed survive, id is stable
        cmd_edit(
            "pnpm-over-npm",
            None,
            Some("the package manager to use".into()),
            Some("feedback".into()),
            None,
            None,
        )
        .unwrap();
        let e = resolve_entry("pnpm-over-npm").unwrap();
        assert_eq!(e.id, "pnpm-over-npm", "the id never changes on edit");
        assert_eq!(e.description, "the package manager to use");
        assert_eq!(e.mtype, MemoryType::Feedback, "type was retyped");
        assert_eq!(
            e.body, "the user prefers pnpm",
            "body untouched by a description-only edit"
        );
        assert!(e.updated.is_some(), "edit stamps updated");

        // an unknown type is REJECTED rather than silently coerced to `reference`
        assert!(
            cmd_edit(
                "pnpm-over-npm",
                None,
                None,
                Some("nonsense".into()),
                None,
                None
            )
            .is_err(),
            "a typo'd type must not silently retype the fact"
        );

        // forget = recoverable archive, not destruction
        cmd_forget("pnpm-over-npm").unwrap();
        assert!(
            resolve_entry("pnpm-over-npm").is_err(),
            "gone from the live store"
        );
        let arch = inventory(&ScopeSel::All, None, 50, true).unwrap();
        assert!(
            arch.contains("pnpm-over-npm"),
            "…but present in the archive: {arch}"
        );

        cmd_restore("pnpm-over-npm", None).unwrap();
        assert!(
            resolve_entry("pnpm-over-npm").is_ok(),
            "restore brings it back"
        );

        // purge only ever touches the archive
        assert!(
            cmd_purge("pnpm-over-npm").is_err(),
            "a LIVE fact cannot be purged directly"
        );
        cmd_forget("pnpm-over-npm").unwrap();
        cmd_purge("pnpm-over-npm").unwrap();
        assert!(
            bloat::caps::list_archive()
                .unwrap()
                .iter()
                .all(|e| e.id != "pnpm-over-npm"),
            "purge is the one irreversible step"
        );

        std::env::remove_var("NG_PROJECT_ROOT");
        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn review_clear_and_drop_move_to_discarded_instead_of_deleting() {
        // The review queue is the ONLY copy of a mid-confidence candidate. Rejecting one is a
        // judgement call about relevance, not permission to destroy the text — so `--clear` (and
        // the new `--drop`) must set files aside, not `remove_dir_all` the queue.
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-review-discard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("NEXTGEN_HOME", &dir);
        std::env::set_var("NG_PROJECT_ROOT", &dir);

        let rdir = config::review_dir();
        let queue = |body: &str, name: &str| {
            let w = store::LearnedWrite {
                name,
                description: "",
                mtype: MemoryType::User,
                body,
                source: ProvenanceKind::Inferred,
                confidence: 0.6,
                session_id: "t",
                no_core: false,
                scope: None,
                subpath: None,
                ..Default::default()
            };
            store::add_learned_in(&rdir, &w).unwrap()
        };
        let keep = queue("the user deploys on fridays", "fridays");
        let doomed = queue("the user might prefer tabs", "tabs");

        cmd_review(None, Some(doomed.clone()), false).unwrap();
        let live: Vec<String> = store::load_from(&rdir)
            .unwrap()
            .into_iter()
            .map(|e| e.id)
            .collect();
        assert_eq!(
            live,
            vec![keep.clone()],
            "--drop removes exactly one from the live queue"
        );
        assert!(
            std::fs::read_dir(rdir.join(".discarded"))
                .unwrap()
                .flatten()
                .count()
                == 1,
            "the dropped candidate is set aside, recoverable"
        );

        cmd_review(None, None, true).unwrap();
        assert!(
            store::load_from(&rdir).unwrap().is_empty(),
            "--clear empties the live queue"
        );
        let set_aside: Vec<String> = std::fs::read_dir(rdir.join(".discarded"))
            .unwrap()
            .flatten()
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .collect();
        assert_eq!(set_aside.len(), 2, "both candidates survive as files");
        assert!(
            set_aside.iter().any(|c| c.contains("fridays")),
            "cleared text is still readable"
        );

        std::env::remove_var("NG_PROJECT_ROOT");
        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_reports_ambiguity_instead_of_picking_one() {
        let mk = |id: &str| MemoryEntry {
            id: id.to_string(),
            name: id.to_string(),
            ..Default::default()
        };
        let all = vec![mk("deploy-staging"), mk("deploy-prod"), mk("pnpm")];

        // exact id wins over any substring candidates
        assert_eq!(
            resolve_in(all.clone(), "deploy-prod").unwrap().id,
            "deploy-prod"
        );
        // a unique substring resolves
        assert_eq!(resolve_in(all.clone(), "pnp").unwrap().id, "pnpm");
        // an ambiguous one must NOT silently edit/delete the wrong fact
        let err = resolve_in(all.clone(), "deploy").unwrap_err().to_string();
        assert!(
            err.contains("matches 2 memories"),
            "ambiguity is reported: {err}"
        );
        assert!(resolve_in(all, "nothing-like-this").is_err());
    }

    #[test]
    fn strip_global_prefix_matches_markers_only_and_is_utf8_safe() {
        assert_eq!(strip_global_prefix("global: x").map(str::trim), Some("x"));
        assert_eq!(strip_global_prefix("G: x").map(str::trim), Some("x"));
        assert!(strip_global_prefix("globally speaking").is_none());
        assert!(
            strip_global_prefix("gõ tiếng Việt nhanh").is_none(),
            "multi-byte head must not panic"
        );
    }

    #[test]
    fn search_scoped_current_hides_foreign_zones() {
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-scope-search-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("NEXTGEN_HOME", &dir);
        std::env::set_var("NG_PROJECT_ROOT", &dir);
        let cur = config::project_slug();

        store::add_scoped(
            "here fact",
            "",
            MemoryType::Reference,
            "the deploy pipeline uses fly",
            Some(&cur),
        )
        .unwrap();
        store::add_scoped(
            "foreign fact",
            "",
            MemoryType::Reference,
            "the deploy pipeline uses render",
            Some("otherproj-11111111"),
        )
        .unwrap();
        // A global fact that is NOT core-trusted (inferred, seen in one session), so it stays in
        // the searchable tail. `store::add` would write a curated global fact, which is now
        // core-resident — and search deliberately excludes whatever the always-on block already
        // carries, so it would be absent here for a reason that has nothing to do with zones.
        store::add_learned(&store::LearnedWrite {
            name: "global fact",
            body: "always deploy carefully",
            mtype: MemoryType::Feedback,
            source: ProvenanceKind::Inferred,
            tier: crate::memory::path_scope::Tier::User,
            ..Default::default()
        })
        .unwrap();

        let current = search_scoped("deploy pipeline", 10, &ScopeSel::Current).unwrap();
        assert!(
            current
                .iter()
                .any(|h| h.entry.scope.as_deref() == Some(cur.as_str())),
            "current zone visible"
        );
        assert!(
            current.iter().any(|h| h.entry.scope.is_none()),
            "global visible"
        );
        assert!(
            current
                .iter()
                .all(|h| h.entry.scope.as_deref() != Some("otherproj-11111111")),
            "another project's facts are invisible in the working view"
        );

        let all = search_filtered_scoped("deploy pipeline", 10, None, &ScopeSel::All).unwrap(); // human view
        assert!(
            all.iter()
                .any(|h| h.entry.scope.as_deref() == Some("otherproj-11111111")),
            "All sees every zone"
        );

        let global_only = search_scoped("deploy", 10, &ScopeSel::Global).unwrap();
        assert!(!global_only.is_empty() && global_only.iter().all(|h| h.entry.scope.is_none()));

        std::env::remove_var("NG_PROJECT_ROOT");
        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn remember_slug_is_kebab_and_bounded() {
        assert_eq!(
            remember_slug("Prefer pnpm over npm"),
            "prefer-pnpm-over-npm"
        );
        let s = remember_slug("  the API base is https://x/v1  ");
        assert!(s.starts_with("the-api-base-is"), "got {s}");
        assert!(
            !s.contains(' ') && !s.starts_with('-') && !s.ends_with('-'),
            "got {s}"
        );
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
        let score_of = |hits: &[Hit]| {
            hits.iter()
                .find(|h| h.entry.id == "a")
                .map(|h| h.score)
                .unwrap_or(0.0)
        };
        assert!(
            score_of(&fuzzy) > score_of(&exact),
            "fuzzy bridges the typo → higher score on 'a'"
        );
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
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
        let hits = search_hybrid_in(
            "oauth login",
            usize::MAX,
            entries,
            &HashSet::new(),
            &caching,
        );
        caching.cache.borrow().save();
        assert!(!hits.is_empty(), "dense⊕lexical fusion returns hits");
        assert_eq!(
            hits[0].entry.id, "b",
            "the auth doc wins for an oauth query"
        );
        assert!(
            config::embed_cache_dir()
                .join(format!("{}.json", inner.id()))
                .exists(),
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
        assert!(
            lexical_coverage(&partial, &hit) < 0.5,
            "only 1/3 tokens covered"
        );
        assert_eq!(lexical_coverage(&HashSet::new(), &hit), 0.0);
    }

    #[test]
    fn gate_closed_on_a_confident_literal_match_skips_dense() {
        // When the top lexical hit fully covers the query, the gate stays CLOSED: the result is the
        // pure lexical order and the (adversarial) embedder is never allowed to reorder it. Uses an
        // embedder that would rank the WRONG doc first if consulted, so a closed gate is observable.
        struct AdversarialEmbedder;
        impl embed::Embedder for AdversarialEmbedder {
            fn id(&self) -> String {
                "adversarial".into()
            }
            fn dim(&self) -> usize {
                4
            }
            // Constant vector → cosine identical for every doc, so dense contributes a stable but
            // meaningless ranking; if it were fused it would inject the non-matching doc as noise.
            fn embed(&self, _t: &str) -> Vec<f32> {
                vec![1.0, 0.0, 0.0, 0.0]
            }
        }
        let entries = vec![
            entry("hit", "auth login oauth jwt session refresh"),
            entry("noise", "postgres index tuning and query plans"),
        ];
        // "oauth login" is fully covered by "hit" → coverage 1.0 ≥ gate 0.6 → gate CLOSED.
        let gated = search_hybrid_gated_in(
            "oauth login",
            usize::MAX,
            entries.clone(),
            &HashSet::new(),
            &AdversarialEmbedder,
            0.6,
        );
        assert_eq!(
            gated[0].entry.id, "hit",
            "confident literal match keeps lexical order"
        );
        assert_eq!(
            gated.len(),
            1,
            "only the lexically-matching doc is returned (dense not fused)"
        );
    }

    #[test]
    fn gate_open_on_low_coverage_fuses_the_dense_neighbor() {
        // A query the lexical floor barely covers opens the gate, so a dense-only hit surfaces.
        // The embedder returns the SAME vector for the query and the target doc (cosine 1.0) and an
        // orthogonal vector otherwise, so dense uniquely favors the intended doc.
        struct TargetedEmbedder;
        impl embed::Embedder for TargetedEmbedder {
            fn id(&self) -> String {
                "targeted".into()
            }
            fn dim(&self) -> usize {
                2
            }
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
            "ship",
            usize::MAX,
            entries,
            &HashSet::new(),
            &TargetedEmbedder,
            0.6,
        );
        assert!(
            !gated.is_empty(),
            "gate open → dense surfaces a semantic neighbor lexical missed"
        );
        assert_eq!(
            gated[0].entry.id, "deploy",
            "the dense-favored doc wins when the gate is open"
        );
    }

    #[test]
    fn category_filter_separates_bug_from_decision_end_to_end() {
        // P3: content-category is derived on load, so a search filtered to one category must return
        // only facts whose text classifies there — proven through the real store, not just classify().
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-cat-filter-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("NEXTGEN_HOME", &dir);

        use crate::memory::category::Category;
        // Place-anchored so neither lands in the always-on frozen core (which search excludes).
        // Residency is decided by TIER now, not by the type tag, so a non-`user` type is no longer
        // enough to keep a fact in the searchable tail — it has to be anchored to a place.
        let here = config::project_slug();
        store::add_scoped(
            "parser crash",
            "",
            MemoryType::Project,
            "the parser hit a null pointer panic on empty input",
            Some(&here),
        )
        .unwrap();
        store::add_scoped(
            "store design",
            "",
            MemoryType::Project,
            "we decided the architecture uses one store per zone by convention",
            Some(&here),
        )
        .unwrap();

        let unfiltered =
            search_filtered_scoped_cat("parser store", 10, None, None, &ScopeSel::All).unwrap();
        assert!(unfiltered.len() >= 2, "both facts match unfiltered");

        let bugs = search_filtered_scoped_cat(
            "parser store",
            10,
            None,
            Some(Category::BugHistory),
            &ScopeSel::All,
        )
        .unwrap();
        assert!(
            !bugs.is_empty()
                && bugs
                    .iter()
                    .all(|h| h.entry.category == Category::BugHistory),
            "bug filter returns only bug-history"
        );
        assert!(bugs.iter().any(|h| h.entry.body.contains("panic")));

        let decisions = search_filtered_scoped_cat(
            "parser store",
            10,
            None,
            Some(Category::ArchDecision),
            &ScopeSel::All,
        )
        .unwrap();
        assert!(
            !decisions.is_empty()
                && decisions
                    .iter()
                    .all(|h| h.entry.category == Category::ArchDecision),
            "decision filter returns only arch-decision"
        );
        assert!(
            decisions.iter().all(|h| !h.entry.body.contains("panic")),
            "the bug is excluded from the decision view"
        );

        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scoped_search_separates_same_keyword_by_dimension() {
        // B1 falsification: two facts both lexically match "package", but one is tooling
        // (pnpm) and one is stack (rust) — dimension scoping must return only the right one.
        let _g = config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-scoped-dim-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("NEXTGEN_HOME", &dir);

        // Place-anchored so neither lands in the always-on frozen core (which search excludes).
        // The type tag no longer decides residency — TIER does — so "not `user` type" is not enough
        // to keep a fact in the searchable tail; it has to be anchored to a place.
        let here = config::project_slug();
        store::add_scoped(
            "prefer pnpm",
            "",
            MemoryType::Feedback,
            "I prefer pnpm as my package manager",
            Some(&here),
        )
        .unwrap();
        store::add_scoped(
            "project stack",
            "",
            MemoryType::Project,
            "the package is built with rust and tokio",
            Some(&here),
        )
        .unwrap();

        let unscoped = search_scoped("package", 10, &ScopeSel::All).unwrap();
        assert!(unscoped.len() >= 2, "both facts match 'package' unscoped");

        let tooling =
            search_filtered_scoped("package", 10, Some(Dimension::Tooling), &ScopeSel::All)
                .unwrap();
        assert!(
            !tooling.is_empty()
                && tooling
                    .iter()
                    .all(|h| h.entry.dimension == Dimension::Tooling)
        );
        assert!(tooling.iter().any(|h| h.entry.body.contains("pnpm")));

        let stack =
            search_filtered_scoped("package", 10, Some(Dimension::Stack), &ScopeSel::All).unwrap();
        assert!(!stack.is_empty() && stack.iter().all(|h| h.entry.dimension == Dimension::Stack));
        assert!(
            stack.iter().all(|h| !h.entry.body.contains("pnpm")),
            "stack scope excludes the tooling fact"
        );

        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn precision_and_token_budget_hold_at_scale() {
        // 2000 unrelated distractors should not crowd a relevant fact out of the top-K,
        // and the injected slice stays under the token budget regardless of corpus size.
        let mut entries = Vec::with_capacity(2002);
        for i in 0..2000 {
            entries.push(entry(
                &format!("d{i}"),
                &format!("distractor entry {i} about miscellaneous unrelated chores"),
            ));
        }
        entries.push(entry(
            "target",
            "prefers pnpm over npm for package management",
        ));
        let hits = search_in("pnpm package management preference", 5, entries);
        assert!(hits.len() <= 5);
        assert!(
            hits.iter().any(|h| h.entry.id == "target"),
            "relevant fact must survive 2k distractors"
        );

        let top: Vec<MemoryEntry> = hits.into_iter().map(|h| h.entry).collect();
        let (block, _inc, _sp) = render::render_block("search", &top, settings().search_max_tokens);
        assert!(
            render::est_tokens(&block) <= settings().search_max_tokens,
            "per-turn injected tokens must stay bounded as the corpus grows"
        );
    }
}
