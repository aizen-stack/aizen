//! Content-type classification of a memory fact (P3 — CoALA memory typing).
//!
//! A second DERIVED axis, ORTHOGONAL to both [`crate::memory::store::MemoryType`] (a *storage +
//! scope-routing* axis: user/feedback→global, project/reference→zoned) and
//! [`crate::memory::dimension::Dimension`] (the *user-profile topical* axis: style/tooling/…).
//! Where `Dimension` answers "what facet of the USER is this", `Category` answers "what KIND of
//! project knowledge is this" — the taxonomy the research design calls for: bug history, failed
//! attempts, successful patterns, architecture decisions, commands, security rules, deploy notes,
//! codebase facts.
//!
//! Mapped onto the neural memory model (CoALA / Generative-Agents): each category rolls up to an
//! [`Kind`] — EPISODIC (things that happened: a bug, a failed try, a fix that worked, a command
//! run), SEMANTIC (durable knowledge: an architecture decision, a codebase fact), or PROCEDURAL
//! (reusable how-to: a security rule, a deploy note). Retrieval + evolution policies can key off
//! `Kind` without caring about the finer category.
//!
//! DERIVED, not stored: `classify` runs on load (a cheap bilingual lexicon scan), so the tag never
//! goes stale as the lexicon evolves — same posture as `dimension`. EN + VI (the user is Vietnamese).

/// The coarse neural-memory bucket a [`Category`] belongs to (research's episodic/semantic/procedural).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Something that HAPPENED at a point in time (a bug, a failed attempt, a fix, a command run).
    Episodic,
    /// Durable KNOWLEDGE about the world/project (an architecture decision, a codebase fact).
    Semantic,
    /// Reusable HOW-TO / rule (a security rule, a deploy note).
    Procedural,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Episodic => "episodic",
            Kind::Semantic => "semantic",
            Kind::Procedural => "procedural",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Category {
    /// A bug / crash / regression that was observed.
    BugHistory,
    /// An approach that was tried and did NOT work (a dead end — save the user from re-trying it).
    FailedAttempt,
    /// An approach / fix that DID work (a reusable success pattern).
    SuccessPattern,
    /// An architecture / design decision or project convention.
    ArchDecision,
    /// A concrete command / invocation the project uses.
    Command,
    /// A security / secrets-handling rule or constraint.
    SecurityRule,
    /// A deployment / release / CI note.
    DeployNote,
    /// A codebase fact: where something lives, what a module/function does.
    Codebase,
    /// No clear content-type signal.
    #[default]
    None,
}

impl Category {
    pub fn as_str(self) -> &'static str {
        match self {
            Category::BugHistory => "bug-history",
            Category::FailedAttempt => "failed-attempt",
            Category::SuccessPattern => "success-pattern",
            Category::ArchDecision => "arch-decision",
            Category::Command => "command",
            Category::SecurityRule => "security-rule",
            Category::DeployNote => "deploy-note",
            Category::Codebase => "codebase",
            Category::None => "none",
        }
    }

    /// Parse a category name (for a `--category` flag / a stored tag). Unknown → None-the-variant.
    pub fn parse(s: &str) -> Option<Category> {
        match s.trim().to_lowercase().replace('_', "-").as_str() {
            "bug-history" | "bug" | "bughistory" => Some(Category::BugHistory),
            "failed-attempt" | "failure" | "deadend" | "dead-end" => Some(Category::FailedAttempt),
            "success-pattern" | "pattern" | "success" => Some(Category::SuccessPattern),
            "arch-decision" | "decision" | "architecture" => Some(Category::ArchDecision),
            "command" | "cmd" => Some(Category::Command),
            "security-rule" | "security" => Some(Category::SecurityRule),
            "deploy-note" | "deploy" | "deployment" => Some(Category::DeployNote),
            "codebase" | "code" => Some(Category::Codebase),
            "none" | "other" => Some(Category::None),
            _ => None,
        }
    }

    /// The neural-memory bucket this category rolls up to. `None` → episodic (an untyped observation
    /// is treated as a raw episode until it earns a sharper type).
    pub fn kind(self) -> Kind {
        match self {
            Category::BugHistory
            | Category::FailedAttempt
            | Category::SuccessPattern
            | Category::Command
            | Category::None => Kind::Episodic,
            Category::ArchDecision | Category::Codebase => Kind::Semantic,
            Category::SecurityRule | Category::DeployNote => Kind::Procedural,
        }
    }
}

// Single-word keywords match against the text's word set; multi-word / symbol-bearing keywords
// (have a space) match as a lowercased substring. Bilingual EN + VI throughout.
const BUG_KW: &[&str] = &[
    "bug", "crash", "crashed", "panic", "panicked", "regression", "broke", "broken", "error",
    "exception", "traceback", "stacktrace", "segfault", "npe", "nullpointer", "fails", "failing",
    "lỗi", "sập", "hỏng", "vỡ", "báo lỗi",
];
const FAILED_KW: &[&str] = &[
    "didn't work", "did not work", "does not work", "doesn't work", "no luck", "dead end",
    "gave up", "abandoned", "reverted", "rolled back", "not the way", "wrong approach", "avoid",
    "không hoạt động", "không chạy", "thất bại", "bỏ cuộc", "quay lại", "cách sai", "tránh",
];
const SUCCESS_KW: &[&str] = &[
    "worked", "works", "fixed", "solved", "resolved", "the fix", "the trick", "turned out",
    "solution was", "in the end", "finally", "pattern", "recipe", "approach that",
    "đã sửa", "đã fix", "giải quyết", "hoạt động rồi", "chạy rồi", "cách làm", "mẹo",
];
const DECISION_KW: &[&str] = &[
    "decided", "decision", "we chose", "chose to", "convention", "architecture", "design",
    "standard", "policy", "rule of thumb", "always structure", "the pattern is", "we use",
    "quyết định", "kiến trúc", "quy ước", "chuẩn", "nguyên tắc", "thiết kế", "chọn dùng",
];
const COMMAND_KW: &[&str] = &[
    "command", "run:", "run ", "cargo ", "npm run", "pnpm ", "make ", "docker ", "git ", "invoke",
    "cli", "flag", "argument", "script", "./", "npx ", "bash ", "powershell ",
    "lệnh", "chạy lệnh", "câu lệnh", "tham số",
];
const SECURITY_KW: &[&str] = &[
    "security", "secret", "secrets", "credential", "credentials", "token", "api key", "password",
    "auth", "authentication", "authorization", "permission", "vulnerability", "cve", "encrypt",
    "never commit", "never print", "sanitize", "injection",
    "bảo mật", "mật khẩu", "bí mật", "khoá", "mã hoá", "không được commit", "không in",
];
const DEPLOY_KW: &[&str] = &[
    "deploy", "deployment", "release", "ci", "cd", "pipeline", "publish", "ship", "rollout",
    "staging", "production", "prod", "docker build", "kubernetes", "k8s", "fly", "vercel",
    "netlify", "github actions", "runner",
    "triển khai", "phát hành", "lên prod", "đóng gói",
];
const CODEBASE_KW: &[&str] = &[
    "lives in", "defined in", "located in", "the module", "the function", "the file", "the struct",
    "the enum", "the class", "implemented in", "handles", "responsible for", "entry point",
    "nằm ở", "nằm trong", "định nghĩa ở", "hàm", "module", "tập tin", "cấu trúc", "xử lý",
];

fn count_hits(
    text_lower: &str,
    words: &std::collections::HashSet<String>,
    kws: &[&str],
) -> usize {
    kws.iter()
        .filter(|kw| {
            if kw.contains(' ') || kw.contains('/') || kw.contains(':') {
                text_lower.contains(*kw)
            } else {
                words.contains(**kw)
            }
        })
        .count()
}

/// Classify a fact's text into a content category. Argmax over keyword hits; ties resolve in a
/// fixed priority (the most decision-critical types win, so a fact that mentions both a `bug` and
/// the `command` that reproduced it is filed as `bug-history`). No hits → `None`.
pub fn classify(text: &str) -> Category {
    let lower = text.to_lowercase();
    let words: std::collections::HashSet<String> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    // (count, priority-rank, category) — higher count wins; tie → lower rank wins.
    let candidates = [
        (count_hits(&lower, &words, SECURITY_KW), 0, Category::SecurityRule),
        (count_hits(&lower, &words, FAILED_KW), 1, Category::FailedAttempt),
        (count_hits(&lower, &words, BUG_KW), 2, Category::BugHistory),
        (count_hits(&lower, &words, SUCCESS_KW), 3, Category::SuccessPattern),
        (count_hits(&lower, &words, DEPLOY_KW), 4, Category::DeployNote),
        (count_hits(&lower, &words, DECISION_KW), 5, Category::ArchDecision),
        (count_hits(&lower, &words, COMMAND_KW), 6, Category::Command),
        (count_hits(&lower, &words, CODEBASE_KW), 7, Category::Codebase),
    ];
    let best = candidates
        .iter()
        .filter(|(c, _, _)| *c > 0)
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    match best {
        Some((_, _, cat)) => *cat,
        None => Category::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_the_core_categories_en() {
        assert_eq!(classify("hit a null pointer panic in the parser"), Category::BugHistory);
        assert_eq!(classify("tried the recursive approach but it didn't work, dead end"), Category::FailedAttempt);
        assert_eq!(classify("the fix was to flush the buffer — that worked"), Category::SuccessPattern);
        assert_eq!(classify("we decided the architecture uses one store per zone by convention"), Category::ArchDecision);
        assert_eq!(classify("never commit the api key; sanitize before logging"), Category::SecurityRule);
        assert_eq!(classify("deploy to production via the github actions pipeline"), Category::DeployNote);
    }

    #[test]
    fn classifies_the_core_categories_vi() {
        assert_eq!(classify("gặp lỗi sập chương trình khi parse"), Category::BugHistory);
        assert_eq!(classify("thử cách đệ quy nhưng không hoạt động, cách sai"), Category::FailedAttempt);
        assert_eq!(classify("không được commit mật khẩu, phải mã hoá bí mật"), Category::SecurityRule);
        assert_eq!(classify("triển khai lên prod qua pipeline"), Category::DeployNote);
    }

    #[test]
    fn no_signal_is_none() {
        assert_eq!(classify("the meeting is on friday"), Category::None);
        assert_eq!(classify(""), Category::None);
    }

    #[test]
    fn priority_files_the_most_critical_type_on_a_tie() {
        // A line that names both a security rule and a command must file as security (rank 0),
        // not command (rank 6) — the decision-critical axis wins a 1-1 tie.
        assert_eq!(classify("run the command that rotates the secret token"), Category::SecurityRule);
    }

    #[test]
    fn kind_rollup_matches_the_neural_model() {
        assert_eq!(Category::BugHistory.kind(), Kind::Episodic);
        assert_eq!(Category::FailedAttempt.kind(), Kind::Episodic);
        assert_eq!(Category::Command.kind(), Kind::Episodic);
        assert_eq!(Category::ArchDecision.kind(), Kind::Semantic);
        assert_eq!(Category::Codebase.kind(), Kind::Semantic);
        assert_eq!(Category::SecurityRule.kind(), Kind::Procedural);
        assert_eq!(Category::DeployNote.kind(), Kind::Procedural);
        assert_eq!(Category::None.kind(), Kind::Episodic, "an untyped observation is a raw episode");
    }

    #[test]
    fn parse_roundtrip() {
        for c in [
            Category::BugHistory,
            Category::FailedAttempt,
            Category::SuccessPattern,
            Category::ArchDecision,
            Category::Command,
            Category::SecurityRule,
            Category::DeployNote,
            Category::Codebase,
            Category::None,
        ] {
            assert_eq!(Category::parse(c.as_str()), Some(c), "{} must round-trip", c.as_str());
        }
        assert_eq!(Category::parse("nonsense"), None);
    }
}
