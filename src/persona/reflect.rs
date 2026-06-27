//! Persona reflection (P2) — the "grows like a human" engine.
//!
//! Generative-Agents reflection: when a character has accumulated enough formative experience (see
//! `self_mem::should_reflect`), it steps back and synthesizes a few **higher-level insights** from
//! its recent episodes — observations about the user, about itself, about the relationship — and
//! stores them as durable `insight` self-memories that thereafter shape its `<self>` block.
//!
//! This module is PURE (prompt construction + reply parsing) so it is unit-testable with no
//! network. The actual model call + persistence is orchestrated by the REPL (`maybe_evolve_persona`
//! in `main.rs`), mirroring how `maybe_learn_skill` keeps its one cheap extraction call there.

/// A reflected insight: a first-person, higher-level observation + its importance [0..=10].
#[derive(Debug, Clone, PartialEq)]
pub struct Insight {
    pub text: String,
    pub importance: u8,
}

/// Build the `(system, user)` reflection prompt for `persona_name`/`role` over `episodes`
/// (chronological). The model is asked to return strict JSON; the body is intentionally compact.
pub fn build_reflection_prompt(persona_name: &str, role: &str, episodes: &[String]) -> (String, String) {
    let who = if role.trim().is_empty() {
        persona_name.to_string()
    } else {
        format!("{persona_name}, {}", role.trim())
    };
    let system = format!(
        "You are {who}. Step back and REFLECT on your recent experiences to grow as this character. \
         From the episodes below, synthesize 1-3 higher-level INSIGHTS — durable, first-person \
         observations about the user, about yourself, or about your working relationship that will \
         make you wiser and more in-character next time. Be specific and grounded in the episodes; \
         do NOT invent facts. Prefer insights that generalize over one-off details. Reply with ONLY \
         a JSON object: {{\"insights\":[{{\"text\":\"first-person insight\",\"importance\":0-10}}]}}. \
         If nothing meaningful generalizes, reply {{\"insights\":[]}}."
    );
    let joined = episodes
        .iter()
        .enumerate()
        .map(|(i, e)| format!("{}. {}", i + 1, e.trim()))
        .collect::<Vec<_>>()
        .join("\n");
    let user = format!("Recent episodes (oldest first):\n{joined}");
    (system, user)
}

/// Parse the reflection reply's JSON object (already extracted from any prose/fences) into insights.
/// Tolerant: drops malformed/empty entries, clamps importance, caps at 3, dedups by normalized text.
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
        let text = item.get("text").and_then(|t| t.as_str()).unwrap_or("").trim().to_string();
        if text.is_empty() {
            continue;
        }
        let norm = text.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase();
        if seen.contains(&norm) {
            continue;
        }
        let importance = item
            .get("importance")
            .and_then(|n| n.as_u64())
            .unwrap_or(6)
            .min(10) as u8;
        seen.push(norm);
        out.push(Insight { text, importance });
        if out.len() >= 3 {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_includes_role_and_numbered_episodes() {
        let (sys, usr) = build_reflection_prompt("Aria", "a mentor", &["did x".into(), "did y".into()]);
        assert!(sys.contains("You are Aria, a mentor."));
        assert!(sys.contains("insights"));
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
    }

    #[test]
    fn parse_tolerates_garbage_and_empty() {
        assert!(parse_insights("not json").is_empty());
        assert!(parse_insights(r#"{"insights":[]}"#).is_empty());
        assert!(parse_insights(r#"{"other":1}"#).is_empty());
    }
}
