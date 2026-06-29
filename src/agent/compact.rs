//! Conversation compaction — the shared core used both by the interactive REPL (between turns) and
//! by the agent loop (mid-task, for multi-turn callers like `ng serve`). Older turns are summarized
//! into one dense `system` note; the system prompt and the last [`KEEP_TURNS`] user turns are kept
//! verbatim. The cut is always a `user` boundary so the summarized block never ends mid-turn and the
//! kept tail never begins with an orphan `tool` result (a dangling tool message 400s on strict
//! gateways).
//!
//! The model call is INJECTED as a `summarize` closure rather than hardcoded, so the same core works
//! for any endpoint, can be driven non-streaming/quiet from the loop, and is unit-testable with a
//! fake summarizer.

use crate::core::types::Message;
use anyhow::{anyhow, Result};
use std::future::Future;

/// User turns kept verbatim at the tail; everything older is summarized.
pub const KEEP_TURNS: usize = 3;

/// The summarization instruction. Centralized here so the REPL and the loop produce identical
/// summaries. Mirrors the original `compact_history` prompt.
const SUMMARIZE_SYS: &str = "You compress a coding-assistant conversation to conserve context. \
    Write a DENSE summary that preserves: the user's goals and explicit requests, decisions made, \
    files/paths touched, commands run and their outcomes, important code/config, and any OPEN or \
    UNFINISHED tasks. Use terse bullet points. Do NOT invent anything not in the transcript.";

/// Truncate to `max` chars with a `…[+N chars]` marker (char-safe, never splits a codepoint).
pub fn truncate_chars(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        format!("{}… [+{} chars]", chars[..max].iter().collect::<String>(), chars.len() - max)
    }
}

/// Render conversation messages into a compact transcript for summarization. Tool payloads are
/// truncated so the summary call stays cheap.
pub fn render_transcript(msgs: &[Message]) -> String {
    let mut out = String::new();
    for m in msgs {
        let body = m.content.as_deref().unwrap_or("").trim();
        match m.role.as_str() {
            "user" => out.push_str(&format!("User: {body}\n")),
            "assistant" if !m.tool_calls.is_empty() => {
                if !body.is_empty() {
                    out.push_str(&format!("Assistant: {body}\n"));
                }
                let calls: Vec<String> = m
                    .tool_calls
                    .iter()
                    .map(|c| format!("{}({})", c.function.name, truncate_chars(&c.function.arguments, 160)))
                    .collect();
                out.push_str(&format!("Assistant→tools: {}\n", calls.join(", ")));
            }
            "assistant" => out.push_str(&format!("Assistant: {body}\n")),
            "tool" => out.push_str(&format!("Tool result: {}\n", truncate_chars(body, 600))),
            "system" => out.push_str(&format!("Note: {}\n", truncate_chars(body, 600))),
            other => out.push_str(&format!("{other}: {body}\n")),
        }
    }
    out
}

/// Compute the compaction cut index: the kept tail starts here. It's always a `user` message, so the
/// summarized block never ends mid-turn and the tail never begins with an orphan `tool` result.
/// Keeps the last `keep_turns` user turns verbatim. `None` when the conversation is too short to be
/// worth compacting.
pub fn plan_compact_cut(history: &[Message], keep_turns: usize) -> Option<usize> {
    // User messages (skipping the system prompt at index 0) are the clean turn boundaries.
    let user_idxs: Vec<usize> =
        history.iter().enumerate().skip(1).filter(|(_, m)| m.role == "user").map(|(i, _)| i).collect();
    if user_idxs.len() < 2 {
        return None;
    }
    let keep = keep_turns.min(user_idxs.len() - 1).max(1);
    let cut = user_idxs[user_idxs.len() - keep];
    if cut <= 1 {
        None
    } else {
        Some(cut)
    }
}

/// Rebuild `history` in place as: the system prompt (`[0]`) + one summary `system` note + the
/// verbatim tail from `cut`. The summary should already be trimmed.
pub fn splice_compacted(history: &mut Vec<Message>, cut: usize, summary: &str) {
    let system_prompt = history[0].clone();
    let tail: Vec<Message> = history[cut..].to_vec();
    let mut rebuilt = Vec::with_capacity(2 + tail.len());
    rebuilt.push(system_prompt);
    rebuilt.push(Message::system(format!("[Earlier conversation auto-compacted to conserve context]\n{summary}")));
    rebuilt.extend(tail);
    *history = rebuilt;
}

/// Rough size in tokens (chars/4, no tokenizer dep) — for the before/after trace only.
fn approx_tokens(history: &[Message]) -> usize {
    history.iter().filter_map(|m| m.content.as_ref()).map(|c| c.chars().count()).sum::<usize>() / 4
}

/// Summarize older turns to free context. `summarize` runs the model call on a `[system, user]`
/// prompt and returns the summary text (injected so this works for any endpoint and stays
/// non-streaming + unit-testable). Keeps the system prompt + the last `keep_turns` user turns
/// verbatim; the block between is replaced by one summary `system` message. Returns
/// `(tokens_before, tokens_after)`.
pub async fn compact_history<S, Fut>(
    history: &mut Vec<Message>,
    summarize: S,
    keep_turns: usize,
) -> Result<(usize, usize)>
where
    S: Fn(Vec<Message>) -> Fut,
    Fut: Future<Output = Result<String>>,
{
    let before = approx_tokens(history);
    let cut = plan_compact_cut(history, keep_turns)
        .ok_or_else(|| anyhow!("conversation too short to compact (need at least 2 turns)"))?;
    let older = &history[1..cut];
    if older.is_empty() {
        anyhow::bail!("nothing older to compact");
    }
    let transcript = render_transcript(older);
    let prompt = vec![
        Message::system(SUMMARIZE_SYS),
        Message::user(format!("Conversation to summarize:\n\n{transcript}")),
    ];
    let summary = summarize(prompt).await.map_err(|e| anyhow!("summarization call failed: {e}"))?;
    if summary.trim().is_empty() {
        anyhow::bail!("the model returned an empty summary");
    }
    splice_compacted(history, cut, summary.trim());
    Ok((before, approx_tokens(history)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{FunctionCall, ToolCall};

    fn user(s: &str) -> Message {
        Message::user(s.to_string())
    }
    fn asst(s: &str) -> Message {
        Message::assistant(s.to_string())
    }

    #[test]
    fn truncate_chars_marks_overflow() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert!(truncate_chars("hello world", 5).starts_with("hello… [+"));
    }

    #[test]
    fn plan_cut_needs_two_turns_and_lands_on_user() {
        // too short
        assert_eq!(plan_compact_cut(&[Message::system("s")], KEEP_TURNS), None);
        assert_eq!(plan_compact_cut(&[Message::system("s"), user("u")], KEEP_TURNS), None);
        // 4 user turns, keep last 2 → cut at the 3rd user message; it's a `user` index.
        let h = vec![
            Message::system("s"),
            user("u1"), asst("a1"),
            user("u2"), asst("a2"),
            user("u3"), asst("a3"),
            user("u4"), asst("a4"),
        ];
        let cut = plan_compact_cut(&h, 2).expect("should compact");
        assert_eq!(h[cut].role, "user", "cut must land on a user boundary");
    }

    #[test]
    fn splice_keeps_system_summary_and_tail_without_orphan_tool() {
        // system, [u1, a1→tool, tool], [u2, a2] — cut at u2 so the tool pair is summarized whole.
        let mut h = vec![
            Message::system("SYS"),
            user("u1"),
            Message::assistant_tool_calls(vec![ToolCall {
                id: "1".into(),
                kind: "function".into(),
                function: FunctionCall { name: "echo".into(), arguments: "{}".into() },
            }]),
            Message::tool_result("1", "big result"),
            user("u2"),
            asst("a2"),
        ];
        let cut = 4; // index of u2
        splice_compacted(&mut h, cut, "DENSE_SUMMARY");
        assert_eq!(h[0].content.as_deref(), Some("SYS"), "system prompt preserved at [0]");
        assert!(h[1].content.as_deref().unwrap().contains("DENSE_SUMMARY"), "summary note inserted");
        assert_eq!(h[1].role, "system");
        assert_eq!(h[2].content.as_deref(), Some("u2"), "verbatim tail begins at the cut (a user msg)");
        // No orphan tool result survived the cut.
        assert!(!h[2..].iter().any(|m| m.role == "tool"), "no dangling tool result in the tail");
    }

    #[tokio::test]
    async fn compact_history_uses_injected_summarizer() {
        let mut h = vec![
            Message::system("SYS"),
            user("u1"), asst("a1"),
            user("u2"), asst("a2"),
            user("u3"), asst("a3"),
        ];
        let before_len = h.len();
        let (b, a) = compact_history(&mut h, |_msgs| async { Ok("SUMMARY_OK".to_string()) }, 1)
            .await
            .expect("compaction should succeed");
        assert!(h.len() < before_len, "history shrank: {} → {}", before_len, h.len());
        assert_eq!(h[0].content.as_deref(), Some("SYS"));
        assert!(h[1].content.as_deref().unwrap().contains("SUMMARY_OK"));
        assert!(b > 0 && a > 0);
    }

    #[tokio::test]
    async fn compact_history_errors_when_too_short() {
        let mut h = vec![Message::system("SYS"), user("only one turn")];
        let r = compact_history(&mut h, |_m| async { Ok("x".to_string()) }, KEEP_TURNS).await;
        assert!(r.is_err(), "a single-turn conversation can't be compacted");
    }

    #[tokio::test]
    async fn compact_history_rejects_empty_summary() {
        let mut h = vec![
            Message::system("SYS"),
            user("u1"), asst("a1"),
            user("u2"), asst("a2"),
        ];
        let r = compact_history(&mut h, |_m| async { Ok("   ".to_string()) }, 1).await;
        assert!(r.is_err(), "an empty summary is rejected (don't destroy history for nothing)");
    }
}
