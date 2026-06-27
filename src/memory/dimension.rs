//! Topical classification of a memory fact (phase B1).
//!
//! A small ORTHOGONAL set of topical dimensions for a coding-CLI user model. Kept topical
//! (not "constraint/frustration") so dimensions don't overlap — the negative/avoid facet is
//! already carried by `Polarity` in the learning pipeline. The B2 profile rollup groups by
//! these; B1 uses them for dimension-scoped retrieval.
//!
//! DERIVED, not stored: `classify` runs on load (cheap lexicon scan), so the tag never goes
//! stale when the lexicon evolves. Bilingual (EN + VI) — the user is Vietnamese.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dimension {
    /// Language / tone / verbosity (D1).
    Style,
    /// Package manager / formatter / shell / vcs / build tool choices (D4).
    Tooling,
    /// Process preferences: tests, commits, plan-first, risk/autonomy (D2/D3).
    Workflow,
    /// Languages / frameworks / datastores (D5).
    Stack,
    /// No clear topical signal.
    #[default]
    Other,
}

impl Dimension {
    pub fn as_str(self) -> &'static str {
        match self {
            Dimension::Style => "style",
            Dimension::Tooling => "tooling",
            Dimension::Workflow => "workflow",
            Dimension::Stack => "stack",
            Dimension::Other => "other",
        }
    }

    /// Parse a dimension name (for the `--dimension` flag / a stored tag). Unknown → None.
    pub fn parse(s: &str) -> Option<Dimension> {
        match s.trim().to_lowercase().as_str() {
            "style" => Some(Dimension::Style),
            "tooling" | "tool" | "tools" => Some(Dimension::Tooling),
            "workflow" | "process" => Some(Dimension::Workflow),
            "stack" => Some(Dimension::Stack),
            "other" => Some(Dimension::Other),
            _ => None,
        }
    }
}

// Single-word keywords are matched against the text's word set; multi-word / symbol-bearing
// keywords (have a space) are matched as a lowercased substring.
const STYLE_KW: &[&str] = &[
    "reply", "replies", "respond", "response", "answer", "language", "vietnamese", "english",
    "tiếng việt", "tiếng anh", "concise", "terse", "brief", "succinct", "verbose", "tone",
    "ngắn gọn", "súc tích", "explanation", "wording", "formatting",
];
const TOOLING_KW: &[&str] = &[
    "pnpm", "npm", "yarn", "bun", "cargo", "pip", "poetry", "uv", "git", "prettier", "eslint",
    "rustfmt", "clippy", "black", "ruff", "make", "docker", "vite", "webpack", "esbuild", "bash",
    "zsh", "fish", "powershell", "pwsh", "vim", "neovim", "nvim", "vscode", "tabs", "spaces",
    "formatter", "linter", "package manager",
];
const WORKFLOW_KW: &[&str] = &[
    "test", "tests", "testing", "commit", "commits", "deploy", "push", "build", "plan", "review",
    "pipeline", "merge", "rebase", "branch", "lint", "typecheck", "verify", "confirm", "approve",
    "autonomous", "just do it", "run the tests", "ci",
];
const STACK_KW: &[&str] = &[
    "rust", "typescript", "javascript", "python", "react", "next", "nextjs", "vue", "svelte",
    "dotnet", ".net", "csharp", "c#", "golang", "kotlin", "node", "nodejs", "postgres",
    "postgresql", "redis", "valkey", "tailwind", "fastapi", "django", "flask", "express", "axum",
    "tokio", "java", "go",
];

fn count_hits(text_lower: &str, words: &std::collections::HashSet<String>, kws: &[&str]) -> usize {
    kws.iter()
        .filter(|kw| {
            if kw.contains(' ') {
                text_lower.contains(*kw)
            } else {
                words.contains(**kw)
            }
        })
        .count()
}

/// Classify a fact's text into a topical dimension. Argmax over keyword hits; ties resolve in
/// the fixed priority Style > Tooling > Stack > Workflow; no hits → Other.
pub fn classify(text: &str) -> Dimension {
    let lower = text.to_lowercase();
    let words: std::collections::HashSet<String> = lower
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .collect();

    let style = count_hits(&lower, &words, STYLE_KW);
    let tooling = count_hits(&lower, &words, TOOLING_KW);
    let workflow = count_hits(&lower, &words, WORKFLOW_KW);
    let stack = count_hits(&lower, &words, STACK_KW);

    // (count, priority-rank) — higher count wins; tie → lower rank wins.
    let candidates = [
        (style, 0, Dimension::Style),
        (tooling, 1, Dimension::Tooling),
        (stack, 2, Dimension::Stack),
        (workflow, 3, Dimension::Workflow),
    ];
    let best = candidates
        .iter()
        .filter(|(c, _, _)| *c > 0)
        .max_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    match best {
        Some((_, _, dim)) => *dim,
        None => Dimension::Other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_style() {
        assert_eq!(classify("reply in Vietnamese"), Dimension::Style);
        assert_eq!(classify("keep replies concise and terse"), Dimension::Style);
        assert_eq!(classify("trả lời ngắn gọn"), Dimension::Style);
    }

    #[test]
    fn classifies_tooling() {
        assert_eq!(classify("prefer pnpm over npm"), Dimension::Tooling);
        assert_eq!(classify("use tabs instead of spaces"), Dimension::Tooling);
        assert_eq!(classify("format with prettier"), Dimension::Tooling);
    }

    #[test]
    fn classifies_workflow() {
        assert_eq!(classify("always run the tests before commit"), Dimension::Workflow);
        assert_eq!(classify("ask before you deploy"), Dimension::Workflow);
    }

    #[test]
    fn classifies_stack() {
        assert_eq!(classify("the backend is rust with tokio and axum"), Dimension::Stack);
        assert_eq!(classify("we use react and typescript"), Dimension::Stack);
    }

    #[test]
    fn no_signal_is_other() {
        assert_eq!(classify("the meeting is on friday"), Dimension::Other);
        assert_eq!(classify(""), Dimension::Other);
    }

    #[test]
    fn parse_roundtrip() {
        for d in [Dimension::Style, Dimension::Tooling, Dimension::Workflow, Dimension::Stack, Dimension::Other] {
            assert_eq!(Dimension::parse(d.as_str()), Some(d));
        }
        assert_eq!(Dimension::parse("nonsense"), None);
    }

    #[test]
    fn word_boundary_not_substring() {
        // "go" must not fire on "going"/"google"; it's matched as a whole word.
        assert_eq!(classify("I am going to the store"), Dimension::Other);
    }
}
