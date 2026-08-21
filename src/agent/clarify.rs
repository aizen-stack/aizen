//! `clarify` — the ask-then-YIELD tool: when a task is genuinely ambiguous and a wrong guess
//! would waste real work, the model poses ONE focused question and the turn PAUSES so the user
//! answers in their next message.
//!
//! Why a yield (not a blocking stdin read): under the sticky TUI a background thread owns stdin,
//! so a tool that `read_line`s would fight it (and deadlock / eat keystrokes); under `aizen serve`
//! there is no terminal at all. So instead of READING input, the tool RECORDS the question in a
//! process-global cell and the agent loop, on seeing it, stops with `StopReason::AwaitingInput`.
//! Whatever input mechanism is already in play — the sticky input box, the plain REPL readline, or
//! a Telegram message — then supplies the answer as the next user turn, re-entering the same
//! conversation. One mechanism, every surface, zero stdin contention.
//!
//! Distinct from its neighbours (the repo's anti-overlap discipline): `telegram_ask` is
//! approve/deny over inline buttons for UNATTENDED runs; `memory_ask` recalls what the user
//! ALREADY told us. `clarify` is interactive free-text disambiguation that blocks forward progress.

use crate::agent::tools::Tool;
use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::sync::Mutex;

/// The tool's advertised name — also the key the agent loop uses to decide whether to consult the
/// pending cell this turn (so a turn that never called `clarify` can't drain a stale value).
pub const NAME: &str = "clarify";

/// The single outstanding question, set by `Clarify::execute` and drained by the agent loop
/// (`take_pending`) the same turn. `None` whenever no clarification is in flight. A turn that
/// contains `clarify` runs serially (it is not concurrency-safe), so there is never a race here.
static PENDING: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));

/// Take (and clear) the pending clarification, if any. The agent loop calls this after executing a
/// turn's tool calls; `Some` means "a clarify fired this turn → stop and yield to the user".
pub fn take_pending() -> Option<String> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).take()
}

fn set_pending(display: String) {
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(display);
}

/// Serializes every test (here AND in the agent-loop module) that touches the process-global
/// `PENDING` — cargo runs tests in parallel, so without this a concurrent set/take would interleave.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

/// Render the user-facing display (the prominent question + any numbered options) and the
/// model-facing acknowledgement (the "stop and wait" instruction). Pure — no global state — so the
/// tests can exercise it without touching `PENDING`.
fn build(question: &str, options: &[String]) -> (String, String) {
    let q = question.trim();
    let mut display = q.to_string();
    for (i, o) in options.iter().enumerate() {
        display.push_str(&format!("\n  {}. {}", i + 1, o));
    }
    let opt_hint = if options.is_empty() {
        String::new()
    } else {
        format!(" Suggested answers: {}.", options.join(" / "))
    };
    let ack = format!(
        "Question posed to the user: \"{q}\".{opt_hint} STOP now — do NOT call clarify again or \
         answer on their behalf; their reply arrives as the next user message and you continue \
         from there."
    );
    (display, ack)
}

/// Inverse of [`build`]'s display half: split a stored display back into the question and its
/// numbered options, so the sticky REPL can raise a picker over the input box. Lives HERE, next to
/// `build`, because the two must agree on the format — the round-trip test below pins them
/// together. A display that doesn't match the shape `build` emits (a hand-written AwaitingInput
/// string, a multi-line question) yields no options, which safely degrades to the free-text path.
pub fn parse_display(display: &str) -> (&str, Vec<String>) {
    let mut lines = display.lines();
    let q = lines.next().unwrap_or("").trim();
    let mut opts = Vec::new();
    for l in lines {
        let t = l.trim_start();
        let numbered = t
            .split_once(". ")
            .filter(|(n, rest)| {
                !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && !rest.trim().is_empty()
            })
            .map(|(_, rest)| rest.trim().to_string());
        match numbered {
            Some(o) => opts.push(o),
            // Any non-option line means this is NOT build()'s shape — don't guess.
            None => return (q, Vec::new()),
        }
    }
    (q, opts)
}

pub struct Clarify;

impl Tool for Clarify {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "Ask the user ONE focused question when the task is genuinely ambiguous and you cannot \
         proceed safely without their answer (which of two files, which framework, confirm a risky \
         direction). The turn PAUSES and the user's next message is the answer — so ask only when a \
         wrong guess would waste real work; otherwise make a reasonable assumption, state it, and \
         continue. Not for approve/deny of a command when running unattended → use telegram_ask; \
         not for recalling what the user already told you → use memory_search / memory_ask."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {"type": "string", "description": "the single, specific question to ask"},
                "options": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "optional short suggested answers, shown to the user as a numbered list"
                }
            },
            "required": ["question"],
            "additionalProperties": false
        })
    }

    /// Asking changes nothing on disk and needs no approval.
    fn is_destructive(&self) -> bool {
        false
    }

    /// Control-flow tool with a process-global side effect → must run serially, never in a
    /// parallel batch.
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn execute(&self, args: &Value) -> Result<String> {
        let question = args
            .get("question")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("missing required non-empty string arg 'question'")?;
        let options: Vec<String> = args
            .get("options")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect()
            })
            .unwrap_or_default();
        let (display, ack) = build(question, &options);
        set_pending(display);
        Ok(ack)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_renders_question_and_numbered_options() {
        let (display, ack) = build(
            "Which file?",
            &["src/a.rs".to_string(), "src/b.rs".to_string()],
        );
        assert_eq!(display, "Which file?\n  1. src/a.rs\n  2. src/b.rs");
        assert!(ack.contains("Which file?"));
        assert!(ack.contains("Suggested answers: src/a.rs / src/b.rs."));
        assert!(
            ack.contains("STOP now"),
            "the model must be told to wait, not re-ask: {ack}"
        );
    }

    #[test]
    fn parse_display_roundtrips_build() {
        let opts = vec!["src/a.rs".to_string(), "option 2. with dots".to_string()];
        let (display, _) = build("Which file?", &opts);
        let (q, parsed) = parse_display(&display);
        assert_eq!(q, "Which file?");
        assert_eq!(parsed, opts, "options must survive the round-trip verbatim");
        // No options → none parsed.
        let (display, _) = build("Proceed?", &[]);
        assert_eq!(parse_display(&display), ("Proceed?", Vec::new()));
    }

    #[test]
    fn parse_display_refuses_foreign_shapes() {
        // A multi-line question that never came from build() must not be misread as options.
        let (q, opts) = parse_display("What now?\nsome free-form second line");
        assert_eq!(q, "What now?");
        assert!(opts.is_empty(), "non-numbered line → no menu: {opts:?}");
        // A numbered line followed by a stray line → also refuse (all-or-nothing).
        let (_, opts) = parse_display("Q?\n  1. yes\ntrailing prose");
        assert!(opts.is_empty());
    }

    #[test]
    fn build_without_options_is_just_the_question() {
        let (display, ack) = build("  Proceed?  ", &[]);
        assert_eq!(
            display, "Proceed?",
            "whitespace trimmed, no options appended"
        );
        assert!(!ack.contains("Suggested answers"));
    }

    #[test]
    fn execute_sets_pending_and_take_drains_it() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = take_pending(); // clear any leftover from another test
        let ack = Clarify
            .execute(&json!({"question": "A or B?", "options": ["A", "B"]}))
            .unwrap();
        assert!(ack.contains("A or B?"));
        let pending = take_pending().expect("a question must be pending after execute");
        assert!(pending.starts_with("A or B?"));
        assert!(pending.contains("1. A") && pending.contains("2. B"));
        assert!(
            take_pending().is_none(),
            "take must drain — a second take yields nothing"
        );
    }

    #[test]
    fn execute_rejects_empty_or_missing_question() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(Clarify.execute(&json!({})).is_err(), "missing question");
        assert!(
            Clarify.execute(&json!({"question": "   "})).is_err(),
            "blank question"
        );
        let _ = take_pending();
    }

    #[test]
    fn flags_are_nondestructive_and_serial() {
        assert!(
            !Clarify.is_destructive(),
            "asking a question changes nothing"
        );
        assert!(
            !Clarify.is_concurrency_safe(),
            "global side effect → must run serially"
        );
    }
}
