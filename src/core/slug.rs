//! The one definition of "a name a human can read and retype".
//!
//! Five surfaces independently turn free text into a filename or id: memory entries, `#remember`
//! captures, persona self-memories, session saves, and the project zone key. Every one of them had
//! written its own loop, and four of the five made the same mistake — testing one codepoint at a
//! time against `is_ascii_alphanumeric`. An accented letter fails that test, so it became a word
//! SEPARATOR and the name was cut apart from the inside:
//!
//! ```text
//! "Người dùng giao tiếp bằng tiếng Việt"  →  ng-i-d-ng-giao-ti-p-b-ng-ti-ng-vi-t
//! ```
//!
//! Measured before the fix: 185/243 memory entries and 45/89 persona self-memories were shredded
//! like that. An id nobody can read is an id nobody can pass to `show`/`edit`/`forget`.
//!
//! The fix is the ORDER of operations, not the character set. Fold the accent off the letter FIRST,
//! then decide where words end. This module owns that order so the surfaces cannot drift again.

/// Cap on a slug's length in CHARACTERS.
///
/// Matches the 60 that `learning::secretary::fact_name` cuts display names to, so a learned fact's
/// id is the whole of its name rather than a second, shorter guess at it. Folding to ASCII keeps
/// chars and bytes equal, so this is also the byte length — comfortably inside the Windows 260
/// limit even when the store path is nested.
pub const MAX_ID_CHARS: usize = 60;

/// Lowercase and strip diacritics, leaving the base letters joined to their word.
///
/// 1. NFD-decompose: a precomposed `ế` splits into `e` plus two combining marks. `is_alphanumeric`
///    is false for those marks, so they must be dropped BEFORE any word-boundary test.
/// 2. Fold what NFD leaves alone. `đ`/`Đ` are their own letters in the Vietnamese alphabet, not `d`
///    plus a mark, so no normalization touches them — without this `đường` folds to `uong`.
/// 3. Everything else is left for the caller to classify. Non-Latin scripts have no ASCII fold, so
///    they collapse to separators rather than transliterate; `-` still means "a word ended here",
///    which is the only property the id depends on.
pub fn fold_to_ascii(text: &str) -> String {
    let decomposed = icu_normalizer::DecomposingNormalizer::new_nfd().normalize(text);
    let mut out = String::with_capacity(decomposed.len());
    for c in decomposed.chars() {
        match c {
            // U+0300..=U+036F is Combining Diacritical Marks — every mark NFD produces for the
            // Latin script, including the Vietnamese tone that stacks on an already-modified vowel.
            '\u{0300}'..='\u{036F}' => {}
            'đ' | 'Đ' => out.push('d'),
            _ => out.push(c),
        }
    }
    out.to_lowercase()
}

/// Cut to at most `max` chars WITHOUT leaving a half-word at the end.
///
/// A blind `take(max)` turns `…-tiep-bang-tieng-viet` into `…-tiep-bang-tien`, and `tien` is a
/// different Vietnamese word from `tieng` — the truncated id would read as a fact about something
/// else. Backing up to the last `-` keeps the id a sequence of whole words. A single word longer
/// than the cap is still cut, since the alternative is exceeding the limit.
pub fn truncate_at_word(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    let stem = match cut.rfind('-') {
        Some(i) if i > 0 => &cut[..i],
        _ => cut.as_str(),
    };
    stem.trim_matches('-').to_string()
}

/// Fold, then split into whole ASCII words joined by `-`. The shared core of every id-producing
/// path: `word-word-word`, no leading/trailing dash, no run of dashes, nothing cut mid-word.
///
/// Returns an empty string when nothing survives (all punctuation, all emoji); each caller picks
/// its own fallback stem, since "memory"/"note"/"mem"/"session" carry different meaning to a user
/// staring at a directory listing.
pub fn slug_words(text: &str, max_chars: usize) -> String {
    let mut s = String::new();
    let mut prev_dash = false;
    for c in fold_to_ascii(text.trim()).chars() {
        if c.is_ascii_alphanumeric() {
            s.push(c);
            prev_dash = false;
        } else if !prev_dash && !s.is_empty() {
            s.push('-');
            prev_dash = true;
        }
    }
    truncate_at_word(s.trim_matches('-'), max_chars)
}

/// Like [`slug_words`] but stops after `max_words` whole words — for stems that want a fixed number
/// of words rather than a character budget.
pub fn slug_first_words(text: &str, max_words: usize, max_chars: usize) -> String {
    let full = slug_words(text, usize::MAX);
    let head: Vec<&str> = full
        .split('-')
        .filter(|w| !w.is_empty())
        .take(max_words)
        .collect();
    truncate_at_word(&head.join("-"), max_chars)
}

/// Vendor key prefixes, checked on the RAW token — stripping `-`/`_` first would erase the very
/// evidence, turning `sk-…` into an innocent-looking `sk…`.
const VENDOR_PREFIXES: &[&str] = &[
    "sk-", "sk_", "pk-", "pk_", "rk_", "sk-ant-", "ghp_", "gho_", "ghu_", "ghs_", "ghr_",
    "github_pat_", "glpat-", "xoxb-", "xoxa-", "xoxp-", "xoxr-", "xoxs-", "aiza", "ya29.", "hf_",
    "npm_", "sg.", "akia", "asia", "tvly-", "tvly_", "eyj", "dop_v1_", "shpat_", "sq0atp-",
    "sq0csp-", "lin_api_", "figd_", "pplx-", "gsk_", "xai-", "or-v1-",
];

/// A token that carries a known vendor key prefix. Narrow by design — no shape heuristic, so it can
/// be run over arbitrary stored text (JSON transcripts, logs) without matching timestamps or hashes.
///
/// Measured on 27 real session transcripts: this matches 12 strings, all of them real keys, while the
/// shape test in [`looks_like_credential`] matched 5170 — 4026 of those were ISO timestamps
/// (`2026-08-05T18:12:33Z` is long, mixed-case and mixes letters with digits, exactly like key
/// material). Shape is the right test for one word in a sentence; prefix is the right test for a
/// haystack.
pub fn has_vendor_key_prefix(raw: &str) -> bool {
    let t = raw.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if t.chars().count() < 20 {
        return false; // a real key is long; `sk-` alone in prose is not one
    }
    let lower = t.to_ascii_lowercase();
    VENDOR_PREFIXES.iter().any(|p| lower.starts_with(p))
}

/// Does this single token look like a credential, rather than a word someone typed?
///
/// Deliberately NOT the prose-oriented regex redaction in `agent::codebase`/`agent::mcp`: this runs
/// per-token while DERIVING a name, where the token has no surrounding `key = …` context to match on
/// and the cost of a false positive is one word missing from a suggested filename.
///
/// It exists because an auto-derived name is written to disk and PRINTED — `/sessions` renders the
/// stem. A key pasted as the first line of a chat therefore became a filename that shows up in the
/// picker, in `ls`, and in every backup. One such name was found on a real machine: 40 chars, a
/// vendor prefix, no dashes left after sanitizing.
///
/// Two independent tests, because vendor prefixes only cover keys we know about:
///   1. A known vendor prefix ([`has_vendor_key_prefix`]).
///   2. Shape: long, and mixing character classes the way random material does and words do not.
///
/// The shape half is only safe on a token a human typed as a word. Do not reuse this to scan stored
/// text — an ISO timestamp passes it. Use [`has_vendor_key_prefix`] for that.
pub fn looks_like_credential(raw: &str) -> bool {
    let t = raw.trim_matches(|c: char| !c.is_ascii_alphanumeric());
    if t.chars().count() < 12 {
        return false; // too short to be worth a key's entropy; also where real words live
    }
    let lower = t.to_ascii_lowercase();
    // `ng_` is an Aizen role-key placeholder, short enough to want the lower bar here but too
    // ambiguous for haystack scanning.
    if VENDOR_PREFIXES.iter().any(|p| lower.starts_with(p)) || lower.starts_with("ng_") {
        return true;
    }
    // Shape test on the alphanumeric core, so `sk-abc-123` and `skabc123` score the same.
    let core: String = t.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let n = core.chars().count();
    if n < 16 {
        return false;
    }
    let has_digit = core.chars().any(|c| c.is_ascii_digit());
    let has_alpha = core.chars().any(|c| c.is_ascii_alphabetic());
    let mixed_case = core.chars().any(|c| c.is_ascii_uppercase())
        && core.chars().any(|c| c.is_ascii_lowercase());
    // Digits AND letters together is the signal; mixed case makes 16 enough, otherwise require the
    // length no ordinary word reaches. A lowercase-only run of letters, however long, is prose.
    (has_digit && has_alpha && (mixed_case || n >= 24))
        // A long pure-hex run is a digest or a key, never a word.
        || (n >= 32 && core.chars().all(|c| c.is_ascii_hexdigit()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect this module exists to prevent: accented text cut into one-letter fragments.
    #[test]
    fn vietnamese_folds_into_whole_words() {
        assert_eq!(
            slug_words("Người dùng giao tiếp bằng tiếng Việt", MAX_ID_CHARS),
            "nguoi-dung-giao-tiep-bang-tieng-viet"
        );
        assert_eq!(slug_words("Đường dẫn tới thư mục", MAX_ID_CHARS), "duong-dan-toi-thu-muc");
    }

    /// Every Vietnamese vowel+tone combination folds to its bare ASCII letter, and `đ` is the one
    /// letter no normalization form decomposes. Asserted per-group against the expected base letter
    /// rather than against one hand-counted string, so a miscount in the test can't read as a bug in
    /// the fold (or hide one).
    #[test]
    fn folds_the_whole_vietnamese_alphabet() {
        let groups = [
            ('a', "àáảãạăằắẳẵặâầấẩẫậ"),
            ('e', "èéẻẽẹêềếểễệ"),
            ('i', "ìíỉĩị"),
            ('o', "òóỏõọôồốổỗộơờớởỡợ"),
            ('u', "ùúủũụưừứửữự"),
            ('y', "ỳýỷỹỵ"),
            ('d', "đ"),
        ];
        for (base, accented) in groups {
            for c in accented.chars() {
                let got = fold_to_ascii(&c.to_string());
                assert_eq!(got, base.to_string(), "{c:?} folded to {got:?}, want {base:?}");
            }
            // Uppercase folds to the same lowercase base.
            let upper: String = accented.chars().flat_map(|c| c.to_uppercase()).collect();
            assert_eq!(
                fold_to_ascii(&upper),
                base.to_string().repeat(accented.chars().count()),
                "uppercase {base:?} group"
            );
        }
    }

    /// A cut must not leave a fragment that reads as a different word.
    #[test]
    fn cuts_at_a_word_boundary() {
        let s = slug_words("nguoi dung giao tiep bang tieng viet va mong muon tra loi", 30);
        assert!(s.chars().count() <= 30, "{} chars", s.chars().count());
        assert!(!s.ends_with('-'));
        let src: Vec<&str> = "nguoi dung giao tiep bang tieng viet va mong muon tra loi"
            .split(' ')
            .collect();
        for w in s.split('-') {
            assert!(src.contains(&w), "{w:?} is a fragment, not a whole word (from {s})");
        }
    }

    /// A single word past the cap has no boundary to back up to; it must still be bounded.
    #[test]
    fn caps_a_single_overlong_word() {
        assert_eq!(slug_words(&"a".repeat(200), MAX_ID_CHARS).chars().count(), MAX_ID_CHARS);
    }

    /// Composed vs decomposed spellings of one name must produce one id, or a store written on
    /// macOS (NFD) grows duplicates when read on Windows (NFC).
    #[test]
    fn normalization_insensitive() {
        assert_eq!(slug_words("Việt", 60), slug_words("Việt", 60)); // NFC vs NFD source bytes
        assert_eq!(slug_words("cà phê", 60), "ca-phe");
    }

    /// Migrations re-run the slug over names that are already slugs; a second pass must be a no-op
    /// or ids would keep changing on every startup.
    #[test]
    fn idempotent() {
        for src in ["Người dùng giao tiếp", "Prefer pnpm over npm", "!!!", "a b  c"] {
            let once = slug_words(src, MAX_ID_CHARS);
            assert_eq!(slug_words(&once, MAX_ID_CHARS), once, "not idempotent for {src:?}");
        }
    }

    /// Nothing survivable → empty, so each caller can apply its own fallback stem.
    #[test]
    fn empty_when_nothing_survives() {
        assert_eq!(slug_words("!!!", 60), "");
        assert_eq!(slug_words("   ", 60), "");
        assert_eq!(slug_words("🎉🎉", 60), "");
    }

    /// Non-Latin scripts have no ASCII fold: they collapse rather than transliterate, and the
    /// surrounding ASCII words stay whole.
    #[test]
    fn non_latin_collapses_without_eating_neighbours() {
        assert_eq!(slug_words("deploy 部署 pipeline", 60), "deploy-pipeline");
    }

    #[test]
    fn first_words_takes_whole_words_only() {
        assert_eq!(
            slug_first_words("Người dùng giao tiếp bằng tiếng Việt", 3, MAX_ID_CHARS),
            "nguoi-dung-giao"
        );
        // Fewer words available than asked for is not an error.
        assert_eq!(slug_first_words("hai tu", 5, MAX_ID_CHARS), "hai-tu");
        assert_eq!(slug_first_words("!!!", 5, MAX_ID_CHARS), "");
    }

    /// Credential shapes a derived filename must never carry.
    #[test]
    fn flags_credential_shaped_tokens() {
        for t in [
            "sk-abcdefghijklmnopqrstuvwx",
            "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAA",
            "tvly-dev-abc123def456ghi789jkl012",
            "ghp_abcdefghijklmnopqrstuvwxyz0123",
            "github_pat_11ABCDEFG0abcdefghijkl",
            "AKIAIOSFODNN7EXAMPLE",
            "AIzaSyBcDeFgHiJkLmNoPqRsTuVwXyZ01234567",
            "xoxb-123456789012-abcdefghijkl",
            "glpat-AbCdEfGhIjKlMnOpQrSt",
            "hf_AbCdEfGhIjKlMnOpQrStUvWxYz0123",
            // No known prefix — caught by shape alone (mixed case + digits, ≥16).
            "aB3dE5gH7jK9mN1pQ2rS",
            // Lowercase+digits but long enough that no word reaches it.
            "abc123def456ghi789jkl012mno",
            // A bare 64-char hex digest.
            &"a1b2c3d4".repeat(8),
        ] {
            assert!(looks_like_credential(t), "missed credential shape: {t}");
        }
    }

    /// The false-positive side. These are words a user actually types as the first line of a chat;
    /// dropping one from the suggested name would be a silent, confusing loss.
    #[test]
    fn passes_ordinary_words() {
        for t in [
            "hello",
            "authentication",
            "refactoring",
            "implementation",
            // Long lowercase prose words, well past the length gate but no digits.
            "internationalization",
            "responsibilities",
            // Vietnamese, already folded by the caller before this runs.
            "nguoi",
            "duong-dan-toi-thu-muc",
            // Version-ish and identifier-ish text that carries digits but stays short.
            "v0.5.7",
            "utf8",
            "sha256",
            "windows11",
            "rust2024",
            // Snake/kebab identifiers from real code.
            "max_id_chars",
            "session-name",
            "fold_to_ascii",
        ] {
            assert!(!looks_like_credential(t), "false positive on: {t}");
        }
    }

    /// The vendor prefix must be tested on the raw token: stripping separators first would turn
    /// `sk-…` into `sk…` and lose the evidence.
    #[test]
    fn vendor_prefix_survives_separator_stripping() {
        assert!(looks_like_credential("sk-liveabcdefghijklmnop"));
        assert!(looks_like_credential("  sk-liveabcdefghijklmnop  "));
        assert!(looks_like_credential("\"sk-liveabcdefghijklmnop\""));
    }

    /// The haystack test is narrow ON PURPOSE. Measured over 27 real session transcripts, the shape
    /// half of `looks_like_credential` matched 5170 tokens — 4026 of them ISO timestamps, which are
    /// long, mixed-case, and mix letters with digits exactly like key material. A warning that fires
    /// on every file is a warning the user learns to ignore, so scanning stored text uses prefixes
    /// only.
    #[test]
    fn haystack_scan_ignores_timestamps_and_hashes() {
        for benign in [
            "2026-08-05T18:12:33.123Z",
            "2026-08-05T18:12:33+07:00",
            "2026-08-05",
            // A git sha and a content hash: the shape test flags these, the prefix test must not.
            "64942a9f8b3c1d2e5a6f7b8c9d0e1f2a3b4c5d6e",
            "6fecc5ae1b2c3d4e5f60718293a4b5c6",
            // Long identifiers from real transcripts.
            "toolu_01LUn5WWW2hqdr4cMuMya82z",
            "aizen-2c0e7968",
        ] {
            assert!(
                !has_vendor_key_prefix(benign),
                "haystack scan false-positived on: {benign}"
            );
        }
        // Real keys still caught.
        for keyish in [
            "sk-ant-api03-AAAAAAAAAAAAAAAAAAAAAAAA",
            "tvly-dev-abc123def456ghi789jkl012",
            "ghp_abcdefghijklmnopqrstuvwxyz0123",
            "AKIAIOSFODNN7EXAMPLE",
        ] {
            assert!(has_vendor_key_prefix(keyish), "haystack scan missed: {keyish}");
        }
        // A bare `sk-` in prose is not a key — the length floor rejects it.
        assert!(!has_vendor_key_prefix("sk-test"));
    }
}
