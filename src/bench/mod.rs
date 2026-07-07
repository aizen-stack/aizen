//! `ng bench memory` — anti-oracle recall bench.
//!
//! Discipline (the repo burned itself twice on corrupt oracles): acceptable sets are
//! HUMAN-LABELED, a reachability lint fails hard if an acceptable id doesn't exist in
//! the corpus, and paraphrase-tagged queries are exempt from the lexical-reachability
//! check (they exist precisely to measure the gap the dense tier (P5) will close).

pub mod brain;
pub mod loop_eval;
pub mod metrics;

use crate::memory::embed::{self, Embedder};
use crate::memory::{search_hybrid_gated_in, search_hybrid_in, search_in, search_in_fuzzy};
use crate::memory::store::{MemoryEntry, MemoryType};
use crate::memory::tokenize::tokenize;
use anyhow::{Context, Result};
use metrics::{aggregate, regressions, BenchMetrics};
use serde::Deserialize;
use std::collections::HashSet;
use std::path::PathBuf;

const MEMORIES: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/bench-fixtures/memories.jsonl"));
const Q_GATE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/bench-fixtures/queries.gate.jsonl"));
const Q_TUNE: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/bench-fixtures/queries.tune.jsonl"));

const GATE_EPSILON: f64 = 0.02;

#[derive(Debug, Deserialize)]
struct FixMemory {
    id: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(rename = "type", default)]
    mtype: String,
    #[serde(default)]
    body: String,
}

#[derive(Debug, Deserialize)]
struct FixQuery {
    id: String,
    query: String,
    #[serde(default)]
    acceptable: Vec<String>,
    #[serde(default)]
    tags: Vec<String>,
}

impl FixQuery {
    fn is_paraphrase(&self) -> bool {
        self.tags.iter().any(|t| t == "paraphrase")
    }
    fn is_vn(&self) -> bool {
        self.tags.iter().any(|t| t == "vn")
    }
}

fn parse_jsonl<T: for<'de> Deserialize<'de>>(s: &str, what: &str) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for (i, line) in s.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: T = serde_json::from_str(line)
            .with_context(|| format!("parsing {what} line {}", i + 1))?;
        out.push(v);
    }
    Ok(out)
}

fn corpus() -> Result<Vec<MemoryEntry>> {
    let fixes: Vec<FixMemory> = parse_jsonl(MEMORIES, "memories.jsonl")?;
    Ok(fixes
        .into_iter()
        .map(|f| {
            let tokens = tokenize(&format!("{}\n{}\n{}", f.name, f.description, f.body));
            MemoryEntry {
                id: f.id.clone(),
                path: PathBuf::from(format!("{}.md", f.id)),
                name: f.name,
                description: f.description,
                mtype: MemoryType::parse(&f.mtype),
                created: None,
                body: f.body,
                mtime_ms: 0,
                tokens,
                ..Default::default()
            }
        })
        .collect())
}

/// Reachability lint. Hard error if any acceptable id is missing from the corpus.
/// For non-paraphrase queries, also require ≥1 acceptable id to be lexically reachable.
fn lint(corpus: &[MemoryEntry], queries: &[FixQuery]) -> Result<()> {
    let ids: HashSet<&str> = corpus.iter().map(|e| e.id.as_str()).collect();
    let mut problems = Vec::new();
    for q in queries {
        for a in &q.acceptable {
            if !ids.contains(a.as_str()) {
                problems.push(format!(
                    "query {} references nonexistent memory id '{}' (corrupt oracle)",
                    q.id, a
                ));
            }
        }
        if !q.is_paraphrase() && !q.acceptable.is_empty() {
            let acc: HashSet<&str> = q.acceptable.iter().map(String::as_str).collect();
            let hits = search_in(&q.query, corpus.len(), corpus.to_vec());
            let reachable = hits.iter().any(|h| acc.contains(h.entry.id.as_str()));
            if !reachable {
                problems.push(format!(
                    "query {} ('{}') has NO lexically-reachable acceptable id — unreachable oracle (tag it 'paraphrase' if dense-only)",
                    q.id, q.query
                ));
            }
        }
    }
    if problems.is_empty() {
        Ok(())
    } else {
        anyhow::bail!("reachability lint failed:\n  - {}", problems.join("\n  - "))
    }
}

/// Which lexical ranking the bench scores. `Exact` is the shipped floor; `Fuzzy` adds the
/// Jaro-Winkler bridge (W24 measurement); the dense embedder path is orthogonal (`--hybrid`).
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rank {
    Exact,
    Fuzzy,
}

/// How the dense tier (when an embedder is supplied) is fused. `AlwaysOn` fuses dense on every
/// query (the ceiling the first P6 bench measured); `Gated(c)` fuses only when the top lexical hit
/// covers < `c` of the query tokens (the production `dense_gate_coverage` path), so a confident
/// literal match keeps its lexical precision.
#[derive(Clone, Copy)]
enum DenseMode {
    AlwaysOn,
    Gated(f64),
}

fn eval(
    corpus: &[MemoryEntry],
    queries: &[FixQuery],
    embedder: Option<&dyn Embedder>,
    rank: Rank,
    dense_mode: DenseMode,
) -> BenchMetrics {
    let empty = HashSet::new();
    let evals: Vec<(Vec<String>, HashSet<String>)> = queries
        .iter()
        .map(|q| {
            let ranked: Vec<String> = match embedder {
                Some(e) => {
                    let hits = match dense_mode {
                        DenseMode::AlwaysOn => {
                            search_hybrid_in(&q.query, 10, corpus.to_vec(), &empty, e)
                        }
                        DenseMode::Gated(c) => {
                            search_hybrid_gated_in(&q.query, 10, corpus.to_vec(), &empty, e, c)
                        }
                    };
                    hits.into_iter().map(|h| h.entry.id).collect()
                }
                None if rank == Rank::Fuzzy => search_in_fuzzy(&q.query, 10, corpus.to_vec())
                    .into_iter()
                    .map(|h| h.entry.id)
                    .collect(),
                None => search_in(&q.query, 10, corpus.to_vec())
                    .into_iter()
                    .map(|h| h.entry.id)
                    .collect(),
            };
            let acc: HashSet<String> = q.acceptable.iter().cloned().collect();
            (ranked, acc)
        })
        .collect();
    aggregate(&evals)
}

fn baseline_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("bench-fixtures/cli-baseline.json")
}

fn print_metrics(label: &str, m: &BenchMetrics) {
    println!(
        "{label:<22} n={:<3} recall@5={:.3} recall@10={:.3} mrr={:.3} prec@5={:.3} ndcg@5={:.3} noise={:.3}",
        m.query_count, m.recall_at_5, m.recall_at_10, m.mrr, m.precision_at_5, m.ndcg_at_5, m.noise_rate
    );
}

/// Entry point for `ng bench memory`.
pub fn run(split: &str, update_baseline: bool, hybrid: bool, fuzzy: bool) -> Result<()> {
    let corpus = corpus()?;
    let gate: Vec<FixQuery> = parse_jsonl(Q_GATE, "queries.gate.jsonl")?;
    let tune: Vec<FixQuery> = parse_jsonl(Q_TUNE, "queries.tune.jsonl")?;

    // Lint everything regardless of split — no corrupt/unreachable oracle ships.
    lint(&corpus, &gate)?;
    lint(&corpus, &tune)?;
    println!("reachability lint: OK ({} memories)", corpus.len());

    // The dense embedder used for `--hybrid`: the real model2vec backend when built with
    // `--features dense`, else the pure-Rust hashing embedder (plumbing only, not semantic).
    let emb: Option<Box<dyn Embedder>> = if hybrid {
        Some(embed::default_dense_embedder())
    } else {
        None
    };
    let emb_ref = emb.as_deref();

    if split == "tune" || split == "all" {
        // Split tune into paraphrase vs literal to expose the lexical ceiling (dense gap).
        let (para, lit): (Vec<_>, Vec<_>) = tune.iter().partition(|q| q.is_paraphrase());
        let lit: Vec<FixQuery> = lit.into_iter().map(clone_q).collect();
        let para: Vec<FixQuery> = para.into_iter().map(clone_q).collect();
        // Vietnamese-only paraphrase slice (P6 gate): the diacritic-shredding tokenizer bug hit
        // hardest here, so this is the number the NFC + Unicode-regex repair must move.
        let vn_para: Vec<FixQuery> = para.iter().filter(|q| q.is_vn()).map(clone_q).collect();
        let lex_para = eval(&corpus, &para, None, Rank::Exact, DenseMode::AlwaysOn);
        let lex_lit = eval(&corpus, &lit, None, Rank::Exact, DenseMode::AlwaysOn);
        print_metrics("tune (literal)", &lex_lit);
        print_metrics("tune (paraphrase)", &lex_para);
        if !vn_para.is_empty() {
            print_metrics("tune (paraphrase, vn)", &eval(&corpus, &vn_para, None, Rank::Exact, DenseMode::AlwaysOn));
        }
        println!("  ^ paraphrase recall is the lexical ceiling the dense tier must beat.");
        if let Some(e) = emb_ref {
            // Two dense regimes side by side (P6): ALWAYS-ON fuses dense on every query (the recall
            // ceiling, but it wrecks literal precision); GATED fuses only when BM25 is ambiguous
            // (low query coverage) — the production `dense_gate_coverage` path. The win we want:
            // gated keeps the always-on paraphrase recall lift WITHOUT the literal-slice noise.
            let gate_c = crate::core::config::MemorySettings::default().dense_gate_coverage;
            println!("  embedder = {}  (gate coverage < {:.2})", e.id(), gate_c);

            let on_para = eval(&corpus, &para, Some(e), Rank::Exact, DenseMode::AlwaysOn);
            let on_lit = eval(&corpus, &lit, Some(e), Rank::Exact, DenseMode::AlwaysOn);
            print_metrics("tune (lit, always-on)", &on_lit);
            print_metrics("tune (para, always-on)", &on_para);

            let gt_para = eval(&corpus, &para, Some(e), Rank::Exact, DenseMode::Gated(gate_c));
            let gt_lit = eval(&corpus, &lit, Some(e), Rank::Exact, DenseMode::Gated(gate_c));
            print_metrics("tune (lit, gated)", &gt_lit);
            print_metrics("tune (para, gated)", &gt_para);

            let on_recall = on_para.recall_at_5 - lex_para.recall_at_5;
            let gt_recall = gt_para.recall_at_5 - lex_para.recall_at_5;
            // Literal-slice noise cost of each regime vs the pure lexical floor.
            let on_lit_noise = on_lit.noise_rate - lex_lit.noise_rate;
            let gt_lit_noise = gt_lit.noise_rate - lex_lit.noise_rate;
            println!(
                "  DENSE (always-on): para recall@5 {:+.3}, literal noise {:+.3} (prec {:.3}→{:.3})",
                on_recall, on_lit_noise, lex_lit.precision_at_5, on_lit.precision_at_5
            );
            println!(
                "  DENSE (gated)    : para recall@5 {:+.3}, literal noise {:+.3} (prec {:.3}→{:.3})",
                gt_recall, gt_lit_noise, lex_lit.precision_at_5, gt_lit.precision_at_5
            );
            if gt_recall > 1e-9 && gt_lit_noise + 1e-9 < on_lit_noise {
                println!(
                    "  GATE WIN: gating keeps a paraphrase recall lift (+{:.3}) while cutting the literal-slice noise the always-on path added ({:+.3} → {:+.3}).",
                    gt_recall, on_lit_noise, gt_lit_noise
                );
            } else if gt_recall > 1e-9 {
                println!("  GATE: keeps a paraphrase recall lift (+{:.3}); literal-slice noise not improved over always-on.", gt_recall);
            } else {
                println!("  GATE: no paraphrase recall lift on this corpus — lexical stays the default.");
            }
        }
        if fuzzy {
            // W24 measurement: does the Jaro-Winkler bridge lift recall on literal/paraphrase
            // queries WITHOUT adding noise (dropping precision/ndcg)? Both tiers matter — a fuzzy
            // bridge that lifts recall by dragging in junk is a net loss, not a win.
            let fz_lit = eval(&corpus, &lit, None, Rank::Fuzzy, DenseMode::AlwaysOn);
            let fz_para = eval(&corpus, &para, None, Rank::Fuzzy, DenseMode::AlwaysOn);
            print_metrics("tune (lit, fuzzy)", &fz_lit);
            print_metrics("tune (para, fuzzy)", &fz_para);
            let recall_delta = fz_para.recall_at_5 - lex_para.recall_at_5;
            let noise_delta = fz_para.noise_rate - lex_para.noise_rate;
            println!(
                "  FUZZY: recall@5 {:+.3} ({:.3} → {:.3}), noise {:+.3} ({:.3} → {:.3})",
                recall_delta, lex_para.recall_at_5, fz_para.recall_at_5,
                noise_delta, lex_para.noise_rate, fz_para.noise_rate
            );
        }
    }

    if split == "gate" || split == "all" {
        // The gate always tracks the LEXICAL floor (the shipping default); --hybrid/--fuzzy are
        // measurement-only and never rewrite the baseline.
        let current = eval(&corpus, &gate, None, Rank::Exact, DenseMode::AlwaysOn);
        print_metrics("gate", &current);
        if hybrid {
            let gc = crate::core::config::MemorySettings::default().dense_gate_coverage;
            print_metrics("gate (hybrid, always-on)", &eval(&corpus, &gate, emb_ref, Rank::Exact, DenseMode::AlwaysOn));
            print_metrics("gate (hybrid, gated)", &eval(&corpus, &gate, emb_ref, Rank::Exact, DenseMode::Gated(gc)));
        }
        if fuzzy {
            let fz_gate = eval(&corpus, &gate, None, Rank::Fuzzy, DenseMode::AlwaysOn);
            print_metrics("gate (fuzzy)", &fz_gate);
            if fz_gate.recall_at_5 + 1e-9 < current.recall_at_5 || fz_gate.noise_rate > current.noise_rate + GATE_EPSILON {
                println!(
                    "  FUZZY GATE: would REGRESS the gate (recall {:.3}→{:.3}, noise {:.3}→{:.3}) — stay OFF by default.",
                    current.recall_at_5, fz_gate.recall_at_5, current.noise_rate, fz_gate.noise_rate
                );
            } else {
                println!("  FUZZY GATE: no regression on the gate split.");
            }
        }

        if update_baseline {
            let json = serde_json::to_string_pretty(&current)?;
            std::fs::write(baseline_path(), json + "\n")
                .with_context(|| format!("writing {}", baseline_path().display()))?;
            println!("baseline updated: {}", baseline_path().display());
            return Ok(());
        }

        match std::fs::read_to_string(baseline_path()) {
            Ok(s) => {
                let base: BenchMetrics =
                    serde_json::from_str(&s).context("parsing cli-baseline.json")?;
                let regs = regressions(&base, &current, GATE_EPSILON);
                if regs.is_empty() {
                    println!("GATE: PASS (vs baseline, eps {GATE_EPSILON})");
                } else {
                    eprintln!("GATE: FAIL");
                    for r in &regs {
                        eprintln!("  - {r}");
                    }
                    std::process::exit(1);
                }
            }
            Err(_) => {
                println!(
                    "no baseline yet — run `aizen bench memory --update-baseline` to capture one."
                );
            }
        }
    }
    Ok(())
}

// ── evolution gate (P8) ──────────────────────────────────────────────────────
//
// Proves the moat is real, not marketing: a memory that "evolves" must measurably retrieve
// better session-over-session from reuse alone. This is the falsifiable test — an inert engine
// (flat recall) FAILS. Zero tokens: the only signal is implicit reuse reinforcement.

use crate::memory::bloat::decay;
use crate::memory::provenance::ProvenanceKind;
use crate::memory::score::Bm25Index;

/// Rank a query over `corpus` by the PRODUCTION evolved score (`bm25 · decay · salience`).
fn evolved_rank(
    idx: &Bm25Index,
    corpus: &[MemoryEntry],
    query: &str,
    today: &str,
    half_life: f64,
    k: usize,
) -> Vec<String> {
    let q = tokenize(query);
    let mut scored: Vec<(String, f64)> = corpus
        .iter()
        .filter_map(|e| {
            let base = idx.score(&q, &e.tokens);
            if base <= 0.0 {
                return None;
            }
            Some((e.id.clone(), decay::evolved_score(base, e, today, half_life)))
        })
        .collect();
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0))
    });
    scored.into_iter().take(k).map(|(id, _)| id).collect()
}

/// `ng bench memory --evolution`. A 6-session reuse simulation: signal facts (genuinely-useful
/// answers, reused every session) vs distractors (short/dense → higher RAW BM25, never reused).
/// At session 0 ranking is pure BM25 so distractors fill the top-5 and recall starts low; each
/// session the signals are reinforced (salience ↑, kept fresh) while unused distractors age-decay
/// → signals climb into the top-5. The gate: recall@5 is monotonic (never forgets) and climbs
/// at a mean ≥5%/session until it plateaus.
pub fn run_evolution() -> Result<()> {
    const SESSIONS: usize = 6;
    const HALF_LIFE: f64 = 30.0;
    let query = "deploy production database backup schedule";

    let mk = |id: &str, body: &str| MemoryEntry {
        id: id.into(),
        path: PathBuf::from(format!("{id}.md")),
        name: id.into(),
        mtype: MemoryType::Reference,
        source: ProvenanceKind::Inferred, // curated facts never decay; these must, to model reuse
        body: body.into(),
        tokens: tokenize(body),
        created: Some("2026-01-01".into()),
        updated: Some("2026-01-01".into()),
        ..Default::default()
    };

    let mut corpus: Vec<MemoryEntry> = Vec::new();
    // distractors: bare query terms (short, dense) → out-rank signals on raw BM25; never reused.
    for i in 0..5 {
        corpus.push(mk(&format!("distractor-{i}"), query));
    }
    // signals: query + increasing unique filler (length-penalized → staggered BM25 so they cross
    // their distractors at different sessions → a gradual curve). These are the acceptable answers.
    let mut acceptable: HashSet<String> = HashSet::new();
    for i in 0..5 {
        let filler: Vec<String> = (0..(2 + i * 3)).map(|j| format!("ctx{i}fill{j}")).collect();
        corpus.push(mk(&format!("signal-{i}"), &format!("{query} {}", filler.join(" "))));
        acceptable.insert(format!("signal-{i}"));
    }

    let idx = Bm25Index::build(corpus.iter().map(|e| e.tokens.as_slice()));
    let base = chrono::NaiveDate::from_ymd_opt(2026, 1, 1).expect("valid date");

    println!(
        "evolution gate: {SESSIONS}-session reuse simulation ({} facts, {} reused signals, {} distractors)",
        corpus.len(),
        acceptable.len(),
        corpus.len() - acceptable.len()
    );
    let mut recalls: Vec<f64> = Vec::with_capacity(SESSIONS);
    for s in 0..SESSIONS {
        let today = (base + chrono::Duration::days(s as i64 * 7)).format("%Y-%m-%d").to_string();
        let ranked = evolved_rank(&idx, &corpus, query, &today, HALF_LIFE, 10);
        let recall = metrics::recall_at_k(&ranked, &acceptable, 5);
        recalls.push(recall);
        println!("  session {s} ({today}): recall@5 = {recall:.3}");
        // end-of-session reuse: the user retrieved the signals into context again (the spine).
        for e in corpus.iter_mut().filter(|e| acceptable.contains(&e.id)) {
            e.reinforced += 1;
            e.last_retrieved = Some(today.clone());
            e.updated = Some(today.clone()); // a reused fact stays fresh (no decay)
        }
    }
    evolution_gate(&recalls)
}

/// Gate: monotonic non-decreasing (memory never forgets) + a genuine climb averaging ≥5%/session
/// until plateau. An inert engine (flat recall) or a regressing one FAILS.
fn evolution_gate(recalls: &[f64]) -> Result<()> {
    const MIN_LIFT_PER_SESSION: f64 = 0.05;
    for w in recalls.windows(2) {
        if w[1] + 1e-6 < w[0] {
            anyhow::bail!(
                "EVOLUTION GATE: FAIL — recall regressed {:.3} -> {:.3} (memory must not forget)",
                w[0],
                w[1]
            );
        }
    }
    let first = *recalls.first().unwrap_or(&0.0);
    let peak = recalls.iter().copied().fold(0.0_f64, f64::max);
    let plateau = recalls.iter().position(|&r| r >= peak - 1e-6).unwrap_or(0);
    if plateau == 0 || peak <= first + 1e-9 {
        anyhow::bail!(
            "EVOLUTION GATE: FAIL — recall did not climb with reuse (first {first:.3}, peak {peak:.3}); the evolution engine is inert"
        );
    }
    let mean_lift = (peak - first) / plateau as f64;
    println!(
        "  peak recall@5 {peak:.3} at session {plateau}; mean lift {mean_lift:.3}/session (gate ≥ {MIN_LIFT_PER_SESSION:.2})"
    );
    if mean_lift + 1e-9 < MIN_LIFT_PER_SESSION {
        anyhow::bail!("EVOLUTION GATE: FAIL — mean lift {mean_lift:.3}/session < {MIN_LIFT_PER_SESSION:.2}");
    }
    println!(
        "EVOLUTION GATE: PASS — recall@5 climbed {first:.3} → {peak:.3} over {plateau} session(s) via implicit reuse (zero tokens)."
    );
    Ok(())
}

fn clone_q(q: &FixQuery) -> FixQuery {
    FixQuery {
        id: q.id.clone(),
        query: q.query.clone(),
        acceptable: q.acceptable.clone(),
        tags: q.tags.clone(),
    }
}
