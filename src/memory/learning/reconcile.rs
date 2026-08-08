//! Local reconciliation (M2a): given a fact the secretary just produced, decide — **for free** —
//! whether the store already knows it, already nearly knows it, or has never heard it.
//!
//! "For free" is a design constraint, not a nicety: this runs on the write path of every learning
//! turn, over a pool already in RAM (`learning::ingest` loads the store once), using the MinHash and
//! lexical scorers the dedup path already ships. No model call, no extra disk read.
//!
//! ## Why the decision happens HERE and not in the secretary's prompt
//!
//! The natural design is to hand the secretary a shortlist of possibly-conflicting facts and let it
//! judge. That shortlist cannot be built: selecting candidates means comparing them to the fact's
//! text, and the fact does not exist until the secretary has already answered. Selecting on the raw
//! transcript instead recalls badly, so genuine conflicts land in `new` — which is the exact failure
//! reconciliation exists to prevent. So the secretary only GENERATES; classification is arithmetic
//! run afterwards, when there is a text to compare against.
//!
//! ## The bands
//!
//! | similarity | verdict | rows added |
//! |---|---|---|
//! | ≥ 0.80 | [`Verdict::Same`] — confirm the existing fact | 0 |
//! | 0.55 – 0.80 | [`Verdict::NeedsJudgement`] — review queue, BOTH texts | 0 (queue is a live view) |
//! | < 0.55 | [`Verdict::New`] — write it | 1 |
//!
//! The similarity feeding those bands is the max of three measures — see [`best_match`]. The bands
//! themselves are unchanged; what changed (2026-08-06) is that a Vietnamese restatement now REACHES
//! them. Measured beforehand on 360 live facts, zero pairs cleared either threshold while the store
//! plainly held duplicates, so the bands were tuned against a signal that could not fire.
//!
//! A contradiction phrased in different words ("we use npm" vs "switched to pnpm") scores LOW and so
//! lands in `New`. That is correct here and is why phase 4 exists: resolving it needs enough context
//! to judge intent, which belongs in a batched off-turn pass, not on this hot path.
//!
//! ## The batch pass removes duplicates; before 2026-08-06 it only counted them
//!
//! M2a (above) stops a duplicate from being WRITTEN. Nothing removed one already in the store, and
//! that gap was invisible because the pass reported success either way: measured on the live store,
//! **246 reconcile passes had produced 227 `confirm` actions and retired exactly 0 rows**, while six
//! copies of one fact sat in the store having survived every pass. [`Action::Confirm`] named only the
//! surviving target, so `apply_action` credited it and returned — the redundant half was never
//! addressed even though [`BatchVerdict::Same`]'s own doc called it redundant.
//!
//! `Confirm` now carries the duplicate too, and removing it answers to its own gates
//! ([`decide_action`]). Two properties make that safe to do automatically:
//!
//! - **Nothing is destroyed.** A live duplicate is retired with `supersededBy` (revivable, still in
//!   `memory as-of`); a queued one moves to `review/.discarded/`. See [`drop_redundant`].
//! - **A cluster never empties.** Similarity is symmetric, so {a,b,c} yields pairs a→b, b→c, c→a and
//!   three individually-correct decisions would delete all three. [`guard_against_cycles`] downgrades
//!   any pair whose target an earlier pair already retired, leaving exactly one survivor.

use crate::memory::bloat::dedup;
use crate::memory::learning::match_text;
use crate::memory::path_scope::{self, Tier};
use crate::memory::score::lexical_score_tokens;
use crate::memory::store::MemoryEntry;
use crate::memory::tokenize::tokenize;

/// At or above this, the store already says this — confirm, do not duplicate.
pub const SAME_MIN: f64 = 0.80;
/// At or above this (but below [`SAME_MIN`]), it is too close to call locally.
pub const JUDGEMENT_MIN: f64 = 0.55;

/// Lowest model confidence that may DESTRUCTIVELY act (refine a body, retire a fact). Below it the
/// pair stays in review, untouched — the asymmetry is deliberate: leaving a duplicate costs a row,
/// acting wrongly costs a true fact.
pub const APPLY_MIN: f64 = 0.65;

/// Most pairs one batch pass will judge. The cap is the budget: all pairs go in ONE model call, so
/// this bounds both the call's size and the damage a single confused response can do.
pub const MAX_PAIRS: usize = 12;

/// Pending pairs that trigger a pass on their own.
pub const TRIGGER_PENDING: usize = 8;
/// Days since the last pass that trigger one regardless of how few pairs are waiting.
pub const TRIGGER_DAYS: i64 = 7;

/// A target with at least this many confirmations ALWAYS goes to review, whatever the model's
/// confidence. The barrier scales with what is being broken: a fact the user has leaned on twice is
/// not something an automatic pass gets to overwrite on its own say-so.
pub const PROTECTED_CONFIRMATIONS: u32 = 2;

/// What to do with one freshly-produced fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The store already holds this fact (`id`). Confirm it; write no new row.
    Same { id: String },
    /// Close to `id`, but not close enough to merge automatically. Queue both texts for a human.
    NeedsJudgement { id: String },
    /// Nothing comparable — persist it.
    New,
}

/// The best match for `text` within `pool`, as `(id, similarity)`.
///
/// Similarity is the MAX of three cheap measures rather than any alone, because they fail on
/// different inputs: MinHash-over-shingles catches rewording and word order but is blind to a short
/// fact (too few shingles to sample); token overlap handles short facts but is fooled by two
/// sentences that share vocabulary and mean different things; and BOTH are defeated by a restatement
/// that swaps the words themselves, which is the normal case in Vietnamese
/// (`người dùng`/`user`/`anh`, `giao tiếp`/`trao đổi`). Taking the max means a hit from any is enough
/// to stop a duplicate, which is the asymmetry we want — a missed duplicate is a second row saying
/// the same thing, a false duplicate is a lost fact.
///
/// The third measure ([`match_text::match_similarity`]) was added after measuring the live store:
/// over 360 real facts, the first two produced ZERO pairs above either band while the store visibly
/// held five copies of one fact. See that module's header for the numbers.
pub fn best_match(text: &str, pool: &[MemoryEntry]) -> Option<(String, f64)> {
    if pool.is_empty() {
        return None;
    }
    let sig = dedup::signature(text);
    let toks = tokenize(text);
    let mut best: Option<(String, f64)> = None;
    for e in pool {
        let minhash = dedup::similarity(&sig, &dedup::signature(&e.body));
        let lexical = lexical_score_tokens(&toks, &e.tokens);
        let normalized = match_text::match_similarity(text, &e.body);
        let s = minhash.max(lexical).max(normalized);
        if best.as_ref().is_none_or(|(_, bs)| s > *bs) {
            best = Some((e.id.clone(), s));
        }
    }
    best
}

/// Classify `text` against `pool`. Pure — no I/O, no model call.
pub fn classify_local(text: &str, pool: &[MemoryEntry]) -> Verdict {
    match best_match(text, pool) {
        Some((id, s)) if s >= SAME_MIN => Verdict::Same { id },
        Some((id, s)) if s >= JUDGEMENT_MIN => Verdict::NeedsJudgement { id },
        _ => Verdict::New,
    }
}

// ── M2b: the batched, off-hot-path pass ──────────────────────────────────

/// One suspicious pair for the batch pass to judge: a candidate (a review-queue item, or a fact
/// added since the last pass) against the live fact it resembles.
#[derive(Debug, Clone)]
pub struct Pair {
    /// Id of the candidate. For a review-queue item this is its file id in `review/`.
    pub candidate_id: String,
    pub candidate_text: String,
    /// The LIVE fact the candidate resembles — the one at risk of being overwritten or retired.
    pub target_id: String,
    pub target_text: String,
    /// Local similarity that made this pair suspicious (for the prompt and the audit trail).
    pub similarity: f64,
    /// The target's confirmation count, which decides whether it is protected.
    pub target_confirmations: u32,
    /// Is the candidate a LIVE fact (as opposed to a review-queue item)? The two are retired by
    /// different mechanisms — a live row gets `supersededBy`, a queued row moves to `.discarded/` —
    /// and only the live case can leave the store one row shorter.
    pub candidate_is_live: bool,
    /// The candidate's own confirmation count. The redundant side of a `same` pair is *deleted*, so
    /// it needs the same protection [`PROTECTED_CONFIRMATIONS`] gives the target: a duplicate the
    /// user has leaned on twice is not something an automatic pass gets to remove unasked.
    pub candidate_confirmations: u32,
}

/// What the model said about one pair.
#[derive(Debug, Clone, PartialEq)]
pub enum BatchVerdict {
    /// Two statements of the same fact. Confirm the target; the candidate is redundant.
    Same,
    /// The candidate says the target better. Rewrite the target's body IN PLACE, keeping its id.
    Refine { text: String },
    /// The candidate contradicts the target: the world changed. Write it as a new fact carrying
    /// `supersedes: <target>`.
    Contradict { text: String },
    /// Cannot tell. Leave everything alone.
    Unsure,
}

impl BatchVerdict {
    pub fn label(&self) -> &'static str {
        match self {
            BatchVerdict::Same => "same",
            BatchVerdict::Refine { .. } => "refine",
            BatchVerdict::Contradict { .. } => "contradict",
            BatchVerdict::Unsure => "unsure",
        }
    }

    /// Would acting on this verdict overwrite or retire the target?
    fn is_destructive(&self) -> bool {
        matches!(
            self,
            BatchVerdict::Refine { .. } | BatchVerdict::Contradict { .. }
        )
    }
}

/// One judged pair: which pair, what verdict, how sure.
#[derive(Debug, Clone, PartialEq)]
pub struct Judgement {
    /// Index into the `Pair` slice the prompt was built from.
    pub pair: usize,
    pub verdict: BatchVerdict,
    pub confidence: f64,
}

/// What the safety rails decided to actually DO with a judged pair.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Credit the target and drop the redundant candidate.
    ///
    /// `redundant` is the candidate to remove, or `None` when the duplicate has to stay: an
    /// unprotected duplicate is deleted, a protected or already-claimed one is only counted.
    /// Splitting it this way keeps the CONFIRM half unconditional (crediting a fact is always safe)
    /// while the DELETE half answers to its own gate.
    Confirm {
        target: String,
        redundant: Option<Redundant>,
    },
    /// Rewrite the target's body in place, keeping its id.
    Refine { target: String, body: String },
    /// Write a new fact that retires the target in the same write.
    Supersede { target: String, body: String },
    /// Touch nothing; the pair stays in review. `why` is shown in the dry-run listing.
    Review { target: String, why: &'static str },
}

/// The duplicate half of a `same` pair, and how to get rid of it.
#[derive(Debug, Clone, PartialEq)]
pub struct Redundant {
    pub id: String,
    /// A live row is retired with `supersededBy` (reversible via `memory revive`); a review-queue
    /// row is moved to `.discarded/` (the queue is that text's only copy, so it is never unlinked).
    pub live: bool,
}

impl Action {
    pub fn label(&self) -> &'static str {
        match self {
            Action::Confirm { .. } => "confirm",
            Action::Refine { .. } => "refine",
            Action::Supersede { .. } => "supersede",
            Action::Review { .. } => "review",
        }
    }
}

/// The safety rails, as one pure function — the whole point of M2b's caution lives here, so it is
/// testable without a model, a store, or a filesystem.
///
/// Two gates stand between a verdict and a destructive write:
///
/// 1. **Confidence.** Below [`APPLY_MIN`] nothing destructive happens. A 0.5 guess and a 0.9 call are
///    not the same evidence, and the cost of being wrong is asymmetric: an unresolved duplicate is
///    one redundant row, a wrong supersede is a true fact the user can no longer see.
/// 2. **The target's confirmations.** At [`PROTECTED_CONFIRMATIONS`] or above, the pair goes to
///    review no matter how sure the model is. Confirmations are the store's only record that a fact
///    actually helped someone; a batch pass overruling that on its own authority inverts who is in
///    charge. The barrier scales with what is being broken.
///
/// `Same` is exempt from BOTH, but only for the half that adds evidence. Crediting the target
/// destroys nothing, so it happens at any confidence. **Removing the duplicate is a delete**, and
/// answers to the same two gates the destructive verdicts do — which is the whole reason `Confirm`
/// carries `redundant: Option<_>` instead of a bare id. Measured 2026-08-06 on the live store: 246
/// reconcile passes had produced 227 `confirm` actions and removed exactly zero rows, because this
/// branch used to return only the target. Six copies of one fact survived every one of those passes.
///
/// The delete is gated on the CANDIDATE's confirmations, not the target's: the candidate is what
/// disappears. A duplicate the user has confirmed twice is evidence in its own right — probably that
/// the two texts are not actually the same fact — so it stays and a human decides.
pub fn decide_action(pair: &Pair, j: &Judgement) -> Action {
    match &j.verdict {
        BatchVerdict::Same => Action::Confirm {
            target: pair.target_id.clone(),
            // A duplicate is removed only when the pass is sure AND the duplicate carries no
            // standing of its own. Otherwise the target is still credited and the row survives.
            redundant: (j.confidence >= APPLY_MIN
                && pair.candidate_confirmations < PROTECTED_CONFIRMATIONS)
                .then(|| Redundant {
                    id: pair.candidate_id.clone(),
                    live: pair.candidate_is_live,
                }),
        },
        BatchVerdict::Unsure => Action::Review {
            target: pair.target_id.clone(),
            why: "model was unsure",
        },
        v if j.confidence < APPLY_MIN => Action::Review {
            target: pair.target_id.clone(),
            why: if v.is_destructive() {
                "confidence below the apply bar"
            } else {
                "low confidence"
            },
        },
        _ if pair.target_confirmations >= PROTECTED_CONFIRMATIONS => Action::Review {
            target: pair.target_id.clone(),
            why: "the target has been confirmed — a human decides",
        },
        BatchVerdict::Refine { text } => Action::Refine {
            target: pair.target_id.clone(),
            body: text.clone(),
        },
        BatchVerdict::Contradict { text } => Action::Supersede {
            target: pair.target_id.clone(),
            body: text.clone(),
        },
    }
}

/// Second-pass rail: re-decide one action in light of what earlier pairs in the SAME batch already
/// removed. Pure, so the "a cluster never empties" invariant is testable without a filesystem.
///
/// [`decide_action`] judges each pair in isolation, which is right — it has no business knowing the
/// batch. But duplicate detection is symmetric, so a cluster of three near-identical facts produces
/// pairs A→B, B→C, C→A, and three isolated-but-correct decisions delete all three. This function is
/// where the batch gets a say:
///
/// - **The candidate is already gone.** Nothing to do; the pair is history.
/// - **The target is already gone.** The candidate was judged redundant *relative to that target*.
///   With the target retired, keeping the candidate is no longer redundancy — it may now be the
///   cluster's only survivor. Downgrade to review and let a human look.
///
/// Non-`Confirm` actions pass through: [`Action::Supersede`] writes a replacement in the same
/// operation, so it cannot empty anything, and `Refine`/`Review` remove nothing.
fn guard_against_cycles(action: Action, removed: &std::collections::HashSet<String>) -> Action {
    let Action::Confirm { target, redundant } = action else {
        return action;
    };
    if removed.contains(&target) {
        return Action::Review {
            target,
            why: "its target was just retired — a human decides which survives",
        };
    }
    let redundant = redundant.filter(|r| !removed.contains(&r.id));
    Action::Confirm { target, redundant }
}

// ── The same-chain tie-break ─────────────────────────────────────────────
//
// Complete and unit-tested, but not yet called from the reconcile pass: nothing feeds it candidate
// pairs. Kept whole because the rule it encodes (freshness first, then specificity, re-anchor to the
// common ancestor) is the decision, not the plumbing around it.

/// Which of two place facts on the same inheritance chain survives, and where it re-anchors.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub struct ChainWinner {
    pub winner_id: String,
    pub loser_id: String,
    /// Where the survivor must be re-anchored: the deepest folder containing BOTH claims. `None`
    /// when they share nothing worth anchoring to, in which case the winner keeps its own anchor.
    pub reanchor: Option<String>,
}

/// The freshness key for the chain rule: the later of `lastUsed` and `updated`. Dates are
/// `YYYY-MM-DD`, which sorts chronologically as a plain string.
#[allow(dead_code)]
fn freshness(e: &MemoryEntry) -> String {
    let a = e.last_used.as_deref().unwrap_or("");
    let b = e.updated.as_deref().unwrap_or("");
    let c = e.created.as_deref().unwrap_or("");
    [a, b, c].into_iter().max().unwrap_or("").to_string()
}

/// Resolve two near-duplicate place facts whose anchors sit on the same inheritance chain (one
/// anchor is an ancestor of the other, so both claims are live in the deeper directory at once).
///
/// **Newer wins; specificity only breaks a tie.** The plan's "nearest ancestor wins" is a rule about
/// LOCATION — which fact applies where — and using it as the arbiter for a CONTRADICTION gets the
/// answer backwards: `tokio 1.35` anchored at `…/aizen/src` would beat `tokio 1.40` anchored at
/// `…/aizen` purely for being deeper, i.e. the stale fact wins for having been written about a
/// smaller area. Recency is the signal that tracks which claim is true now.
///
/// The survivor is re-anchored to the **common ancestor**, never left at its own anchor: the two
/// facts disagreed about CONTENT, and silently narrowing the winner's scope as a side effect would
/// change where it applies without saying so. Returns `None` when the pair is not on one chain (or
/// is not a place pair at all), which is the caller's signal to leave the anchors alone.
#[allow(dead_code)]
pub fn resolve_chain(a: &MemoryEntry, b: &MemoryEntry) -> Option<ChainWinner> {
    if a.tier != Tier::Place || b.tier != Tier::Place {
        return None;
    }
    let (aa, ba) = (a.anchor.as_deref()?, b.anchor.as_deref()?);
    if !path_scope::is_ancestor(aa, ba) && !path_scope::is_ancestor(ba, aa) {
        return None; // different subtrees — not one chain, so there is nothing to arbitrate
    }
    let (fa, fb) = (freshness(a), freshness(b));
    let (winner, loser) = match fa.cmp(&fb) {
        std::cmp::Ordering::Greater => (a, b),
        std::cmp::Ordering::Less => (b, a),
        // Same day: NOW specificity decides, because there is no recency signal left to read.
        std::cmp::Ordering::Equal => {
            if path_scope::depth(aa) >= path_scope::depth(ba) {
                (a, b)
            } else {
                (b, a)
            }
        }
    };
    Some(ChainWinner {
        winner_id: winner.id.clone(),
        loser_id: loser.id.clone(),
        reanchor: path_scope::common_ancestor(aa, ba),
    })
}

// ── Trigger + pair collection ────────────────────────────────────────────

/// Days between two `YYYY-MM-DD` dates, or `None` if either fails to parse.
fn days_between(from: &str, to: &str) -> Option<i64> {
    let f = chrono::NaiveDate::parse_from_str(from.trim(), "%Y-%m-%d").ok()?;
    let t = chrono::NaiveDate::parse_from_str(to.trim(), "%Y-%m-%d").ok()?;
    Some((t - f).num_days())
}

/// Should a pass run automatically? `≥ TRIGGER_PENDING` pairs waiting, OR `≥ TRIGGER_DAYS` since the
/// last one (so a store with a slow trickle of pairs still converges), and never with zero pairs.
///
/// `last_run` is `None` on a store that has never run one, which counts as due.
pub fn should_run(pending: usize, last_run: Option<&str>, today: &str) -> bool {
    if pending == 0 {
        return false;
    }
    if pending >= TRIGGER_PENDING {
        return true;
    }
    match last_run {
        None => true,
        Some(d) => days_between(d, today).is_none_or(|n| n >= TRIGGER_DAYS),
    }
}

/// Marker holding the date of the last batch pass.
fn last_run_path() -> std::path::PathBuf {
    crate::core::config::cli_memory_dir().join(".reconcile-last")
}

/// The date of the last pass, if any.
pub fn last_run() -> Option<String> {
    std::fs::read_to_string(last_run_path())
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn stamp_last_run(today: &str) {
    let p = last_run_path();
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(&p, today.as_bytes());
}

/// Build the pair list: every candidate matched against its closest live fact, keeping only pairs in
/// the suspicious band, **highest similarity first**, capped at [`MAX_PAIRS`].
///
/// Sorting by similarity before truncating matters: the cap is a budget, and spending it on the
/// twelve most-likely-duplicate pairs converges the store faster than spending it on whichever
/// twelve the directory listing happened to yield first.
pub fn collect_pairs(candidates: &[MemoryEntry], live: &[MemoryEntry]) -> Vec<Pair> {
    let mut pairs: Vec<Pair> = Vec::new();
    for c in candidates {
        // Never pair a candidate with itself (a review item promoted earlier, or the live copy of a
        // fact that is also in the candidate list).
        let pool: Vec<MemoryEntry> = live.iter().filter(|e| e.id != c.id).cloned().collect();
        let Some((target_id, similarity)) = best_match(&c.body, &pool) else {
            continue;
        };
        if similarity < JUDGEMENT_MIN {
            continue; // nothing close enough to be a conflict
        }
        let Some(t) = pool.iter().find(|e| e.id == target_id) else {
            continue;
        };
        pairs.push(Pair {
            candidate_id: c.id.clone(),
            candidate_text: c.body.clone(),
            target_id: t.id.clone(),
            target_text: t.body.clone(),
            similarity,
            target_confirmations: t.confirmations,
            // Identity by id, not by path: `live` is the caller's active view, and a candidate that
            // appears in it IS that row rather than a queued copy of it.
            candidate_is_live: live.iter().any(|e| e.id == c.id),
            candidate_confirmations: c.confirmations,
        });
    }
    pairs.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    pairs.truncate(MAX_PAIRS);
    pairs
}

// ── Prompt + parse ───────────────────────────────────────────────────────

pub fn system_prompt() -> &'static str {
    "You are reconciling a memory store. Each numbered item shows a STORED fact and a NEW candidate \
that resembles it. For each, decide:\n\
- \"same\": both state the same thing. No change needed.\n\
- \"refine\": the same claim, but the candidate states it more precisely or completely. Supply the \
merged wording in \"text\".\n\
- \"contradict\": they cannot both be true — something changed. Supply the wording that is true NOW \
in \"text\".\n\
- \"unsure\": you cannot tell from the two texts alone.\n\
\n\
Rules: judge ONLY the two texts given; never invent detail that is in neither. Prefer \"unsure\" over \
a guess — a wrong \"contradict\" erases a true fact. \"confidence\" is your own 0..1 certainty.\n\
\n\
Reply with JSON only:\n\
{\"pairs\":[{\"n\":1,\"verdict\":\"same|refine|contradict|unsure\",\"confidence\":0.0,\"text\":\"\"}]}"
}

/// Render the pairs as the one user message of the single model call.
pub fn build_prompt(pairs: &[Pair]) -> String {
    let mut s = String::new();
    for (i, p) in pairs.iter().enumerate() {
        s.push_str(&format!(
            "{}. STORED: {}\n   NEW: {}\n",
            i + 1,
            p.target_text.replace('\n', " "),
            p.candidate_text.replace('\n', " ")
        ));
    }
    s
}

/// Parse the model's reply. Unparseable output yields an EMPTY list, not a guess: a confused
/// response costs one wasted call and changes nothing, which is the correct failure mode for a pass
/// whose actions are destructive.
///
/// Out-of-range indices and unknown verdicts are dropped individually, so one malformed item does
/// not discard the rest of a good response. A `refine`/`contradict` with no `text` becomes `Unsure`:
/// the model asked for a rewrite and then supplied nothing to write.
pub fn parse_judgements(raw: &str, n_pairs: usize) -> Vec<Judgement> {
    let Some(json) = crate::extract_json_object(raw) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let Some(items) = v.get("pairs").and_then(|p| p.as_array()) else {
        return Vec::new();
    };
    let mut out: Vec<Judgement> = Vec::new();
    for it in items {
        let Some(n) = it.get("n").and_then(|n| n.as_u64()) else {
            continue;
        };
        let idx = match usize::try_from(n) {
            Ok(k) if k >= 1 && k <= n_pairs => k - 1,
            _ => continue,
        };
        if out.iter().any(|j| j.pair == idx) {
            continue; // one verdict per pair; a repeat is noise
        }
        let confidence = it
            .get("confidence")
            .and_then(|c| c.as_f64())
            .unwrap_or(0.0)
            .clamp(0.0, 1.0);
        let text = it
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let verdict = match it
            .get("verdict")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .trim()
            .to_lowercase()
            .as_str()
        {
            "same" => BatchVerdict::Same,
            "refine" if !text.is_empty() => BatchVerdict::Refine { text },
            "contradict" if !text.is_empty() => BatchVerdict::Contradict { text },
            "refine" | "contradict" | "unsure" => BatchVerdict::Unsure,
            _ => continue,
        };
        out.push(Judgement {
            pair: idx,
            verdict,
            confidence,
        });
    }
    out
}

// ── Apply ────────────────────────────────────────────────────────────────

/// One line of what a pass did (or would do), for the CLI listing.
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedPair {
    pub candidate_id: String,
    pub target_id: String,
    pub verdict: &'static str,
    pub confidence: f64,
    pub action: Action,
    /// Empty on a dry run; otherwise what the write produced (new id, revision id, …).
    pub note: String,
}

/// Outcome of one batch pass.
#[derive(Debug, Clone, Default)]
pub struct BatchReport {
    pub pairs_judged: usize,
    pub model_calls: usize,
    pub applied: Vec<AppliedPair>,
    pub dry_run: bool,
}

/// Perform one batch pass over `pairs`.
///
/// `judge` is the single model call, injected rather than called directly: it makes every rail above
/// testable with a canned response, and it makes the "≤1 call" budget structural — `batch_pass` has
/// exactly one place it can invoke it, and no loop around it.
///
/// `dry_run` is the CLI default. It runs the model call (so the listing shows real verdicts) but
/// performs no writes.
pub fn batch_pass<F>(
    pairs: &[Pair],
    judge: F,
    dry_run: bool,
    session_id: &str,
    live: &[MemoryEntry],
) -> BatchReport
where
    F: FnOnce(&str, &str) -> Option<String>,
{
    let mut report = BatchReport {
        dry_run,
        ..Default::default()
    };
    if pairs.is_empty() {
        return report;
    }
    let capped = &pairs[..pairs.len().min(MAX_PAIRS)];
    report.pairs_judged = capped.len();

    let raw = match judge(system_prompt(), &build_prompt(capped)) {
        Some(r) => {
            report.model_calls = 1;
            r
        }
        None => return report, // the call failed: nothing judged, nothing written
    };
    let today = crate::memory::bloat::decay::today();
    // Ids this pass has already removed. Duplicate detection is SYMMETRIC — if A is a restatement
    // of B, then B is a restatement of A — so one cluster of near-identical facts yields pairs
    // pointing both ways, and acting on each independently would delete every member and leave the
    // store with none. Two rules keep at least one survivor per cluster:
    //
    //   1. A row already removed is not removed twice.
    //   2. A pair whose TARGET is gone cannot act. The target is the survivor the candidate was
    //      judged redundant *against*; once it is retired, "keep the target, drop the candidate" no
    //      longer describes anything true, so the pair goes to review instead of guessing.
    //
    // Rule 2 is what breaks the A→B / B→A cycle: the second pair sees its target already retired.
    let mut removed: std::collections::HashSet<String> = std::collections::HashSet::new();
    for j in parse_judgements(&raw, capped.len()) {
        let pair = &capped[j.pair];
        let mut action = decide_action(pair, &j);
        if !dry_run {
            action = guard_against_cycles(action, &removed);
        }
        let mut note = String::new();
        if !dry_run {
            note = apply_action(pair, &action, live, session_id, &today);
            // Only a removal that actually happened counts — a failed rename must not make the next
            // pair believe the row is gone.
            if let Action::Confirm {
                redundant: Some(r), ..
            } = &action
            {
                if !note.contains("failed") {
                    removed.insert(r.id.clone());
                }
            }
        }
        crate::memory::learning::audit::reconcile(
            session_id,
            j.verdict.label(),
            j.confidence,
            &pair.target_id,
            action.label(),
        );
        report.applied.push(AppliedPair {
            candidate_id: pair.candidate_id.clone(),
            target_id: pair.target_id.clone(),
            verdict: j.verdict.label(),
            confidence: j.confidence,
            action,
            note,
        });
    }
    if !dry_run {
        stamp_last_run(&today);
    }
    report
}

/// Execute one decided action. Best-effort per pair: a failed write is reported in the note and the
/// pass moves on, rather than aborting and leaving the remaining pairs unjudged for another week.
fn apply_action(
    pair: &Pair,
    action: &Action,
    live: &[MemoryEntry],
    session_id: &str,
    today: &str,
) -> String {
    use crate::memory::store::{self, LearnedWrite};

    let target = live.iter().find(|e| e.id == pair.target_id);
    let Some(target) = target else {
        return "target vanished".to_string();
    };
    match action {
        Action::Review { .. } => String::new(),
        Action::Confirm { redundant, .. } => {
            let credited = match store::confirm_use(target, today) {
                Ok(true) => "confirmed".to_string(),
                Ok(false) => "already confirmed today".to_string(),
                Err(e) => return format!("failed: {e}"),
            };
            // Credit first, remove second. If the removal fails the store is left with a duplicate
            // and an accurate confirmation count — the harmless direction. The reverse order could
            // drop the duplicate and then fail to credit the survivor, losing the only record that
            // two sessions agreed on the fact.
            let Some(r) = redundant else {
                return credited;
            };
            match drop_redundant(r, session_id, &target.id) {
                Ok(where_to) => format!("{credited}; dropped duplicate {} → {where_to}", r.id),
                Err(e) => format!("{credited}; duplicate {} kept — {e}", r.id),
            }
        }
        Action::Refine { body, .. } => match store::refine_in_place(target, body, today) {
            Ok(rev) => format!("previous wording kept as {rev}"),
            Err(e) => format!("failed: {e}"),
        },
        Action::Supersede { body, .. } => {
            let name: String = body.chars().take(60).collect();
            let w = LearnedWrite {
                name: &name,
                description: "",
                mtype: target.mtype,
                body,
                source: crate::memory::provenance::ProvenanceKind::Inferred,
                confidence: 0.8,
                session_id,
                no_core: false,
                scope: None,
                subpath: None,
                // The replacement inherits WHERE the retired fact applied. Reconciliation resolves
                // content; it must not relocate a fact as a side effect.
                tier: target.tier,
                anchor: target.anchor.clone(),
                device: target.device.clone(),
                // One write does both halves: the new fact exists and the old is hidden, or neither
                // happened. No window in which two rival answers are both live.
                supersedes: Some(target.id.clone()),
            };
            match store::add_learned(&w) {
                Ok(id) => {
                    crate::memory::learning::audit::append(
                        crate::memory::learning::audit::AuditEvent {
                            ts: crate::memory::learning::audit::ts_now(),
                            session_id,
                            op: "supersede",
                            old_id: Some(&target.id),
                            new_id: Some(&id),
                            verdict: Some("contradict"),
                            ..Default::default()
                        },
                    );
                    format!("wrote {id}, retiring {}", target.id)
                }
                Err(e) => format!("failed: {e}"),
            }
        }
    }
}

/// Remove the redundant half of a `same` pair. Returns where it went, for the report.
///
/// Neither branch unlinks anything. The two cases differ because the two stores differ:
///
/// - A **live** row is retired with `supersededBy: <survivor>`, exactly as a `contradict` retires a
///   fact — so `aizen memory revive <id>` undoes it, `memory as-of` still shows it, and the row that
///   replaced it is named on disk rather than merely implied.
/// - A **review-queue** row moves to `review/.discarded/`, which is where the queue's own reject path
///   puts it. The queue is a mid-confidence fact's only copy; `supersededBy` is not available there
///   because nothing reads frontmatter in `.discarded/`.
///
/// Failure is returned, never swallowed: the caller reports "duplicate kept — <why>", because a
/// silent no-op here is what let 227 confirms remove zero rows.
fn drop_redundant(r: &Redundant, session_id: &str, survivor: &str) -> anyhow::Result<String> {
    use crate::memory::store;

    if r.live {
        let all = store::load_all()?;
        let entry = all
            .iter()
            .find(|e| e.id == r.id)
            .ok_or_else(|| anyhow::anyhow!("'{}' is no longer in the store", r.id))?;
        store::mark_superseded(entry, survivor)?;
        crate::memory::learning::audit::append(crate::memory::learning::audit::AuditEvent {
            ts: crate::memory::learning::audit::ts_now(),
            session_id,
            op: "supersede",
            old_id: Some(&r.id),
            new_id: Some(survivor),
            verdict: Some("same"),
            ..Default::default()
        });
        return Ok(format!("retired behind {survivor} (revive to undo)"));
    }

    let rdir = crate::core::config::review_dir();
    let queued = store::load_from(&rdir)?;
    let item = queued
        .iter()
        .find(|e| e.id == r.id)
        .ok_or_else(|| anyhow::anyhow!("'{}' is no longer in the review queue", r.id))?;
    let ddir = rdir.join(".discarded");
    std::fs::create_dir_all(&ddir)?;
    let dest = crate::memory::bloat::caps::unique_in(&ddir, &item.id);
    std::fs::rename(&item.path, &dest)?;
    crate::memory::learning::audit::append(crate::memory::learning::audit::AuditEvent {
        ts: crate::memory::learning::audit::ts_now(),
        session_id,
        op: "review-discard",
        old_id: Some(&r.id),
        new_id: Some(survivor),
        verdict: Some("same"),
        ..Default::default()
    });
    Ok("review/.discarded/".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::store::MemoryType;

    fn entry(id: &str, body: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            name: id.into(),
            mtype: MemoryType::User,
            body: body.into(),
            tokens: tokenize(body),
            tier: crate::memory::path_scope::Tier::User,
            ..Default::default()
        }
    }

    #[test]
    fn an_empty_store_can_only_produce_new() {
        assert_eq!(classify_local("anything at all", &[]), Verdict::New);
    }

    #[test]
    fn high_similarity_confirms_without_adding_a_row() {
        let pool = vec![entry(
            "pnpm",
            "the user prefers pnpm over npm for installing packages",
        )];
        // Same sentence → Same. This is the case that keeps the store from doubling every time the
        // user restates a standing preference.
        match classify_local(
            "the user prefers pnpm over npm for installing packages",
            &pool,
        ) {
            Verdict::Same { id } => assert_eq!(id, "pnpm"),
            other => panic!("verbatim restatement must be Same, got {other:?}"),
        }
    }

    #[test]
    fn an_unrelated_fact_is_new() {
        let pool = vec![entry(
            "pnpm",
            "the user prefers pnpm over npm for installing packages",
        )];
        assert_eq!(
            classify_local("the staging database runs in frankfurt", &pool),
            Verdict::New
        );
    }

    #[test]
    fn the_ambiguous_band_asks_instead_of_guessing() {
        // Measured at 0.687 against this pool: real overlap, but the candidate drops the
        // "and production" half, so it is a narrower claim rather than a restatement.
        let pool = vec![entry(
            "deploy",
            "the deploy pipeline uses fly for staging and production",
        )];
        let candidate = "the deploy pipeline uses fly";
        let (_, s) = best_match(candidate, &pool).expect("a match exists");
        assert!(
            (JUDGEMENT_MIN..SAME_MIN).contains(&s),
            "fixture must sit in the ambiguous band to test it, got {s}"
        );
        match classify_local(candidate, &pool) {
            Verdict::NeedsJudgement { id } => assert_eq!(id, "deploy"),
            other => panic!("the ambiguous band must defer to a human, got {other:?}"),
        }
    }

    #[test]
    fn a_reworded_duplicate_is_caught_by_minhash_not_token_overlap() {
        // Same claim, different word order/filler — this is what the MinHash half is for.
        let pool = vec![entry(
            "git",
            "git lives at c:/program files/git/cmd on this machine",
        )];
        let s = best_match(
            "on this machine git lives at c:/program files/git/cmd",
            &pool,
        )
        .map(|(_, s)| s)
        .unwrap_or(0.0);
        assert!(
            s >= SAME_MIN,
            "a reordering is still the same fact, got {s}"
        );
    }

    #[test]
    fn a_differently_worded_contradiction_scores_low_and_defers_to_phase_4() {
        // "we use npm" vs "switched to pnpm" share almost no vocabulary, so no arithmetic measure
        // can see the conflict. It lands in New — correct here, and precisely why the batched
        // off-turn reconciliation pass exists.
        let pool = vec![entry("npm", "the project installs dependencies with npm")];
        assert_eq!(
            classify_local("we have switched over to pnpm", &pool),
            Verdict::New
        );
    }

    #[test]
    fn best_match_returns_the_closest_of_several() {
        let pool = vec![
            entry("a", "the staging database runs in frankfurt"),
            entry(
                "b",
                "the user prefers pnpm over npm for installing packages",
            ),
            entry("c", "ci runs on github actions"),
        ];
        let (id, _) = best_match(
            "the user prefers pnpm over npm for installing packages",
            &pool,
        )
        .expect("a match exists");
        assert_eq!(id, "b", "must pick the closest, not the first");
    }

    // ── M2b ──────────────────────────────────────────────────────────────

    /// A pair with the target's confirmation count as the only interesting variable.
    fn pair(target_confirmations: u32) -> Pair {
        Pair {
            candidate_id: "cand".into(),
            candidate_text: "the project uses pnpm".into(),
            target_id: "target".into(),
            target_text: "the project uses npm".into(),
            similarity: 0.6,
            target_confirmations,
            candidate_is_live: true,
            candidate_confirmations: 0,
        }
    }

    fn judge(verdict: BatchVerdict, confidence: f64) -> Judgement {
        Judgement {
            pair: 0,
            verdict,
            confidence,
        }
    }

    #[test]
    fn batch_pass_is_capped_at_one_call_and_twelve_pairs() {
        // Twenty suspicious pairs arrive; the budget is one call over at most twelve of them. The
        // call counter is what makes "≤1 model call per session" checkable rather than aspirational.
        let pairs: Vec<Pair> = (0..20)
            .map(|i| Pair {
                candidate_id: format!("c{i}"),
                target_id: format!("t{i}"),
                ..pair(0)
            })
            .collect();
        let calls = std::cell::Cell::new(0);
        let report = batch_pass(
            &pairs,
            |_sys, prompt| {
                calls.set(calls.get() + 1);
                // The prompt itself must not carry more than the cap — the cap bounds the CALL, not
                // just the loop that reads the answer back.
                let numbered = prompt
                    .lines()
                    .filter(|l| l.trim_start().starts_with(|c: char| c.is_ascii_digit()))
                    .count();
                assert!(numbered <= MAX_PAIRS, "prompt carried {numbered} pairs");
                Some(r#"{"pairs":[{"n":1,"verdict":"unsure","confidence":0.9}]}"#.to_string())
            },
            true, // dry run: no store needed
            "s",
            &[],
        );
        assert_eq!(calls.get(), 1, "exactly one model call, never one per pair");
        assert_eq!(report.model_calls, 1);
        assert_eq!(
            report.pairs_judged, MAX_PAIRS,
            "judged the cap, not all twenty"
        );
    }

    #[test]
    fn a_failed_model_call_judges_nothing_and_writes_nothing() {
        // The pass must be inert on failure rather than falling back to local guesses — its actions
        // are destructive, so "no answer" has to mean "no change".
        let report = batch_pass(&[pair(0)], |_, _| None, false, "s", &[]);
        assert_eq!(report.model_calls, 0);
        assert!(report.applied.is_empty());
    }

    #[test]
    fn low_confidence_contradict_never_touches_the_old_fact() {
        // Below the apply bar, a contradiction is a suggestion, not a verdict. The old fact stays
        // exactly as it is and the pair waits for a human.
        let p = pair(0);
        let j = judge(
            BatchVerdict::Contradict {
                text: "the project uses pnpm".into(),
            },
            0.64,
        );
        match decide_action(&p, &j) {
            Action::Review { target, .. } => assert_eq!(target, "target"),
            other => panic!("0.64 is below APPLY_MIN {APPLY_MIN}, got {other:?}"),
        }
        // …and one notch above it, the same verdict does act. Otherwise this test would pass with a
        // rail that simply never applies anything.
        let j = judge(
            BatchVerdict::Contradict {
                text: "the project uses pnpm".into(),
            },
            0.66,
        );
        assert!(
            matches!(decide_action(&p, &j), Action::Supersede { .. }),
            "above the bar it must actually act"
        );
    }

    #[test]
    fn confirmed_target_always_goes_to_review() {
        // The barrier scales with what is being broken: a fact the user has leaned on twice is not
        // something a batch pass overwrites on its own authority, at ANY confidence.
        let p = pair(PROTECTED_CONFIRMATIONS);
        for conf in [0.7, 0.95, 1.0] {
            let j = judge(
                BatchVerdict::Contradict {
                    text: "pnpm now".into(),
                },
                conf,
            );
            assert!(
                matches!(decide_action(&p, &j), Action::Review { .. }),
                "a confirmed target must survive a {conf} contradiction"
            );
            let j = judge(
                BatchVerdict::Refine {
                    text: "pnpm, actually".into(),
                },
                conf,
            );
            assert!(
                matches!(decide_action(&p, &j), Action::Review { .. }),
                "…and a {conf} refine too"
            );
        }
        // `Same` is exempt: confirming adds evidence and destroys nothing.
        let j = judge(BatchVerdict::Same, 0.9);
        assert!(matches!(decide_action(&p, &j), Action::Confirm { .. }));
    }

    #[test]
    fn refine_resets_confirmations() {
        // A refine rewrites the words in place. The confirmation count measured agreement with the
        // OLD words, so it drops to min(c, 1) — and the previous wording has to stay readable.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-refine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("AIZEN_HOME", &dir);

        let id =
            crate::memory::store::add("deploy target", "", MemoryType::Project, "deploys to fly")
                .unwrap();
        let e = crate::memory::store::load_all()
            .unwrap()
            .into_iter()
            .find(|x| x.id == id)
            .unwrap();
        // Three confirmations of the OLD wording.
        for d in ["2026-07-01", "2026-07-02", "2026-07-03"] {
            crate::memory::store::confirm_use(&e, d).unwrap();
        }
        let e = crate::memory::store::load_all()
            .unwrap()
            .into_iter()
            .find(|x| x.id == id)
            .unwrap();
        assert_eq!(
            e.confirmations, 3,
            "fixture must actually accumulate confirmations"
        );

        let rev =
            crate::memory::store::refine_in_place(&e, "deploys to fly, staging only", "2026-07-27")
                .unwrap();
        let after = crate::memory::store::load_all()
            .unwrap()
            .into_iter()
            .find(|x| x.id == id)
            .unwrap();
        assert_eq!(
            after.id, id,
            "a refine keeps the id — edges and pointers must still resolve"
        );
        assert_eq!(after.body.trim(), "deploys to fly, staging only");
        assert_eq!(
            after.confirmations, 1,
            "new words have not earned the old agreement"
        );
        assert_eq!(after.last_used.as_deref(), Some("2026-07-27"));
        // The old wording survives as a revision, not as a hole in the history.
        let old = crate::memory::bloat::caps::list_archive()
            .unwrap()
            .into_iter()
            .find(|x| x.id == rev)
            .expect("previous wording parked in the archive");
        assert!(old.body.contains("deploys to fly"));

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── the duplicate half of a `same` pair actually goes away ───────────

    /// The bug this whole branch exists for. Measured on the live store 2026-08-06: 246 reconcile
    /// passes, 227 `confirm` actions, **0 rows removed** — because `Confirm` named only the target.
    /// Six copies of one fact had survived every pass. A confident `same` must now name the
    /// duplicate, or the pass is decorative.
    #[test]
    fn a_confident_same_names_the_duplicate_for_removal() {
        let p = pair(0);
        let a = decide_action(&p, &judge(BatchVerdict::Same, 0.9));
        let Action::Confirm { target, redundant } = a else {
            panic!("expected Confirm, got {a:?}");
        };
        assert_eq!(target, "target", "the survivor is still credited");
        let r = redundant.expect("the redundant candidate must be named");
        assert_eq!(r.id, "cand");
        assert!(r.live, "this fixture's candidate is a live row");
    }

    /// Removing a row is a delete, so it answers to the same two gates the destructive verdicts do —
    /// unlike the CONFIRM half, which only ever adds evidence. Both halves are checked here because
    /// the point is that they diverge: the target keeps getting credited either way.
    #[test]
    fn an_unsure_or_confirmed_duplicate_is_kept_but_the_target_is_still_credited() {
        // Gate 1: below the apply bar. Confirming is safe at any confidence; deleting is not.
        let low = decide_action(&pair(0), &judge(BatchVerdict::Same, 0.5));
        match low {
            Action::Confirm { redundant, .. } => assert!(
                redundant.is_none(),
                "a 0.5 guess must not delete a row: {redundant:?}"
            ),
            other => panic!("credit must still happen, got {other:?}"),
        }

        // Gate 2: the CANDIDATE's own confirmations. The candidate is what disappears, so its
        // standing is what protects it — a duplicate the user leaned on twice is evidence that the
        // two texts may not be the same fact after all.
        let mut p = pair(0);
        p.candidate_confirmations = PROTECTED_CONFIRMATIONS;
        match decide_action(&p, &judge(BatchVerdict::Same, 0.95)) {
            Action::Confirm { redundant, .. } => assert!(
                redundant.is_none(),
                "a confirmed duplicate is a human's call: {redundant:?}"
            ),
            other => panic!("credit must still happen, got {other:?}"),
        }
    }

    /// The invariant that makes automatic removal safe at all: **a cluster never empties.**
    ///
    /// Duplicate detection is symmetric, so three near-identical facts produce pairs A→B, B→C, C→A.
    /// Each decision is correct in isolation and deleting all three is catastrophic. The batch-level
    /// rail downgrades any pair whose target has already been retired.
    #[test]
    fn a_symmetric_cluster_never_deletes_its_last_survivor() {
        let mut removed = std::collections::HashSet::new();
        let mut survivors: Vec<&str> = vec!["a", "b", "c"];

        // The three pairs a cluster of {a,b,c} yields, in the order the batch would see them.
        for (cand, targ) in [("a", "b"), ("b", "c"), ("c", "a")] {
            let p = Pair {
                candidate_id: cand.into(),
                target_id: targ.into(),
                ..pair(0)
            };
            let action =
                guard_against_cycles(decide_action(&p, &judge(BatchVerdict::Same, 0.9)), &removed);
            if let Action::Confirm {
                redundant: Some(r), ..
            } = &action
            {
                removed.insert(r.id.clone());
                survivors.retain(|s| *s != r.id);
            }
        }

        assert!(
            !survivors.is_empty(),
            "every member of the cluster was deleted — the fact is gone"
        );
        assert_eq!(
            survivors.len(),
            1,
            "a duplicate cluster should converge to exactly one row, kept {survivors:?}"
        );
    }

    /// The rail's second rule, stated on its own: once the survivor a candidate was judged against
    /// is gone, "drop the candidate as redundant" no longer describes anything true. It may now be
    /// the last copy, so a human decides rather than the pass guessing.
    #[test]
    fn a_pair_whose_target_was_retired_goes_to_review() {
        let removed: std::collections::HashSet<String> =
            ["target".to_string()].into_iter().collect();
        let action = guard_against_cycles(
            decide_action(&pair(0), &judge(BatchVerdict::Same, 0.9)),
            &removed,
        );
        match action {
            Action::Review { why, .. } => assert!(
                why.contains("retired"),
                "the reason must say what happened, got {why:?}"
            ),
            other => panic!("expected Review once the target is gone, got {other:?}"),
        }
    }

    /// A live duplicate is retired the same way a `contradict` retires a fact: `supersededBy` set,
    /// file kept, `memory revive` able to undo it. **Nothing is unlinked** — the store's whole
    /// promise is that a model deciding something is obsolete cannot destroy it.
    #[test]
    fn dropping_a_live_duplicate_hides_it_reversibly_without_deleting_the_file() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-dupe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("AIZEN_HOME", &dir);

        let keep =
            crate::memory::store::add("keep", "", MemoryType::Project, "deploys to fly").unwrap();
        let dupe =
            crate::memory::store::add("dupe", "", MemoryType::Project, "the deploy goes to fly")
                .unwrap();

        let r = Redundant {
            id: dupe.clone(),
            live: true,
        };
        let note = drop_redundant(&r, "s", &keep).expect("the drop must succeed");
        assert!(
            note.contains("revive"),
            "the note must say it is undoable: {note}"
        );

        let all = crate::memory::store::load_all().unwrap();
        let d = all
            .iter()
            .find(|e| e.id == dupe)
            .expect("the FILE must still exist — retiring is not deleting");
        assert_eq!(
            d.superseded_by.as_deref(),
            Some(keep.as_str()),
            "the survivor must be named on disk, not merely implied"
        );
        // The point of the exercise: the active view is one row shorter.
        let live = crate::memory::bloat::supersede::active(&all);
        assert!(
            live.iter().all(|e| e.id != dupe),
            "the duplicate is still live — 227 confirms removed 0 rows exactly like this"
        );
        assert!(
            live.iter().any(|e| e.id == keep),
            "the survivor must remain"
        );

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A review-queue duplicate moves to `.discarded/` instead of being retired: the queue is that
    /// text's only copy, and nothing reads frontmatter in `.discarded/`, so `supersededBy` would be
    /// a pointer no code follows. Same rule as the queue's own reject path.
    #[test]
    fn dropping_a_queued_duplicate_moves_it_aside_rather_than_retiring_it() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-dupeq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("AIZEN_HOME", &dir);

        let rdir = crate::core::config::review_dir();
        std::fs::create_dir_all(&rdir).unwrap();
        let qid = crate::memory::store::add_learned_in(
            &rdir,
            &crate::memory::store::LearnedWrite {
                name: "queued dupe",
                description: "",
                mtype: MemoryType::Project,
                body: "the deploy goes to fly",
                source: crate::memory::provenance::ProvenanceKind::Inferred,
                confidence: 0.7,
                session_id: "s",
                no_core: false,
                scope: None,
                subpath: None,
                tier: crate::memory::path_scope::Tier::User,
                anchor: None,
                device: None,
                supersedes: None,
            },
        )
        .unwrap();

        let r = Redundant {
            id: qid.clone(),
            live: false,
        };
        let note = drop_redundant(&r, "s", "keep").expect("the drop must succeed");
        assert!(note.contains("discarded"), "got {note}");

        assert!(
            crate::memory::store::load_from(&rdir)
                .unwrap()
                .iter()
                .all(|e| e.id != qid),
            "the item must leave the live queue"
        );
        assert!(
            rdir.join(".discarded").join(format!("{qid}.md")).is_file(),
            "the text must survive in .discarded/ — rejecting is not destroying"
        );

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dry run must be exactly that. The CLI's default surface prints what it WOULD do, and a
    /// preview that quietly retires rows is worse than no preview.
    #[test]
    fn a_dry_run_removes_nothing() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-dry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("AIZEN_HOME", &dir);

        let keep =
            crate::memory::store::add("keep", "", MemoryType::Project, "deploys to fly").unwrap();
        let dupe =
            crate::memory::store::add("dupe", "", MemoryType::Project, "the deploy goes to fly")
                .unwrap();
        let all = crate::memory::store::load_all().unwrap();

        let p = Pair {
            candidate_id: dupe.clone(),
            target_id: keep.clone(),
            ..pair(0)
        };
        let report = batch_pass(
            std::slice::from_ref(&p),
            // The wire shape `parse_judgements` accepts: a `pairs` array, `n` 1-based.
            |_, _| Some(r#"{"pairs":[{"n":1,"verdict":"same","confidence":0.95}]}"#.to_string()),
            true, // dry run
            "s",
            &all,
        );
        assert_eq!(
            report.applied.len(),
            1,
            "the preview must still list the pair"
        );
        let after = crate::memory::store::load_all().unwrap();
        assert!(
            crate::memory::bloat::supersede::active(&after)
                .iter()
                .any(|e| e.id == dupe),
            "a dry run retired a row"
        );

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// End-to-end on the APPLY path, which is the only one where the batch-level rail runs: three
    /// mutually-similar facts, all three symmetric pairs judged `same` in one response. The store
    /// must end up with exactly one live row — not zero, and not three.
    ///
    /// A unit test of `guard_against_cycles` cannot catch a wiring mistake here (forgetting to
    /// record a removal, or gating the rail on the wrong flag), and a wiring mistake is what would
    /// silently empty a cluster.
    #[test]
    fn applying_a_whole_cluster_leaves_exactly_one_live_row() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-cluster-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("AIZEN_HOME", &dir);

        let ids: Vec<String> = [
            "the deploy goes to fly",
            "deploys to fly",
            "deployment targets fly",
        ]
        .iter()
        .enumerate()
        .map(|(i, b)| {
            crate::memory::store::add(&format!("dep{i}"), "", MemoryType::Project, b).unwrap()
        })
        .collect();
        let all = crate::memory::store::load_all().unwrap();

        // The three pairs a symmetric cluster of {0,1,2} produces.
        let pairs: Vec<Pair> = [(0, 1), (1, 2), (2, 0)]
            .iter()
            .map(|(c, t)| Pair {
                candidate_id: ids[*c].clone(),
                target_id: ids[*t].clone(),
                ..pair(0)
            })
            .collect();
        let response = r#"{"pairs":[
            {"n":1,"verdict":"same","confidence":0.95},
            {"n":2,"verdict":"same","confidence":0.95},
            {"n":3,"verdict":"same","confidence":0.95}]}"#;

        let report = batch_pass(&pairs, |_, _| Some(response.to_string()), false, "s", &all);
        assert_eq!(report.pairs_judged, 3);

        let after = crate::memory::store::load_all().unwrap();
        let live = crate::memory::bloat::supersede::active(&after);
        assert_eq!(
            live.len(),
            1,
            "a duplicate cluster must converge to one row, got {:?}",
            live.iter().map(|e| &e.id).collect::<Vec<_>>()
        );
        // Nothing was destroyed: all three files are still on disk, two of them retired.
        assert_eq!(after.len(), 3, "retiring is not deleting");

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A place fact anchored at `anchor`, last touched on `used`.
    fn place(id: &str, body: &str, anchor: &str, used: &str) -> MemoryEntry {
        MemoryEntry {
            id: id.into(),
            name: id.into(),
            mtype: MemoryType::Project,
            body: body.into(),
            tokens: tokenize(body),
            tier: Tier::Place,
            anchor: Some(anchor.into()),
            last_used: Some(used.into()),
            ..Default::default()
        }
    }

    #[test]
    fn newer_wins_over_deeper_on_same_chain() {
        // The spec's "nearest ancestor wins" is a rule about LOCATION. Used as the arbiter for a
        // CONTRADICTION it inverts the answer: the stale claim wins for having been written about a
        // smaller area. This is that exact case — an old `1.35` pinned deep vs a new `1.40` at the
        // repo root.
        let deep_old = place("tokio-old", "tokio 1.35", "c:/work/aizen/src", "2026-01-01");
        let shallow_new = place("tokio-new", "tokio 1.40", "c:/work/aizen", "2026-07-01");
        let w = resolve_chain(&deep_old, &shallow_new).expect("same chain");
        assert_eq!(w.winner_id, "tokio-new", "recency decides, not depth");
        assert_eq!(w.loser_id, "tokio-old");
        // The survivor re-anchors to where BOTH claims were true — resolving CONTENT must not
        // silently narrow the fact's SCOPE.
        assert_eq!(w.reanchor.as_deref(), Some("c:/work/aizen"));

        // Argument order must not change the outcome.
        let w2 = resolve_chain(&shallow_new, &deep_old).expect("same chain");
        assert_eq!(w2.winner_id, "tokio-new");

        // Same freshness → NOW depth is the only signal left, so it breaks the tie.
        let a = place("a", "tokio 1.35", "c:/work/aizen/src", "2026-07-01");
        let b = place("b", "tokio 1.40", "c:/work/aizen", "2026-07-01");
        assert_eq!(
            resolve_chain(&a, &b).unwrap().winner_id,
            "a",
            "tie → the nearer anchor"
        );

        // Different subtrees are not one chain: both facts are simultaneously true in their own
        // places, so there is nothing to arbitrate.
        let x = place("x", "tokio 1.35", "c:/work/other", "2026-01-01");
        assert!(
            resolve_chain(&x, &b).is_none(),
            "unrelated trees must not be reconciled"
        );
    }

    #[test]
    fn a_pass_only_fires_when_there_is_something_to_do() {
        assert!(
            !should_run(0, None, "2026-07-27"),
            "zero pairs is never a reason to spend a call"
        );
        assert!(
            should_run(TRIGGER_PENDING, Some("2026-07-26"), "2026-07-27"),
            "enough pairs → run"
        );
        assert!(
            !should_run(1, Some("2026-07-26"), "2026-07-27"),
            "one pair, ran yesterday → wait"
        );
        assert!(
            should_run(1, Some("2026-07-01"), "2026-07-27"),
            "a slow trickle still converges"
        );
        assert!(should_run(1, None, "2026-07-27"), "never run → due");
    }

    #[test]
    fn collect_pairs_keeps_the_closest_and_never_pairs_a_fact_with_itself() {
        let live = vec![
            entry("npm", "the project installs dependencies with npm"),
            entry(
                "fly",
                "the deploy pipeline uses fly for staging and production",
            ),
        ];
        // A candidate that IS one of the live facts must not pair with itself.
        let pairs = collect_pairs(&[live[0].clone()], &live);
        assert!(
            pairs.iter().all(|p| p.target_id != p.candidate_id),
            "a fact is not its own conflict: {pairs:?}"
        );
        // A near-restatement of `fly` pairs with `fly`, not `npm`.
        let cand = entry("cand", "the deploy pipeline uses fly");
        let pairs = collect_pairs(&[cand], &live);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].target_id, "fly");
    }

    #[test]
    fn a_garbled_response_changes_nothing_but_a_partly_good_one_still_counts() {
        // Unparseable → empty, so a confused model costs one call and no writes.
        assert!(parse_judgements("I'm not sure what you mean", 3).is_empty());
        assert!(parse_judgements(r#"{"pairs":"nonsense"}"#, 3).is_empty());
        // Out-of-range index and unknown verdict are dropped INDIVIDUALLY — one bad item must not
        // discard the good ones next to it.
        let js = parse_judgements(
            r#"{"pairs":[
                 {"n":9,"verdict":"same","confidence":0.9},
                 {"n":2,"verdict":"wat","confidence":0.9},
                 {"n":1,"verdict":"same","confidence":0.9}
               ]}"#,
            2,
        );
        assert_eq!(js.len(), 1);
        assert_eq!(js[0].pair, 0);
        // A rewrite verdict with nothing to write is Unsure, not an empty overwrite.
        let js = parse_judgements(
            r#"{"pairs":[{"n":1,"verdict":"refine","confidence":0.9}]}"#,
            1,
        );
        assert_eq!(js[0].verdict, BatchVerdict::Unsure);
    }
}
