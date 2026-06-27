//! Standalone, best-for-CLI memory brain. See `.claude/plans/linked-riding-mochi.md`.
//!
//! P1: markdown store + lexical retrieval. Later phases add frozen-core/search tool (P2),
//! learning (P3), anti-bloat (P4), dense semantic tier (P5).

pub mod bloat;
pub mod dialectic;
pub mod dimension;
pub mod embed;
pub mod frontmatter;
pub mod frozen_core;
pub mod fuse;
pub mod learning;
pub mod profile;
pub mod provenance;
pub mod render;
pub mod score;
pub mod store;
pub mod tokenize;

use crate::config::{self, MemorySettings};
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

/// Live lazy search over the long-tail store for INJECTION INTO CONTEXT (the agent's
/// `memory_search` tool). Same ranking as `search_filtered`, **plus** it records implicit-reuse
/// reinforcement (the P8 evolution spine): every fact it returns into context is reinforced at
/// most once/day. `cmd_search` (human inspection) and the bench deliberately use the read-only
/// paths so only genuine context-injection grows the reuse signal.
pub fn search(query: &str, k: usize) -> Result<Vec<Hit>> {
    let hits = search_filtered(query, k, None)?;
    record_reuse(&hits); // best-effort; never fails the search
    Ok(hits)
}

/// Reinforce every returned fact (per-day deduped) — implicit-reuse signal. Best-effort: a
/// read-only store just means no signal this turn, never a failed retrieval.
fn record_reuse(hits: &[Hit]) {
    if hits.is_empty() {
        return;
    }
    let today = bloat::decay::today();
    for h in hits {
        let _ = store::record_retrieval(&h.entry, &today);
    }
}

/// Live search optionally restricted to one topical dimension (B1 scoped retrieval). Filtering
/// happens before decay/sort so the dimension scope can't be diluted by off-topic noise.
pub fn search_filtered(query: &str, k: usize, dim: Option<Dimension>) -> Result<Vec<Hit>> {
    let all = store::load_all()?;
    let active = bloat::supersede::active(&all);
    let cfg = settings();
    let core = frozen_core::build(&active, load_style().as_deref(), cfg.frozen_core_max_tokens);
    let exclude: HashSet<String> = core.source_ids.into_iter().collect();

    // Default path is the exact-BM25 lexical floor. `enable_dense` fuses a dense tier (RRF) with a
    // persistent per-fact embedding cache; `enable_fuzzy` adds the Jaro-Winkler bridge. Both OFF by
    // default — the moat tiers are now REACHABLE + integration-tested, not bench-only scaffolding.
    let mut hits = if cfg.enable_dense {
        let inner = embed::default_dense_embedder();
        let caching = CachingEmbedder {
            inner: inner.as_ref(),
            cache: std::cell::RefCell::new(embed::EmbeddingCache::load(&inner.id())),
        };
        let h = search_hybrid_in(query, usize::MAX, active, &exclude, &caching);
        caching.cache.borrow().save();
        h
    } else {
        rank_lexical(query, usize::MAX, active, &exclude, cfg.enable_fuzzy)
    };
    if let Some(d) = dim {
        hits.retain(|h| h.entry.dimension == d);
    }
    let today = bloat::decay::today();
    let half_life = cfg.recency_half_life_days;
    for h in &mut hits {
        // final = bm25 · decay · salience — facts rise/sink on reuse + reinforcement (P8).
        h.score = bloat::decay::evolved_score(h.score, &h.entry, &today, half_life);
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

/// Pure search excluding ids already served in the frozen core (the lazy long tail).
/// Ranking is BM25 (P7) — IDF + length normalization computed over the candidate set.
/// Lexical-only; the dense path is `search_hybrid_in`.
///
/// Exact BM25 (`score`, not `score_fuzzy`) is the shipped floor: it returns the same nonzero
/// candidate SET as a token-overlap scorer (so injected-noise is unchanged) while ranking it
/// better via IDF + length normalization. The fuzzy bridge is implemented + unit-proven but
/// default-OFF — on the current bench it adds candidate noise without a recall gain (the corpus
/// is saturated), failing its "no English regression" gate; same posture as the dense tier, one
/// flag from on once a typo-heavy fixture set proves net value.
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
pub fn remember(text: &str) -> Result<String> {
    let text = text.trim();
    if text.is_empty() {
        anyhow::bail!("nothing to remember");
    }
    let slug = remember_slug(text);
    let desc: String = text.chars().take(80).collect();
    store::add(&slug, &desc, MemoryType::Feedback, text)
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

pub fn cmd_list() -> Result<()> {
    let mut entries = store::load_all()?;
    let superseded = entries.iter().filter(|e| !e.is_active()).count();
    entries.retain(|e| e.is_active());
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    if entries.is_empty() {
        println!("(no active memories yet — `ng memory add ...`)");
        return Ok(());
    }
    for e in &entries {
        let desc = if e.description.is_empty() {
            String::new()
        } else {
            format!(" — {}", e.description)
        };
        println!("[{}] {}{}", e.mtype.as_str(), e.name, desc);
    }
    println!("\n{} memories", entries.len());
    if superseded > 0 {
        println!("({superseded} superseded — hidden; `ng memory as-of <date>` to view history)");
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

pub fn cmd_search(query: &str, k: usize, dimension: Option<String>) -> Result<()> {
    let dim = match &dimension {
        Some(s) => Some(Dimension::parse(s).ok_or_else(|| {
            anyhow::anyhow!("unknown dimension '{s}' (style|tooling|workflow|stack|other)")
        })?),
        None => None,
    };
    let hits = search_filtered(query, k, dim)?;
    if hits.is_empty() {
        let scope = dim.map(|d| format!(" in dimension '{}'", d.as_str())).unwrap_or_default();
        println!("(no matches for '{query}'{scope})");
        return Ok(());
    }
    for h in &hits {
        println!(
            "{:.3}  [{}/{}] {}",
            h.score,
            h.entry.mtype.as_str(),
            h.entry.dimension.as_str(),
            h.entry.name
        );
    }
    Ok(())
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
        println!("(frozen core empty — add `type=user` memories or a STYLE.md, e.g. `ng memory add me -t user -b \"...\"`)");
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
        println!("? review     {id} (run `ng memory review`){tag}");
    }
    for (fact, why) in &r.rejected {
        println!("✗ rejected   {fact}  — {why}");
    }
    for id in &r.archived {
        println!("⌁ archived   {id} (over inferred-cap; `ng memory restore {id}`)");
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
        None => println!("(no STYLE.md yet — learned via `ng memory learn` core-promotion, or edit {} directly)", config::style_path().display()),
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
        "\n{} item(s). Promote: `ng memory review --promote <id>`; discard all: `ng memory review --clear`",
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
    println!("superseded '{}' → '{}' (history kept; see `ng memory as-of <date>`)", old_e.id, new_e.id);
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
    println!("\n{} archived. Restore: `ng memory restore <id>`", arch.len());
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
    Ok(())
}

/// Settings accessor (verified-good defaults). The retrieval-tier toggles are reachable via env so
/// the fuzzy/dense moat can be turned on for a session without a config-schema change: set
/// `NG_MEM_FUZZY=1` (Jaro-Winkler bridge) and/or `NG_MEM_DENSE=1` (dense⊕lexical RRF; pair with a
/// `--features dense` build for a real semantic backend). Default OFF → the lexical floor ships.
pub fn settings() -> MemorySettings {
    let mut s = MemorySettings::default();
    if env_on("NG_MEM_FUZZY") {
        s.enable_fuzzy = true;
    }
    if env_on("NG_MEM_DENSE") {
        s.enable_dense = true;
    }
    s
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

        let unscoped = search("package", 10).unwrap();
        assert!(unscoped.len() >= 2, "both facts match 'package' unscoped");

        let tooling = search_filtered("package", 10, Some(Dimension::Tooling)).unwrap();
        assert!(!tooling.is_empty() && tooling.iter().all(|h| h.entry.dimension == Dimension::Tooling));
        assert!(tooling.iter().any(|h| h.entry.body.contains("pnpm")));

        let stack = search_filtered("package", 10, Some(Dimension::Stack)).unwrap();
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
