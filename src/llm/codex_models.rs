//! Static ChatGPT Codex model catalog (experimental).
//!
//! The Codex backend does not expose a stable OpenAI-style GET /models list for free-tier
//! discovery the way Platform API does, so the picker uses this curated catalog. Update when
//! upstream renames ids (comment the as-of date in CHANGELOG).

/// (id, short label)
pub const CODEX_MODELS: &[(&str, &str)] = &[
    ("gpt-5.6-sol", "GPT 5.6 Sol"),
    ("gpt-5.6-terra", "GPT 5.6 Terra"),
    ("gpt-5.6-luna", "GPT 5.6 Luna"),
    ("gpt-5.5", "GPT 5.5"),
    ("gpt-5.4", "GPT 5.4"),
    ("gpt-5.4-mini", "GPT 5.4 Mini"),
    ("gpt-5.3-codex-spark", "GPT 5.3 Codex Spark"),
    ("gpt-5.3-codex", "GPT 5.3 Codex"),
    ("gpt-5.2", "GPT 5.2"),
    ("gpt-5.1", "GPT 5.1"),
    ("gpt-5", "GPT 5"),
];

pub fn default_model() -> &'static str {
    "gpt-5.4-mini"
}

#[allow(dead_code)]
pub fn is_known(id: &str) -> bool {
    let base = strip_effort_suffix(id).0;
    CODEX_MODELS.iter().any(|(m, _)| *m == base)
}

/// Strip a trailing reasoning-effort suffix: `model-high` → (`model`, Some("high")).
pub fn strip_effort_suffix(id: &str) -> (&str, Option<&str>) {
    const LEVELS: &[&str] = &["none", "minimal", "low", "medium", "high", "xhigh", "max"];
    for level in LEVELS {
        let suffix = format!("-{level}");
        if let Some(base) = id.strip_suffix(&suffix) {
            if !base.is_empty() {
                let effort = if *level == "max" { "xhigh" } else { *level };
                return (base, Some(effort));
            }
        }
    }
    (id, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn effort_suffix_parse() {
        assert_eq!(
            strip_effort_suffix("gpt-5.4-high"),
            ("gpt-5.4", Some("high"))
        );
        assert_eq!(
            strip_effort_suffix("gpt-5.4-max"),
            ("gpt-5.4", Some("xhigh"))
        );
        assert_eq!(strip_effort_suffix("gpt-5.4"), ("gpt-5.4", None));
        assert!(is_known("gpt-5.4-mini"));
        assert!(is_known("gpt-5.4-mini-high"));
    }
}
