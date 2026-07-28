//! §8 measurement substrate — the numbers the tier/anchor redesign is judged by.
//!
//! The plan's three health metrics are all *derivatives*: "do live facts flatten while use keeps
//! growing", "does the share of injected facts that actually get used rise", "do contradictions
//! peak in weeks 2–4 then fall". A derivative needs a time series, and nothing in the store keeps
//! one — the entries dir only ever shows today. So one cumulative sample is appended per session to
//! `cli-memory/stats.jsonl`, and everything above that line is a PURE function over the parsed
//! samples ([`weekly`], [`saturation`], [`use_ratio`]) plus one over the audit log
//! ([`contradictions_weekly`]). Pure so the bench can evaluate hand-built histories rather than
//! waiting three weeks to find out the arithmetic was wrong.
//!
//! Cumulative, not per-session, for one reason: a per-session delta is unreadable if a session is
//! lost, and totals let any two lines answer a question about the span between them regardless of
//! what happened in the middle.

use crate::core::config;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};

/// One line of `stats.jsonl`. Population fields are a snapshot ("how many live facts exist now");
/// the `*_total` fields are lifetime counters carried forward from the previous line.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Sample {
    pub ts: String,
    pub live: usize,
    pub archived: usize,
    pub superseded: usize,
    pub review: usize,
    /// Facts placed into a prompt by recall, across all sessions.
    pub injected_total: u64,
    /// Of those, the ones a turn actually drew on. Metric 2 is `used_total / injected_total`.
    pub used_total: u64,
    pub secretary_calls: u64,
    pub turns: u64,
}

// ---------------------------------------------------------------------------------------------
// In-process counters. Per-session; folded into the running totals when the sample is appended.
// ---------------------------------------------------------------------------------------------

static INJECTED: AtomicU64 = AtomicU64::new(0);
static USED: AtomicU64 = AtomicU64::new(0);
static SECRETARY: AtomicU64 = AtomicU64::new(0);
static TURNS: AtomicU64 = AtomicU64::new(0);

/// A gated recall injected `n` facts and the turn went on to use `used` of them. Both halves are
/// recorded in one call because the ratio is only meaningful if the denominator cannot drift from
/// the numerator — two separate call sites would eventually disagree.
pub fn note_recall(injected: u64, used: u64) {
    INJECTED.fetch_add(injected, Ordering::Relaxed);
    USED.fetch_add(used.min(injected), Ordering::Relaxed);
}

pub fn note_secretary_call() {
    SECRETARY.fetch_add(1, Ordering::Relaxed);
}

pub fn note_turn() {
    TURNS.fetch_add(1, Ordering::Relaxed);
}

/// The session's counters so far, as `(injected, used, secretary_calls, turns)`.
pub fn session_counters() -> (u64, u64, u64, u64) {
    (
        INJECTED.load(Ordering::Relaxed),
        USED.load(Ordering::Relaxed),
        SECRETARY.load(Ordering::Relaxed),
        TURNS.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
pub fn reset_counters() {
    INJECTED.store(0, Ordering::Relaxed);
    USED.store(0, Ordering::Relaxed);
    SECRETARY.store(0, Ordering::Relaxed);
    TURNS.store(0, Ordering::Relaxed);
}

// ---------------------------------------------------------------------------------------------
// Persistence
// ---------------------------------------------------------------------------------------------

pub fn stats_path() -> std::path::PathBuf {
    config::cli_memory_dir().join("stats.jsonl")
}

/// Read the file, dropping unparseable lines. A corrupt line is skipped rather than fatal: this is
/// telemetry, and losing one sample is strictly better than a store that refuses to open.
pub fn parse_samples(text: &str) -> Vec<Sample> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter_map(|l| serde_json::from_str::<Sample>(l).ok())
        .collect()
}

pub fn load() -> Vec<Sample> {
    match std::fs::read_to_string(stats_path()) {
        Ok(t) => parse_samples(&t),
        Err(_) => Vec::new(),
    }
}

/// Build the sample to append: populations as given, counters added to the last line's totals.
/// Pure, so the carry-forward arithmetic is testable without a filesystem.
pub fn next_sample(
    previous: Option<&Sample>,
    ts: String,
    live: usize,
    archived: usize,
    superseded: usize,
    review: usize,
    session: (u64, u64, u64, u64),
) -> Sample {
    let (injected, used, secretary, turns) = session;
    let base = previous.cloned().unwrap_or_default();
    Sample {
        ts,
        live,
        archived,
        superseded,
        review,
        injected_total: base.injected_total + injected,
        used_total: base.used_total + used,
        secretary_calls: base.secretary_calls + secretary,
        turns: base.turns + turns,
    }
}

/// Append one cumulative sample. Best-effort — telemetry never fails a session.
pub fn append(live: usize, archived: usize, superseded: usize, review: usize) {
    let counters = session_counters();
    // A session with no turns is not a sample. `aizen --version`, a CLI subcommand, and every test
    // that calls the exit-flush path all reach here; recording them would flood the series with
    // zero-turn lines and flatten metric 1's denominator toward a store that looks saturated because
    // nothing ever asked it anything.
    if counters.3 == 0 {
        return;
    }
    let previous = load();
    let s = next_sample(
        previous.last(),
        crate::memory::learning::audit::ts_now(),
        live,
        archived,
        superseded,
        review,
        counters,
    );
    let Ok(line) = serde_json::to_string(&s) else {
        return;
    };
    let path = stats_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    else {
        return;
    };
    let _ = writeln!(f, "{line}");
}

// ---------------------------------------------------------------------------------------------
// Derivatives (pure)
// ---------------------------------------------------------------------------------------------

/// Date part of a sample timestamp. Accepts both `YYYY-MM-DDTHH:MM:SS` and a bare date, because the
/// format has changed once already and a reader that rejects old lines silently truncates history.
pub fn sample_date(ts: &str) -> Option<chrono::NaiveDate> {
    chrono::NaiveDate::parse_from_str(ts.get(..10).unwrap_or(ts), "%Y-%m-%d").ok()
}

/// One week of history. Weeks are counted from the FIRST sample, not from calendar Mondays: the
/// plan's claims are about elapsed weeks of use ("flattening after week 3"), and a calendar bucket
/// would make week 1 an arbitrary fraction depending on which weekday the user installed on.
#[derive(Debug, Clone, PartialEq)]
pub struct Week {
    /// 1-based; week 1 is the first seven days of use.
    pub index: usize,
    pub live_end: usize,
    pub superseded_end: usize,
    pub review_end: usize,
    pub d_live: i64,
    pub d_turns: u64,
    pub d_injected: u64,
    pub d_used: u64,
}

/// Group samples into weeks, carrying deltas against the previous week's last sample. Weeks with no
/// samples are skipped rather than zero-filled — an absent week means the tool was not used, and
/// inventing a zero-growth week there would fake exactly the flattening metric 1 looks for.
pub fn weekly(samples: &[Sample]) -> Vec<Week> {
    let dated: Vec<(chrono::NaiveDate, &Sample)> = samples
        .iter()
        .filter_map(|s| sample_date(&s.ts).map(|d| (d, s)))
        .collect();
    let Some((first, _)) = dated.first().copied() else {
        return Vec::new();
    };

    // The per-week fold is a free `fn` rather than a closure: it takes two references with
    // INDEPENDENT lifetimes (the sample being flushed, and the `prev` slot it is stored into), and a
    // closure forces those to unify — which the borrow checker rejects. Naming `'s` once says what is
    // actually true: both point into `samples`, which outlives the whole call.
    fn flush<'s>(
        cur: Option<(usize, &'s Sample)>,
        prev: &mut Option<&'s Sample>,
        out: &mut Vec<Week>,
    ) {
        let Some((index, last)) = cur else { return };
        let base = prev.cloned().unwrap_or_default();
        out.push(Week {
            index,
            live_end: last.live,
            superseded_end: last.superseded,
            review_end: last.review,
            d_live: last.live as i64 - base.live as i64,
            d_turns: last.turns.saturating_sub(base.turns),
            d_injected: last.injected_total.saturating_sub(base.injected_total),
            d_used: last.used_total.saturating_sub(base.used_total),
        });
        *prev = Some(last);
    }

    let mut out: Vec<Week> = Vec::new();
    let mut prev: Option<&Sample> = None;
    let mut current: Option<(usize, &Sample)> = None;

    for (date, s) in &dated {
        let idx = ((*date - first).num_days().max(0) / 7) as usize + 1;
        match current {
            Some((cur_idx, _)) if cur_idx == idx => current = Some((idx, s)),
            Some(_) => {
                flush(current, &mut prev, &mut out);
                current = Some((idx, s));
            }
            None => current = Some((idx, s)),
        }
    }
    flush(current, &mut prev, &mut out);
    out
}

/// Metric 1 — new live facts per turn, per week. The claim under test is that this DECLINES after
/// week 3 while `superseded_end + review_end` keeps rising: the store stops growing because facts
/// are being resolved, not because the user stopped working. Weeks with no turns yield `None`
/// rather than 0.0, since "no growth because nothing happened" is not saturation.
pub fn saturation(weeks: &[Week]) -> Vec<Option<f64>> {
    weeks
        .iter()
        .map(|w| {
            if w.d_turns == 0 {
                None
            } else {
                Some(w.d_live as f64 / w.d_turns as f64)
            }
        })
        .collect()
}

/// Metric 2 — of the facts recall injected this week, the share a turn actually used. Target ≥0.35
/// and rising. `None` when nothing was injected: a week where recall never fired says nothing about
/// recall's precision.
pub fn use_ratio(weeks: &[Week]) -> Vec<Option<f64>> {
    weeks
        .iter()
        .map(|w| {
            if w.d_injected == 0 {
                None
            } else {
                Some(w.d_used as f64 / w.d_injected as f64)
            }
        })
        .collect()
}

/// True when metric 1's shape holds: growth-per-turn trending down across the last `tail` weeks
/// while the resolved population (superseded + review) trends up. Reported as one boolean because
/// either half alone is misleading — flat growth with flat supersession is just a dormant store.
///
/// Requires `tail` weeks to actually EXIST. Answering a 3-week question from 2 weeks of data was the
/// original behaviour and it silently turned a gap in the series into evidence: two samples either
/// side of an 18-day silence look exactly like a store that settled down, so a vacation proved the
/// design worked. Too little history is `false` — not proven — and the caller says why.
pub fn is_flattening(weeks: &[Week], tail: usize) -> bool {
    let n = weeks.len();
    if n < tail || tail < 2 {
        return false;
    }
    let span = &weeks[n - tail..];
    let rates: Vec<f64> = saturation(span).into_iter().flatten().collect();
    if rates.len() < 2 {
        return false;
    }
    let growth_down = rates.first() > rates.last();
    let resolved = |w: &Week| w.superseded_end + w.review_end;
    let resolved_up = resolved(span.last().unwrap()) > resolved(span.first().unwrap());
    growth_down && resolved_up
}

/// Metric 3 — contradictions FOUND per week, read from the learning audit log rather than from
/// `stats.jsonl`: the count that matters is per-event, and a weekly snapshot cannot recover it.
/// Counts explicit `supersede` writes plus every `reconcile` line whose verdict was `contradict`,
/// including the ones routed to review — the plan's claim is about how many conflicts the system
/// *notices*, and a conflict a human still has to confirm was noticed all the same.
pub fn contradictions_weekly(audit_jsonl: &str) -> Vec<(usize, usize)> {
    let mut dates: Vec<chrono::NaiveDate> = Vec::new();
    for line in audit_jsonl.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        let op = v.get("op").and_then(|o| o.as_str()).unwrap_or_default();
        let verdict = v.get("verdict").and_then(|o| o.as_str());
        let counts = op == "supersede" || (op == "reconcile" && verdict == Some("contradict"));
        if !counts {
            continue;
        }
        if let Some(d) = v.get("ts").and_then(|t| t.as_str()).and_then(sample_date) {
            dates.push(d);
        }
    }
    dates.sort();
    let Some(first) = dates.first().copied() else {
        return Vec::new();
    };
    let mut out: Vec<(usize, usize)> = Vec::new();
    for d in dates {
        let idx = ((d - first).num_days() / 7) as usize + 1;
        match out.last_mut() {
            Some((i, n)) if *i == idx => *n += 1,
            _ => out.push((idx, 1)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(ts: &str, live: usize, superseded: usize, turns: u64, inj: u64, used: u64) -> Sample {
        Sample {
            ts: ts.into(),
            live,
            superseded,
            turns,
            injected_total: inj,
            used_total: used,
            ..Default::default()
        }
    }

    #[test]
    fn totals_carry_forward_from_the_previous_line() {
        // The counters are per-session but the file is cumulative, so a fresh session's 4 turns must
        // land as 14, not overwrite the 10 already there. Getting this backwards would make every
        // weekly delta negative and silently invert metric 1.
        let prev = sample("2026-07-01T10:00:00", 20, 3, 10, 100, 40);
        let s = next_sample(
            Some(&prev),
            "2026-07-02T09:00:00".into(),
            22,
            1,
            4,
            2,
            (30, 12, 5, 4),
        );
        assert_eq!(s.turns, 14);
        assert_eq!(s.injected_total, 130);
        assert_eq!(s.used_total, 52);
        assert_eq!(s.live, 22, "populations are a snapshot, not a sum");
        assert_eq!(
            (s.archived, s.superseded, s.review),
            (1, 4, 2),
            "in signature order"
        );
    }

    #[test]
    fn a_first_sample_starts_from_zero() {
        let s = next_sample(None, "2026-07-01T10:00:00".into(), 5, 0, 0, 1, (9, 3, 2, 1));
        assert_eq!((s.injected_total, s.used_total, s.turns), (9, 3, 1));
    }

    #[test]
    fn used_can_never_exceed_injected() {
        // The ratio is a share; a caller that miscounts and reports 5 used out of 3 injected would
        // produce >1.0 and make metric 2 unreadable. Clamp at the recording site.
        reset_counters();
        note_recall(3, 5);
        let (inj, used, _, _) = session_counters();
        assert_eq!((inj, used), (3, 3));
        reset_counters();
    }

    #[test]
    fn weeks_are_counted_from_first_use_and_gaps_are_not_invented() {
        // Samples on day 0, day 3 (same week), then day 21 (week 4). Week 4 must be reported as
        // week 4 — not week 2 — and weeks 2/3 must be ABSENT rather than zero-filled, since a
        // fabricated flat week is indistinguishable from real saturation.
        let s = vec![
            sample("2026-06-01T10:00:00", 10, 0, 5, 20, 5),
            sample("2026-06-04T10:00:00", 18, 1, 15, 60, 20),
            sample("2026-06-22T10:00:00", 20, 9, 40, 160, 70),
        ];
        let w = weekly(&s);
        assert_eq!(w.iter().map(|x| x.index).collect::<Vec<_>>(), vec![1, 4]);
        assert_eq!(
            w[0].d_live, 18,
            "week 1 ends at the LAST sample in the week"
        );
        assert_eq!(w[0].d_turns, 15);
        assert_eq!(
            w[1].d_live, 2,
            "growth between week 1's end and week 4's end"
        );
        assert_eq!(w[1].d_turns, 25);
        assert_eq!(w[1].d_injected, 100);
        assert_eq!(w[1].d_used, 50);
    }

    #[test]
    fn flattening_requires_both_halves() {
        // Growth per turn falls 1.0 → 0.1 while superseded climbs 0 → 30: the shape §8 predicts.
        let healthy = vec![
            Week {
                index: 1,
                live_end: 20,
                superseded_end: 0,
                review_end: 0,
                d_live: 20,
                d_turns: 20,
                d_injected: 40,
                d_used: 8,
            },
            Week {
                index: 2,
                live_end: 24,
                superseded_end: 30,
                review_end: 0,
                d_live: 4,
                d_turns: 40,
                d_injected: 90,
                d_used: 40,
            },
        ];
        assert!(is_flattening(&healthy, 2));

        // Same flat growth, but nothing is being resolved — a dormant store, not a saturating one.
        let dormant = vec![
            Week {
                index: 1,
                live_end: 20,
                superseded_end: 0,
                review_end: 0,
                d_live: 20,
                d_turns: 20,
                d_injected: 40,
                d_used: 8,
            },
            Week {
                index: 2,
                live_end: 24,
                superseded_end: 0,
                review_end: 0,
                d_live: 4,
                d_turns: 40,
                d_injected: 90,
                d_used: 40,
            },
        ];
        assert!(
            !is_flattening(&dormant, 2),
            "flat growth alone is not saturation"
        );
    }

    #[test]
    fn empty_weeks_yield_none_not_zero() {
        let w = vec![Week {
            index: 1,
            live_end: 3,
            superseded_end: 0,
            review_end: 0,
            d_live: 3,
            d_turns: 0,
            d_injected: 0,
            d_used: 0,
        }];
        assert_eq!(
            saturation(&w),
            vec![None],
            "no turns means no rate, not a rate of zero"
        );
        assert_eq!(use_ratio(&w), vec![None]);
    }

    #[test]
    fn contradictions_count_findings_including_declined_ones() {
        // A `contradict` verdict routed to review was still a conflict the system NOTICED, so it
        // counts. `same` and `refine` do not, and neither does an unrelated op.
        let log = [
            r#"{"ts":"2026-06-01T10:00:00","op":"reconcile","verdict":"contradict","confidence":0.9}"#,
            r#"{"ts":"2026-06-02T10:00:00","op":"reconcile","verdict":"contradict","confidence":0.4}"#,
            r#"{"ts":"2026-06-02T11:00:00","op":"reconcile","verdict":"same","confidence":0.9}"#,
            r#"{"ts":"2026-06-03T10:00:00","op":"learn"}"#,
            r#"{"ts":"2026-06-15T10:00:00","op":"supersede"}"#,
            "not json at all",
        ]
        .join("\n");
        assert_eq!(contradictions_weekly(&log), vec![(1, 2), (3, 1)]);
    }

    #[test]
    fn a_corrupt_line_costs_one_sample_not_the_file() {
        let text = format!(
            "{}\nhalf a line{{\n{}\n",
            serde_json::to_string(&sample("2026-06-01T10:00:00", 1, 0, 1, 1, 0)).unwrap(),
            serde_json::to_string(&sample("2026-06-02T10:00:00", 2, 0, 2, 2, 1)).unwrap()
        );
        assert_eq!(parse_samples(&text).len(), 2);
    }
}
