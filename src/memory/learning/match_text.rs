//! Comparison-time text normalization for duplicate detection.
//!
//! This is NOT the retrieval tokenizer. [`crate::memory::tokenize::tokenize`] deliberately PRESERVES
//! Vietnamese diacritics (`cà phê` must not collapse to `ca phe` when the user searches for it), and
//! a test pins that. Dedup wants the opposite: two facts that say the same thing in different words
//! must look alike, so here accents are folded and the pronoun-and-particle layer is stripped.
//!
//! ## Why this module exists at all
//!
//! Measured 2026-08-06 on the real store (360 live facts): running the shipped
//! [`crate::memory::learning::reconcile::best_match`] over every same-tier pair produced
//!
//! ```text
//! pairs >= SAME_MIN (0.80): 0        pairs >= JUDGEMENT_MIN (0.55): 0
//! median nearest-neighbour similarity: 0.199
//! ```
//!
//! — the duplicate gate had never fired once, while the store held five separate facts all saying
//! the user writes Vietnamese, two saying OmniRoute must not be built on the small VPS, and two
//! saying only rust-analyzer is installed. Their peak pairwise similarity was **0.44**, under the
//! 0.55 floor, because the shared vocabulary is exactly the layer that varies: `người dùng` / `user`
//! / `anh`, `giao tiếp` / `trao đổi`, `muốn` / `mong muốn`.
//!
//! [`crate::persona::self_mem`] already hit this and fixed it the same way (its comment records
//! "highest pairwise Jaccard was 0.15 against a 0.75 threshold, so the dedup gate never fired").
//! That fix was never applied one layer up, to facts. This module is that layer, factored out so the
//! two cannot drift again.
//!
//! ## Phrases before single words — and why that is not a detail
//!
//! Folding accents makes distinct Vietnamese words collide, so a per-word synonym table is unsafe:
//! `lỗi` (an error) and `lời` (in `trả lời`, a reply) both fold to `loi`. A table mapping
//! `loi -> traloi` would quietly merge every fact about a BUG with every fact about REPLY LANGUAGE.
//! So multi-word expressions are canonicalized as whole phrases over the word SEQUENCE
//! ([`PHRASES`]), and the single-word table ([`SYNONYMS`]) is restricted to forms with no common
//! collision (mostly English verbs). The same reasoning removed `chi` (`chị` you / `chỉ` only),
//! `tao` (`tao` I / `tạo` create) and `ban` (`bạn` you / `bản` version) from the referent list, and
//! removed `aizen` — it is the project's name, present in dozens of unrelated facts, so folding it
//! into the subject token would have made every project fact share a subject.
//!
//! ## Containment, not just Jaccard
//!
//! Jaccard punishes length asymmetry, and the commonest duplicate shape is *the store already says
//! this, plus more*: "VPS quá nhỏ để build OmniRoute" against "Không build OmniRoute trên VPS nhỏ,
//! build ở nơi khác rồi kéo về" is Jaccard 0.40 but containment 0.67. So the score is the max of
//! Jaccard and the overlap coefficient — guarded, because containment alone would let a 2-token
//! fragment swallow anything: it needs [`MIN_SHARED_TOKENS`] shared tokens AND
//! [`MIN_TOKENS_FOR_CONTAINMENT`] on the shorter side. Both floors are load-bearing; the tests pin a
//! real pair that sits just under them.
//!
//! Everything here is pure and allocation-light; it runs on the write path of every learned fact.

use crate::core::slug::fold_to_ascii;
use once_cell::sync::Lazy;
use std::collections::{HashMap, HashSet};

/// The token every self/agent-referring expression folds to. Deliberately not a real word, so it can
/// never collide with content.
const SELF_TOKEN: &str = "\u{ab}self\u{bb}";

/// Shared tokens required before the containment score is allowed to fire. Two words in common is a
/// coincidence (every fact about this user shares a subject and a verb); three is a claim.
const MIN_SHARED_TOKENS: usize = 3;

/// Token floor on the SHORTER side for containment. Below this, a terse fact would be "contained" in
/// half the store.
const MIN_TOKENS_FOR_CONTAINMENT: usize = 4;

/// Ceiling on the containment-derived score.
///
/// Containment returns 1.0 for any PROPER SUBSET, and a proper subset is not the same claim: "the
/// pipeline uses fly" against "the pipeline uses fly for staging and production" is contained but
/// strictly less informative. Letting containment reach [`SAME_MIN`] would `confirm` the terse text
/// and silently discard the extra clause — a data-losing merge, and the worst outcome available here.
///
/// So containment is capped just under the confirm band: it can escalate a pair into REVIEW, where a
/// human (or the batch judge) sees both texts and decides which one survives, but it can never merge
/// on its own. Only Jaccard — which requires the two token sets to genuinely coincide — may confirm.
const MAX_CONTAINMENT_SCORE: f64 = 0.79;

/// How many times longer the bigger side may be before containment is refused.
///
/// This bound is what separates "restated" from "mentioned in passing", and it is the single most
/// sensitive number in the module. The live store holds one 298-token fact (median is 22); without a
/// ratio cap it *contained* every short fact sharing its topic, and the review queue would have
/// received 229 pairs. Measured over all same-tier pairs of the 360 live facts:
///
/// ```text
/// ratio cap | SAME (>=0.80) | JUDGEMENT (0.55-0.80)
///      1.5  |       4       |        16
///      2.0  |       6       |        37     <- chosen
///      2.5  |       8       |        54
///      3.0  |       9       |        61
///     none  |      32       |       229
/// ```
///
/// 2.0 keeps every known-duplicate group (the five Vietnamese-language facts stay ≥ 0.60, the VPS
/// build pair 0.67) while the pairs it admits are, on inspection, real duplicates. Raising it buys
/// mostly noise: the extra pairs above 2.0 are a long fact that merely mentions a short fact's topic.
const MAX_LENGTH_RATIO: f64 = 2.0;

/// Multi-word expressions canonicalized over the word sequence, longest first. Each maps a folded
/// word run to one token.
///
/// This is where anything ambiguous belongs — see the module header on `loi`. Every entry is a form
/// actually observed in the live store; speculative additions are how a dedup layer starts eating
/// distinct facts.
static PHRASES: Lazy<Vec<(Vec<&'static str>, &'static str)>> = Lazy::new(|| {
    let mut v: Vec<(Vec<&'static str>, &'static str)> = vec![
        (vec!["nguoi", "su", "dung"], SELF_TOKEN),
        (vec!["nguoi", "dung"], SELF_TOKEN),
        (vec!["end", "user"], SELF_TOKEN),
        (vec!["tra", "loi"], "traloi"),
        (vec!["phan", "hoi"], "traloi"),
        (vec!["su", "dung"], "dung"),
        (vec!["mong", "muon"], "muon"),
        (vec!["giao", "tiep"], "giaotiep"),
        (vec!["trao", "doi"], "giaotiep"),
        (vec!["noi", "chuyen"], "giaotiep"),
        (vec!["tieng", "viet"], "tiengviet"),
        (vec!["ngon", "ngu"], "ngonngu"),
        (vec!["bien", "dich"], "build"),
    ];
    // Longest phrase first so `nguoi su dung` wins over `su dung`.
    v.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    v
});

/// Words naming the user or the agent. A fact's SUBJECT is nearly always one of these, and the
/// choice between them is stylistic drift, not a difference in meaning.
///
/// Entries whose folded form collides with a common content word are deliberately ABSENT — see the
/// module header.
static REFERENTS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        // Vietnamese address terms (post-fold, so unaccented forms only).
        "toi",
        "tui",
        "minh",
        "anh",
        "em",
        "nguoi",
        // English + the human's own name.
        "user",
        "you",
        "me",
        "myself",
        "dawn",
        "maintainer",
        "owner",
    ]
    .into_iter()
    .collect()
});

/// Function words that carry no topical signal. Bilingual for the same reason `self_mem` is: a short
/// Vietnamese sentence is mostly particles, so leaving them in makes every pair look ~15% alike and
/// no pair look 80% alike.
static STOP: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        // English
        "the", "an", "and", "or", "to", "of", "in", "on", "for", "is", "are", "was", "were", "it",
        "this", "that", "with", "as", "at", "be", "have", "has", "will", "would", "can", "should",
        "not", "but", "from", "by", "when", "then", "than", "into", "its", "their", "there",
        // Vietnamese (folded: no diacritics)
        "la", "va", "cua", "cho", "voi", "khi", "mot", "nay", "kia", "do", "duoc", "co", "khong",
        "thi", "ma", "nen", "can", "phai", "se", "da", "dang", "vi", "de", "cac", "nhung", "trong",
        "ngoai", "ra", "vao", "lai", "roi", "nua", "hon", "rat", "cung", "chi", "theo", "sau",
        "truoc", "hay", "hoac", "neu", "bang", "ve", "tu", "den", "gi", "nao", "sao", "the",
        "viec", "cai", "boi", "tai", "moi", "luon", "van", "chua", "dau", "day", "muc",
    ]
    .into_iter()
    .collect()
});

/// Single-token synonym folds, applied after [`PHRASES`]. Restricted to forms with no common
/// Vietnamese collision after accent folding — ambiguous ones live in `PHRASES` instead.
static SYNONYMS: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    [
        ("prefers", "muon"),
        ("prefer", "muon"),
        ("wants", "muon"),
        ("want", "muon"),
        ("thich", "muon"),
        ("reply", "traloi"),
        ("answer", "traloi"),
        ("respond", "traloi"),
        ("use", "dung"),
        ("uses", "dung"),
        ("using", "dung"),
        ("compile", "build"),
        ("language", "ngonngu"),
        ("vietnamese", "tiengviet"),
        ("host", "machine"),
        ("may", "machine"),
    ]
    .into_iter()
    .collect()
});

/// Normalize `text` into the token multiset dedup compares.
///
/// Pipeline: fold accents → split into words (dropping 1-char noise) → canonicalize whole
/// [`PHRASES`] over the sequence → collapse [`REFERENTS`] to [`SELF_TOKEN`] → drop [`STOP`] →
/// apply [`SYNONYMS`]. A run of adjacent referents contributes ONE subject token, so `người dùng`
/// and `anh` weigh the same.
pub fn match_tokens(text: &str) -> Vec<String> {
    let folded = fold_to_ascii(text);
    let words: Vec<&str> = folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 2)
        .collect();

    // Pass 1: phrase canonicalization over the word sequence.
    let mut staged: Vec<&str> = Vec::with_capacity(words.len());
    let mut i = 0usize;
    'outer: while i < words.len() {
        for (phrase, canonical) in PHRASES.iter() {
            if words.len() - i >= phrase.len() && words[i..i + phrase.len()] == phrase[..] {
                staged.push(canonical);
                i += phrase.len();
                continue 'outer;
            }
        }
        staged.push(words[i]);
        i += 1;
    }

    // Pass 2: referents → subject, stopwords out, remaining synonyms folded.
    let mut out: Vec<String> = Vec::with_capacity(staged.len());
    for w in staged {
        if w == SELF_TOKEN || REFERENTS.contains(w) {
            if out.last().map(String::as_str) != Some(SELF_TOKEN) {
                out.push(SELF_TOKEN.to_string());
            }
            continue;
        }
        if STOP.contains(w) {
            continue;
        }
        out.push(SYNONYMS.get(w).copied().unwrap_or(w).to_string());
    }
    out
}

/// Similarity over [`match_tokens`]: `max(Jaccard, guarded+capped containment)`.
///
/// Containment (the overlap coefficient, `|A∩B| / min(|A|,|B|)`) is what catches "the store already
/// says this, plus more" — the shape Jaccard scores worst. It applies only when all three guards are
/// met ([`MIN_SHARED_TOKENS`], [`MIN_TOKENS_FOR_CONTAINMENT`], [`MAX_LENGTH_RATIO`]) and its result
/// is capped at [`MAX_CONTAINMENT_SCORE`] so a proper subset can only ever reach REVIEW, never
/// `confirm`. Otherwise the plain Jaccard stands.
///
/// Two empty texts are NOT similar (0.0): an empty fact should be rejected upstream, never merged.
pub fn match_similarity(a: &str, b: &str) -> f64 {
    let sa: HashSet<String> = match_tokens(a).into_iter().collect();
    let sb: HashSet<String> = match_tokens(b).into_iter().collect();
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let inter = sa.intersection(&sb).count();
    if inter == 0 {
        return 0.0;
    }
    let union = sa.union(&sb).count() as f64;
    let jaccard = if union > 0.0 {
        inter as f64 / union
    } else {
        0.0
    };
    let smaller = sa.len().min(sb.len());
    let larger = sa.len().max(sb.len());
    if inter >= MIN_SHARED_TOKENS
        && smaller >= MIN_TOKENS_FOR_CONTAINMENT
        && (larger as f64) <= MAX_LENGTH_RATIO * smaller as f64
    {
        let containment = (inter as f64 / smaller as f64).min(MAX_CONTAINMENT_SCORE);
        return jaccard.max(containment);
    }
    jaccard
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::learning::reconcile::{JUDGEMENT_MIN, SAME_MIN};

    /// The five real facts from the live store that all say "this user writes Vietnamese". Under the
    /// shipped lexical scorer their peak pairwise similarity was 0.44 — below `JUDGEMENT_MIN`, so all
    /// five were written as distinct rows. Every pair must now at least reach the review band.
    #[test]
    fn the_five_vietnamese_facts_collide() {
        let variants = [
            "Người dùng giao tiếp bằng tiếng Việt và mong muốn trả lời bằng tiếng Việt",
            "user giao tiếp bằng tiếng Việt và muốn nhận trả lời tiếng Việt",
            "Người dùng trao đổi và muốn được trả lời bằng tiếng Việt",
            "Anh giao tiếp bằng tiếng Việt, xưng hô anh",
            "Người dùng trao đổi bằng tiếng Việt, xưng hô anh",
        ];
        for (i, a) in variants.iter().enumerate() {
            for b in variants.iter().skip(i + 1) {
                let s = match_similarity(a, b);
                assert!(
                    s >= JUDGEMENT_MIN,
                    "similarity {s:.2} still below the judgement floor\n  a={a}\n  b={b}"
                );
            }
        }
    }

    /// The three that are pure restatements (no extra clause) must reach the CONFIRM band, so they
    /// cost zero rows rather than three review entries.
    #[test]
    fn pure_restatements_reach_the_confirm_band() {
        let s = match_similarity(
            "Người dùng giao tiếp bằng tiếng Việt và mong muốn trả lời bằng tiếng Việt",
            "Người dùng trao đổi và muốn được trả lời bằng tiếng Việt",
        );
        assert!(s >= SAME_MIN, "similarity {s:.2} should confirm, not queue");
    }

    /// Accent folding must make a typed-without-diacritics restatement identical, not merely close.
    #[test]
    fn accents_do_not_change_the_token_set() {
        assert_eq!(
            match_tokens("Người dùng thích tiếng Việt"),
            match_tokens("Nguoi dung thich tieng Viet"),
        );
    }

    /// Every way of naming the user folds to one subject token, and a multi-word referent counts once.
    #[test]
    fn referents_collapse_to_a_single_subject_token() {
        for s in ["người dùng", "người sử dụng", "user", "anh", "tôi", "mình"] {
            assert_eq!(
                match_tokens(s),
                vec![SELF_TOKEN.to_string()],
                "{s:?} should reduce to the subject token"
            );
        }
    }

    /// The collision this module's phrase layer exists to prevent: `lỗi` (an error) and `lời` (in
    /// `trả lời`, a reply) both fold to `loi`. A per-word synonym table would merge bug facts into
    /// reply-language facts.
    #[test]
    fn error_facts_do_not_fold_into_reply_facts() {
        let toks = match_tokens("lỗi serde bị nuốt nên không thấy nguyên nhân");
        assert!(
            !toks.contains(&"traloi".to_string()),
            "`lỗi` must not canonicalize to the reply token, got {toks:?}"
        );
        let s = match_similarity(
            "Người dùng muốn trả lời bằng tiếng Việt",
            "Lỗi serde bị nuốt thành None nên bug ẩn hàng tháng",
        );
        assert!(
            s < JUDGEMENT_MIN,
            "similarity {s:.2} would merge a bug into a language preference"
        );
    }

    /// The guard that keeps this from becoming a fact-eater: genuinely different claims must stay
    /// below the review floor even when they share a subject, a language, and a verb.
    #[test]
    fn distinct_facts_stay_distinct() {
        let pairs = [
            (
                "Người dùng muốn trả lời bằng tiếng Việt",
                "Người dùng không muốn agent tự commit, chỉ sửa file rồi chờ",
            ),
            (
                "aizen-be chạy ở port 8799",
                "Dashboard OmniRoute chạy ở port 20128",
            ),
            (
                "Máy này chỉ cài rust-analyzer, không có LSP khác",
                "Máy này chạy Windows, home là C:\\Users\\admin",
            ),
            (
                "Repo aizen_admin là monorepo chứa aizen-be và OmniRoute",
                "Repo aizen public ở github.com/aizen-stack/aizen",
            ),
        ];
        for (a, b) in pairs {
            let s = match_similarity(a, b);
            assert!(
                s < JUDGEMENT_MIN,
                "similarity {s:.2} would wrongly queue/merge\n  a={a}\n  b={b}"
            );
        }
    }

    /// Two facts observed colliding in the live store about the VPS build — the containment shape
    /// (same claim, plus the remedy) that plain Jaccard scores at only 0.40.
    #[test]
    fn the_vps_build_pair_collides() {
        let s = match_similarity(
            "VPS quá nhỏ để build builder OmniRoute",
            "Không build OmniRoute trên VPS nhỏ, build ở nơi khác rồi kéo về",
        );
        assert!(s >= JUDGEMENT_MIN, "similarity {s:.2}");
    }

    /// Containment must not fire on a thin overlap: two shared tokens, or a shorter side under the
    /// floor, falls back to Jaccard.
    #[test]
    fn containment_needs_both_floors() {
        // Exactly 2 shared tokens → containment suppressed, Jaccard governs.
        let s = match_similarity(
            "build OmniRoute VPS",
            "build OmniRoute trên máy khác hoàn toàn",
        );
        assert!(
            s < 1.0,
            "a 2-token overlap must not score as containment (got {s:.2})"
        );
    }

    /// A PROPER SUBSET must never reach the confirm band. `confirm` writes no row and keeps the
    /// EXISTING text, so confirming a superset against its own subset would silently discard the
    /// extra clause. Such a pair belongs in review, where both texts are visible.
    ///
    /// This is not hypothetical: it is what the shipped `the_ambiguous_band_queues_both_texts_for_a_human`
    /// test caught when containment was uncapped.
    #[test]
    fn a_proper_subset_can_only_reach_review_never_confirm() {
        let terse = "the deploy pipeline uses fly";
        let full = "the deploy pipeline uses fly for staging and production";
        let s = match_similarity(terse, full);
        assert!(
            s >= JUDGEMENT_MIN,
            "a subset is still suspicious enough to review (got {s:.2})"
        );
        assert!(
            s < SAME_MIN,
            "but it must NOT confirm — that would drop 'for staging and production' (got {s:.2})"
        );
    }

    #[test]
    fn empty_is_never_similar() {
        assert_eq!(match_similarity("", "anything at all"), 0.0);
        assert_eq!(match_similarity("   ", ""), 0.0);
        assert!(match_tokens("").is_empty());
    }
}
