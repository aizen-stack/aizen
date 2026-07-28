//! `ng bench profile` + `ng bench dialectic` — anti-oracle GOLDEN sets for the DERIVED brain
//! (the B2 profile rollup + the B3 dialectic Q&A). The recall bench (`bench memory`) measures a
//! tunable retrieval metric against a baseline; these two assert deterministic CORRECTNESS, so
//! they are golden sets: every human-labeled case MUST pass (no baseline file to drift).
//!
//! Discipline (the repo's twice-burned lesson on corrupt oracles): expectations are
//! HUMAN-LABELED in `bench-fixtures/{profile,dialectic-*}.jsonl`, a lint hard-fails on a
//! malformed/duplicate/invalid-enum case BEFORE evaluation, and the dialectic set explicitly
//! pins the load-bearing property — ABSTAIN-when-unknown (a hypothetical / out-of-corpus
//! question must NOT be answered).

use crate::memory::dialectic::{self, AbstainReason, AnswerKind};
use crate::memory::dimension;
use crate::memory::profile::{self, ProfileDim, UserProfile, Verdict};
use crate::memory::provenance::ProvenanceKind;
use crate::memory::store::{MemoryEntry, MemoryType};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashSet;

/// Fixed reference date so recency decay is neutral (facts are stamped the same day) → the
/// golden expectations are reproducible regardless of the wall clock.
const TODAY: &str = "2026-06-20";
const HALF_LIFE: f64 = 30.0;

const PROFILE_CASES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/bench-fixtures/profile.jsonl"
));
const DIALECTIC_MEMORIES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/bench-fixtures/dialectic-memories.jsonl"
));
const DIALECTIC_QUERIES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/bench-fixtures/dialectic-queries.jsonl"
));

#[derive(Debug, Deserialize)]
struct FixFact {
    #[serde(default)]
    name: String,
    body: String,
    #[serde(rename = "type", default)]
    mtype: String,
    #[serde(default)]
    source: String,
    #[serde(default)]
    reinforced: u32,
}

#[derive(Debug, Deserialize)]
struct ProfileCase {
    id: String,
    facts: Vec<FixFact>,
    dim: String,
    kind: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    value: Option<String>,
    #[serde(default)]
    contains: Vec<String>,
    #[serde(default)]
    excludes: Vec<String>,
    #[serde(default)]
    min_confidence: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct DialecticMem {
    id: String,
    body: String,
    #[serde(default)]
    reinforced: u32,
}

#[derive(Debug, Deserialize)]
struct DialecticQuery {
    id: String,
    query: String,
    kind: String,
    #[serde(default)]
    dim: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

fn parse_jsonl<T: for<'de> Deserialize<'de>>(s: &str, what: &str) -> Result<Vec<T>> {
    let mut out = Vec::new();
    for (i, line) in s.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        out.push(
            serde_json::from_str(line).with_context(|| format!("parsing {what} line {}", i + 1))?,
        );
    }
    Ok(out)
}

/// Build a `MemoryEntry` from a fixture fact (dimension classified on load, like the real store).
fn entry_from(
    i: usize,
    name: &str,
    body: &str,
    mtype: &str,
    source: &str,
    reinforced: u32,
) -> MemoryEntry {
    let nm = if name.is_empty() {
        format!("f{i}")
    } else {
        name.to_string()
    };
    let src = if source.is_empty() {
        ProvenanceKind::Manual
    } else {
        ProvenanceKind::parse(source)
    };
    let mt = if mtype.is_empty() { "user" } else { mtype };
    let dim = dimension::classify(&format!("{nm} {body}"));
    MemoryEntry {
        id: format!("c{i}"),
        name: nm,
        body: body.to_string(),
        mtype: MemoryType::parse(mt),
        source: src,
        confidence: 0.9,
        reinforced,
        created: Some(TODAY.to_string()),
        updated: Some(TODAY.to_string()),
        dimension: dim,
        ..Default::default()
    }
}

fn parse_dim(s: &str) -> Result<ProfileDim> {
    Ok(match s {
        "language" => ProfileDim::Language,
        "verbosity" => ProfileDim::Verbosity,
        "autonomy" => ProfileDim::Autonomy,
        "tooling" => ProfileDim::Tooling,
        "stack" => ProfileDim::Stack,
        "frustrations" => ProfileDim::Frustrations,
        other => bail!("unknown profile dimension '{other}'"),
    })
}

fn parse_reason(s: &str) -> Result<AbstainReason> {
    Ok(match s {
        "counterfactual_novel" => AbstainReason::CounterfactualNovel,
        "insufficient_evidence" => AbstainReason::InsufficientEvidence,
        "no_match" => AbstainReason::NoMatch,
        other => bail!("unknown abstain reason '{other}'"),
    })
}

// ── profile bench ──────────────────────────────────────────────────────────

fn lint_profile(cases: &[ProfileCase]) -> Result<()> {
    let mut seen = HashSet::new();
    for c in cases {
        if !seen.insert(c.id.as_str()) {
            bail!("duplicate profile case id '{}'", c.id);
        }
        parse_dim(&c.dim).with_context(|| format!("case {}", c.id))?;
        if !matches!(
            c.kind.as_str(),
            "scalar" | "choice" | "ranked" | "insufficient"
        ) {
            bail!("case {}: unknown expected kind '{}'", c.id, c.kind);
        }
        if c.facts.is_empty() {
            bail!("case {}: has no facts", c.id);
        }
    }
    Ok(())
}

fn eval_profile_case(case: &ProfileCase) -> std::result::Result<(), String> {
    let dim = parse_dim(&case.dim).map_err(|e| e.to_string())?;
    let entries: Vec<MemoryEntry> = case
        .facts
        .iter()
        .enumerate()
        .map(|(i, f)| entry_from(i, &f.name, &f.body, &f.mtype, &f.source, f.reinforced))
        .collect();
    let p = profile::build(&entries, TODAY, HALF_LIFE);
    let summary = p
        .dim(dim)
        .ok_or_else(|| format!("dimension {} missing", case.dim))?;

    match case.kind.as_str() {
        "insufficient" => {
            if !matches!(summary.verdict, Verdict::Insufficient) {
                return Err(format!("expected insufficient, got {:?}", summary.verdict));
            }
        }
        "scalar" => match &summary.verdict {
            Verdict::Scalar { label, .. } => {
                if let Some(exp) = &case.label {
                    if label != exp {
                        return Err(format!("scalar label '{label}' != expected '{exp}'"));
                    }
                }
            }
            other => return Err(format!("expected scalar, got {other:?}")),
        },
        "choice" => match &summary.verdict {
            Verdict::Choice {
                value, runner_up, ..
            } => {
                if let Some(exp) = &case.value {
                    if value != exp {
                        return Err(format!("choice '{value}' != expected '{exp}'"));
                    }
                }
                for c in &case.contains {
                    if value != c && runner_up.as_deref() != Some(c.as_str()) {
                        return Err(format!("choice missing expected '{c}'"));
                    }
                }
            }
            other => return Err(format!("expected choice, got {other:?}")),
        },
        "ranked" => match &summary.verdict {
            Verdict::Ranked { items } => {
                let names: Vec<&str> = items.iter().map(|(t, _)| t.as_str()).collect();
                for c in &case.contains {
                    if !names.iter().any(|n| n == c) {
                        return Err(format!("ranked missing '{c}' (got {names:?})"));
                    }
                }
                for e in &case.excludes {
                    if names.iter().any(|n| n == e) {
                        return Err(format!("ranked must exclude '{e}' (got {names:?})"));
                    }
                }
            }
            other => return Err(format!("expected ranked, got {other:?}")),
        },
        other => return Err(format!("unknown expected kind '{other}'")),
    }

    if let Some(mc) = case.min_confidence {
        if summary.confidence < mc {
            return Err(format!(
                "confidence {:.3} < min {:.3}",
                summary.confidence, mc
            ));
        }
    }
    Ok(())
}

/// Entry point for `ng bench profile`.
pub fn run_profile() -> Result<()> {
    let cases: Vec<ProfileCase> = parse_jsonl(PROFILE_CASES, "profile.jsonl")?;
    lint_profile(&cases)?;
    println!("profile bench: {} golden case(s)", cases.len());
    let mut failed = 0usize;
    for c in &cases {
        match eval_profile_case(c) {
            Ok(()) => println!("  ✓ {}", c.id),
            Err(e) => {
                eprintln!("  ✗ {} — {e}", c.id);
                failed += 1;
            }
        }
    }
    if failed == 0 {
        println!("PROFILE GATE: PASS ({n}/{n} golden cases)", n = cases.len());
        Ok(())
    } else {
        eprintln!("PROFILE GATE: FAIL ({failed}/{} cases)", cases.len());
        std::process::exit(1);
    }
}

// ── dialectic bench ────────────────────────────────────────────────────────

fn lint_dialectic(queries: &[DialecticQuery]) -> Result<()> {
    let mut seen = HashSet::new();
    for q in queries {
        if !seen.insert(q.id.as_str()) {
            bail!("duplicate dialectic query id '{}'", q.id);
        }
        if q.query.trim().is_empty() {
            bail!("query {}: empty", q.id);
        }
        match q.kind.as_str() {
            "profile" => {
                if let Some(d) = &q.dim {
                    parse_dim(d).with_context(|| format!("query {}", q.id))?;
                }
            }
            "evidence" => {}
            "abstain" => {
                if let Some(r) = &q.reason {
                    parse_reason(r).with_context(|| format!("query {}", q.id))?;
                }
            }
            other => bail!("query {}: unknown kind '{}'", q.id, other),
        }
    }
    Ok(())
}

fn eval_dialectic(
    p: &UserProfile,
    entries: &[MemoryEntry],
    q: &DialecticQuery,
) -> std::result::Result<(), String> {
    let a = dialectic::answer(p, entries, &q.query);
    match q.kind.as_str() {
        "profile" => {
            if !matches!(a.kind, AnswerKind::Profile { .. }) {
                return Err(format!("expected a profile answer, got {:?}", a.kind));
            }
            if let Some(ds) = &q.dim {
                let want = parse_dim(ds).map_err(|e| e.to_string())?;
                if a.dimension != Some(want) {
                    return Err(format!("expected dim {ds}, got {:?}", a.dimension));
                }
            }
        }
        "evidence" => {
            if !matches!(a.kind, AnswerKind::Evidence) {
                return Err(format!("expected an evidence answer, got {:?}", a.kind));
            }
        }
        "abstain" => match a.kind {
            AnswerKind::Abstain { reason } => {
                if let Some(rs) = &q.reason {
                    let want = parse_reason(rs).map_err(|e| e.to_string())?;
                    if reason != want {
                        return Err(format!("expected abstain {rs}, got {reason:?}"));
                    }
                }
            }
            other => {
                return Err(format!(
                    "MUST abstain (unknown/hypothetical), got {other:?}"
                ))
            }
        },
        other => return Err(format!("unknown expected kind '{other}'")),
    }
    Ok(())
}

/// Entry point for `ng bench dialectic`.
pub fn run_dialectic() -> Result<()> {
    let mems: Vec<DialecticMem> = parse_jsonl(DIALECTIC_MEMORIES, "dialectic-memories.jsonl")?;
    let queries: Vec<DialecticQuery> = parse_jsonl(DIALECTIC_QUERIES, "dialectic-queries.jsonl")?;
    lint_dialectic(&queries)?;
    let entries: Vec<MemoryEntry> = mems
        .iter()
        .enumerate()
        .map(|(i, m)| entry_from(i, &m.id, &m.body, "user", "manual", m.reinforced))
        .collect();
    let p = profile::build(&entries, TODAY, HALF_LIFE);

    println!(
        "dialectic bench: {} memory fact(s), {} golden query(ies)",
        entries.len(),
        queries.len()
    );
    let mut failed = 0usize;
    for q in &queries {
        match eval_dialectic(&p, &entries, q) {
            Ok(()) => println!("  ✓ {}", q.id),
            Err(e) => {
                eprintln!("  ✗ {} ('{}') — {e}", q.id, q.query);
                failed += 1;
            }
        }
    }
    if failed == 0 {
        println!(
            "DIALECTIC GATE: PASS ({n}/{n} golden cases incl. abstain-when-unknown)",
            n = queries.len()
        );
        Ok(())
    } else {
        eprintln!("DIALECTIC GATE: FAIL ({failed}/{} cases)", queries.len());
        std::process::exit(1);
    }
}

// ── health bench (§8 measurements) ─────────────────────────────────────────

const HEALTH_CASES: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/bench-fixtures/health.jsonl"
));

#[derive(Debug, Deserialize)]
struct HealthSample {
    ts: String,
    live: usize,
    #[serde(default)]
    superseded: usize,
    #[serde(default)]
    review: usize,
    turns: u64,
    injected: u64,
    used: u64,
}

#[derive(Debug, Deserialize)]
struct HealthExpect {
    /// The week indices the series must resolve to. Pins the "gaps are not invented" rule: a
    /// zero-filled silent week would show up here as an extra index.
    weeks: Vec<usize>,
    flattening: bool,
    flattening_tail: usize,
    /// Overall `used/injected` must be ≥ this (a store whose recall earns its budget).
    #[serde(default)]
    use_ratio_min: Option<f64>,
    /// …or < this (a store whose recall does not — metric 2 must be able to fail alone).
    #[serde(default)]
    use_ratio_max: Option<f64>,
    contradictions: Vec<(usize, usize)>,
    /// Which week the contradiction count peaks in. §8 predicts weeks 2–4.
    #[serde(default)]
    contradictions_peak_week: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct HealthCase {
    id: String,
    /// Human rationale. Required and linted non-empty: an oracle nobody can explain is the exact
    /// thing this repo has been burned by twice.
    why: String,
    samples: Vec<HealthSample>,
    audit: Vec<serde_json::Value>,
    expect: HealthExpect,
}

fn lint_health(cases: &[HealthCase]) -> Result<()> {
    let mut seen = HashSet::new();
    for c in cases {
        if !seen.insert(c.id.as_str()) {
            bail!("duplicate health case id '{}'", c.id);
        }
        if c.why.trim().len() < 20 {
            bail!("case {}: 'why' must explain what the case pins", c.id);
        }
        if c.samples.len() < 2 {
            bail!("case {}: a trend needs at least two samples", c.id);
        }
        if c.expect.flattening_tail < 2 {
            bail!("case {}: flattening_tail < 2 is not a trend", c.id);
        }
        // Cumulative counters must be monotonic, or the fixture describes something the writer
        // cannot produce and the case proves nothing about real data.
        for w in c.samples.windows(2) {
            if w[1].turns < w[0].turns || w[1].injected < w[0].injected || w[1].used < w[0].used {
                bail!(
                    "case {}: totals must be cumulative (they only ever grow)",
                    c.id
                );
            }
            if w[1].used > w[1].injected {
                bail!("case {}: used exceeds injected", c.id);
            }
            if crate::memory::stats::sample_date(&w[1].ts).is_none() {
                bail!("case {}: unparseable sample date '{}'", c.id, w[1].ts);
            }
        }
        // A claim of flattening that its own tail cannot cover is an unanswerable question, and
        // asserting `true` there would bless exactly the gap-as-evidence bug.
        if c.expect.flattening && c.expect.weeks.len() < c.expect.flattening_tail {
            bail!(
                "case {}: expects flattening over {} weeks but the series has {}",
                c.id,
                c.expect.flattening_tail,
                c.expect.weeks.len()
            );
        }
        if let Some(pk) = c.expect.contradictions_peak_week {
            let peak = c
                .expect
                .contradictions
                .iter()
                .max_by_key(|(_, n)| *n)
                .map(|(i, _)| *i);
            if peak != Some(pk) {
                bail!(
                    "case {}: declared peak week {pk} is not the maximum in `contradictions`",
                    c.id
                );
            }
        }
    }
    Ok(())
}

fn eval_health_case(c: &HealthCase) -> std::result::Result<(), String> {
    use crate::memory::stats::{self, Sample};

    let samples: Vec<Sample> = c
        .samples
        .iter()
        .map(|s| Sample {
            ts: s.ts.clone(),
            live: s.live,
            superseded: s.superseded,
            review: s.review,
            turns: s.turns,
            injected_total: s.injected,
            used_total: s.used,
            ..Default::default()
        })
        .collect();

    let weeks = stats::weekly(&samples);
    let got: Vec<usize> = weeks.iter().map(|w| w.index).collect();
    if got != c.expect.weeks {
        return Err(format!(
            "week indices {got:?}, expected {:?}",
            c.expect.weeks
        ));
    }

    // Metric 1.
    let flat = stats::is_flattening(&weeks, c.expect.flattening_tail);
    if flat != c.expect.flattening {
        return Err(format!(
            "flattening={flat}, expected {} (rates {:?})",
            c.expect.flattening,
            stats::saturation(&weeks)
        ));
    }

    // Metric 2, over the whole span: one quiet week swings a weekly ratio hard, and the §8 target
    // is about the trend, not a single bucket.
    let (inj, used) = weeks
        .iter()
        .fold((0u64, 0u64), |(a, b), w| (a + w.d_injected, b + w.d_used));
    if inj > 0 {
        let r = used as f64 / inj as f64;
        if let Some(min) = c.expect.use_ratio_min {
            if r < min {
                return Err(format!("use ratio {r:.3} below the expected floor {min}"));
            }
        }
        if let Some(max) = c.expect.use_ratio_max {
            if r >= max {
                return Err(format!(
                    "use ratio {r:.3} not below the expected ceiling {max}"
                ));
            }
        }
    }

    // Metric 3, from the audit log rather than the samples — the count is per-event.
    let log: String = c
        .audit
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let contra = stats::contradictions_weekly(&log);
    if contra != c.expect.contradictions {
        return Err(format!(
            "contradictions {contra:?}, expected {:?}",
            c.expect.contradictions
        ));
    }
    if let Some(pk) = c.expect.contradictions_peak_week {
        let peak = contra.iter().max_by_key(|(_, n)| *n).map(|(i, _)| *i);
        if peak != Some(pk) {
            return Err(format!("contradiction peak {peak:?}, expected week {pk}"));
        }
        if !(2..=4).contains(&pk) {
            return Err(format!(
                "§8 predicts the peak in weeks 2–4; case declares {pk}"
            ));
        }
    }
    Ok(())
}

/// Entry point for `aizen bench health` — the three §8 measurements against hand-labeled histories.
///
/// A golden set rather than a baseline file, and for the same reason as the other two: these assert
/// that the metric READS a shape correctly, which is a correctness property, not a tunable score.
/// The cases that matter most are the negative ones — a runaway store, a dormant store, and a gap in
/// the series must all fail to look like saturation, because each of them otherwise would.
pub fn run_health() -> Result<()> {
    let cases: Vec<HealthCase> = parse_jsonl(HEALTH_CASES, "health.jsonl")?;
    lint_health(&cases)?;
    println!("health bench: {} golden history(ies)", cases.len());
    let mut failed = 0usize;
    for c in &cases {
        match eval_health_case(c) {
            Ok(()) => println!("  ✓ {}", c.id),
            Err(e) => {
                eprintln!("  ✗ {} — {e}\n      pins: {}", c.id, c.why);
                failed += 1;
            }
        }
    }
    if failed == 0 {
        println!(
            "HEALTH GATE: PASS ({n}/{n} golden histories)",
            n = cases.len()
        );
        Ok(())
    } else {
        eprintln!("HEALTH GATE: FAIL ({failed}/{} cases)", cases.len());
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_fixtures_lint_and_all_pass() {
        let cases: Vec<ProfileCase> = parse_jsonl(PROFILE_CASES, "profile.jsonl").unwrap();
        lint_profile(&cases).expect("profile fixtures must lint clean");
        assert!(!cases.is_empty());
        for c in &cases {
            eval_profile_case(c).unwrap_or_else(|e| panic!("profile case '{}' failed: {e}", c.id));
        }
    }

    #[test]
    fn dialectic_fixtures_lint_and_all_pass() {
        let mems: Vec<DialecticMem> =
            parse_jsonl(DIALECTIC_MEMORIES, "dialectic-memories.jsonl").unwrap();
        let queries: Vec<DialecticQuery> =
            parse_jsonl(DIALECTIC_QUERIES, "dialectic-queries.jsonl").unwrap();
        lint_dialectic(&queries).expect("dialectic fixtures must lint clean");
        let entries: Vec<MemoryEntry> = mems
            .iter()
            .enumerate()
            .map(|(i, m)| entry_from(i, &m.id, &m.body, "user", "manual", m.reinforced))
            .collect();
        let p = profile::build(&entries, TODAY, HALF_LIFE);
        // The whole point: at least one query must exercise the abstain firewall.
        assert!(
            queries.iter().any(|q| q.kind == "abstain"),
            "must test abstain-when-unknown"
        );
        for q in &queries {
            eval_dialectic(&p, &entries, q)
                .unwrap_or_else(|e| panic!("dialectic case '{}' failed: {e}", q.id));
        }
    }

    #[test]
    fn health_fixtures_lint_and_all_pass() {
        let cases: Vec<HealthCase> = parse_jsonl(HEALTH_CASES, "health.jsonl").unwrap();
        lint_health(&cases).expect("health fixtures must lint clean");
        // The negative cases are the load-bearing ones: without them the metric could return `true`
        // unconditionally and still pass a set of healthy histories.
        assert!(
            cases.iter().any(|c| !c.expect.flattening),
            "must include a history that does NOT saturate"
        );
        assert!(
            cases.iter().any(|c| c.expect.use_ratio_max.is_some()),
            "metric 2 must be able to fail while metric 1 passes"
        );
        for c in &cases {
            eval_health_case(c).unwrap_or_else(|e| {
                panic!("health case '{}' failed: {e}\n  pins: {}", c.id, c.why)
            });
        }
    }

    #[test]
    fn health_lint_rejects_a_gap_masquerading_as_a_trend() {
        // The oracle guard that matters: a case whose series is shorter than the trend it claims.
        // Two samples either side of a silence look exactly like a store that settled down, so a
        // fixture asserting flattening over 3 weeks with 2 weeks of data must be rejected outright.
        let bad = vec![HealthCase {
            id: "too-short".into(),
            why: "a claim wider than its own evidence".into(),
            samples: vec![
                HealthSample {
                    ts: "2026-01-05".into(),
                    live: 10,
                    superseded: 0,
                    review: 0,
                    turns: 10,
                    injected: 20,
                    used: 8,
                },
                HealthSample {
                    ts: "2026-02-05".into(),
                    live: 11,
                    superseded: 4,
                    review: 0,
                    turns: 90,
                    injected: 150,
                    used: 60,
                },
            ],
            audit: vec![],
            expect: HealthExpect {
                weeks: vec![1, 5],
                flattening: true,
                flattening_tail: 3,
                use_ratio_min: None,
                use_ratio_max: None,
                contradictions: vec![],
                contradictions_peak_week: None,
            },
        }];
        assert!(lint_health(&bad).is_err());
    }

    #[test]
    fn health_lint_rejects_non_cumulative_totals() {
        // `stats.jsonl` totals only ever grow. A fixture where they shrink describes data the writer
        // cannot produce, so any behaviour it proves is about a file that will never exist.
        let bad = vec![HealthCase {
            id: "counters-go-backwards".into(),
            why: "totals that shrink cannot come from the real writer".into(),
            samples: vec![
                HealthSample {
                    ts: "2026-01-05".into(),
                    live: 10,
                    superseded: 0,
                    review: 0,
                    turns: 50,
                    injected: 90,
                    used: 40,
                },
                HealthSample {
                    ts: "2026-01-12".into(),
                    live: 12,
                    superseded: 1,
                    review: 0,
                    turns: 20,
                    injected: 95,
                    used: 42,
                },
            ],
            audit: vec![],
            expect: HealthExpect {
                weeks: vec![1, 2],
                flattening: false,
                flattening_tail: 2,
                use_ratio_min: None,
                use_ratio_max: None,
                contradictions: vec![],
                contradictions_peak_week: None,
            },
        }];
        assert!(lint_health(&bad).is_err());
    }

    #[test]
    fn lint_rejects_corrupt_profile_oracle() {
        // a case with an invalid dimension must be caught by the lint (anti-oracle guard).
        let bad = vec![ProfileCase {
            id: "bad".into(),
            facts: vec![FixFact {
                name: String::new(),
                body: "x".into(),
                mtype: String::new(),
                source: String::new(),
                reinforced: 0,
            }],
            dim: "nonsense".into(),
            kind: "scalar".into(),
            label: None,
            value: None,
            contains: vec![],
            excludes: vec![],
            min_confidence: None,
        }];
        assert!(lint_profile(&bad).is_err());
    }

    #[test]
    fn lint_rejects_corrupt_dialectic_oracle() {
        let bad = vec![DialecticQuery {
            id: "bad".into(),
            query: "q".into(),
            kind: "abstain".into(),
            dim: None,
            reason: Some("not_a_reason".into()),
        }];
        assert!(lint_dialectic(&bad).is_err());
    }
}
