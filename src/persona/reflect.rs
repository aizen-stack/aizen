//! Persona reflection — episodic → semantic distillation (Generative Agents + CoALA).
//!
//! When a character has accumulated enough *formative* experience (`self_mem::should_reflect`), it
//! synthesizes a few higher-level **insights** about the user relationship / its own working style —
//! never coding trivia or raw transcript restatements. Insights become the durable always-on
//! `<self>` layer; episodes remain the substrate.
//!
//! This module is PURE (prompt construction + reply parsing) so it is unit-testable with no
//! network. The actual model call + persistence is orchestrated by the REPL (`maybe_evolve_persona`
//! in `main.rs`).

/// A reflected insight: a first-person, higher-level observation + its importance [0..=10].
#[derive(Debug, Clone, PartialEq)]
pub struct Insight {
    pub text: String,
    pub importance: u8,
}

/// Build the `(system, user)` reflection prompt for `persona_name`/`role` over formative `episodes`
/// (chronological). The model is asked to return strict JSON; the body is intentionally compact.
pub fn build_reflection_prompt(
    persona_name: &str,
    role: &str,
    episodes: &[String],
) -> (String, String) {
    let who = if role.trim().is_empty() {
        persona_name.to_string()
    } else {
        format!("{persona_name}, {}", role.trim())
    };
    let system = format!(
        "You are {who}. Step back and REFLECT on your recent formative experiences to grow as this \
         character. From the typed episodes below (correction / preference / work / bond), synthesize \
         1-3 higher-level INSIGHTS — durable, first-person observations about:\n\
         - the USER's working style with you (language, tone, autonomy, tools they care about),\n\
         - YOUR relationship / how you should show up for them,\n\
         - boundaries or patterns that keep repeating.\n\
         RULES:\n\
         - Be specific and grounded in the episodes; do NOT invent facts.\n\
         - Prefer generalizations that will still be true next week over one-off task details.\n\
         - NEVER write insights about a specific bug, file, commit, or coding task — those belong in \
         project memory, not character memory.\n\
         - NEVER restate a raw episode verbatim; distill.\n\
         - If nothing meaningful about the relationship/character generalizes, reply {{\"insights\":[]}}.\n\
         Reply with ONLY a JSON object: \
         {{\"insights\":[{{\"text\":\"first-person insight\",\"importance\":0-10}}]}}."
    );
    let joined = episodes
        .iter()
        .enumerate()
        .map(|(i, e)| format!("{}. {}", i + 1, e.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    let user = format!("Recent formative episodes (oldest first):\n{joined}");
    (system, user)
}

/// Parse the reflection reply's JSON object (already extracted from any prose/fences) into insights.
/// Tolerant: drops malformed/empty entries, clamps importance, caps at 3, dedups by normalized text,
/// and rejects insights that are just coding-task trivia or raw episode echoes.
pub fn parse_insights(json: &str) -> Vec<Insight> {
    let v: serde_json::Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let arr = match v.get("insights").and_then(|x| x.as_array()) {
        Some(a) => a,
        None => return Vec::new(),
    };
    let mut out: Vec<Insight> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for item in arr {
        let text = item
            .get("text")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if text.is_empty() {
            continue;
        }
        if looks_like_task_trivia(&text) {
            continue;
        }
        let norm = text
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase();
        if seen.contains(&norm) {
            continue;
        }
        let importance = item
            .get("importance")
            .and_then(|n| n.as_u64())
            .unwrap_or(6)
            .min(10) as u8;
        // Floor: reflected insights should be at least moderately important.
        let importance = importance.max(5);
        seen.push(norm);
        out.push(Insight { text, importance });
        if out.len() >= 3 {
            break;
        }
    }
    out
}

/// Reject insights that are clearly about a one-off coding task (project memory, not character).
fn looks_like_task_trivia(text: &str) -> bool {
    let t = text.to_lowercase();
    const MARKERS: &[&str] = &[
        ".rs",
        ".ts",
        ".js",
        ".py",
        ".go",
        ".tsx",
        ".jsx",
        "cargo ",
        "npm ",
        "git commit",
        "pull request",
        "stack trace",
        "compile error",
        "type error",
        "line ",
        "fn ",
        "function ",
        "bug in",
        "fixed the",
        "patched ",
        "src/",
    ];
    // Only reject when it looks *dominantly* like task trivia AND lacks relationship language.
    let hit = MARKERS.iter().any(|m| t.contains(m));
    if !hit {
        return false;
    }
    const REL: &[&str] = &[
        "user",
        "prefer",
        "style",
        "tone",
        "language",
        "relationship",
        "trust",
        "always",
        "never",
        "with me",
        "when we",
        "they like",
        "they want",
        "tôi",
        "bạn",
        "anh",
        "em",
    ];
    !REL.iter().any(|r| t.contains(r))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_role_and_numbered_episodes() {
        let (sys, usr) =
            build_reflection_prompt("Aria", "a mentor", &["did x".into(), "did y".into()]);
        assert!(sys.contains("You are Aria, a mentor."));
        assert!(sys.contains("insights"));
        assert!(sys.contains("NEVER write insights about a specific bug"));
        assert!(usr.contains("1. did x") && usr.contains("2. did y"));
    }

    #[test]
    fn prompt_handles_empty_role() {
        let (sys, _) = build_reflection_prompt("Aria", "  ", &["e".into()]);
        assert!(sys.contains("You are Aria."));
    }

    #[test]
    fn parse_clamps_caps_and_dedups() {
        let json = r#"{"insights":[
            {"text":"the user likes terse replies","importance":12},
            {"text":"The user   likes  terse replies","importance":5},
            {"text":"prefers vietnamese","importance":7},
            {"text":"","importance":9},
            {"text":"a fourth one","importance":4},
            {"text":"a fifth one","importance":4}
        ]}"#;
        let got = parse_insights(json);
        assert_eq!(got.len(), 3, "deduped + capped at 3");
        assert_eq!(got[0].importance, 10, "12 clamped to 10");
        assert_eq!(got[0].text, "the user likes terse replies");
        assert_eq!(got[1].text, "prefers vietnamese");
        assert!(got[2].importance >= 5, "floor applied");
    }

    #[test]
    fn parse_drops_task_trivia() {
        let json = r#"{"insights":[
            {"text":"fixed the bug in src/config.rs line 40","importance":8},
            {"text":"the user wants terse vietnamese replies","importance":7}
        ]}"#;
        let got = parse_insights(json);
        assert_eq!(got.len(), 1);
        assert!(got[0].text.contains("terse vietnamese"));
    }

    #[test]
    fn parse_tolerates_garbage_and_empty() {
        assert!(parse_insights("not json").is_empty());
        assert!(parse_insights(r#"{"insights":[]}"#).is_empty());
        assert!(parse_insights(r#"{"other":1}"#).is_empty());
    }
}
