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

/// Stable prefix identifying the compaction-boundary `system` note (P-ctx3). Analogous to Claude
/// Code's `subtype:"compact_boundary"` marker: everything before this note in history is a lossy
/// summary, everything after is verbatim. A stable prefix (not a bespoke `Message` field — the core
/// stays pure) makes the boundary QUERYABLE: the HUD counts compactions from it, and each successive
/// compaction reads the prior sequence number off it and increments, so the count survives even
/// though the old boundary note itself is summarized away into the new one.
pub const COMPACT_MARKER_PREFIX: &str = "[Earlier conversation auto-compacted";

/// The summarization instruction. Centralized here so the REPL and the loop produce identical
/// summaries. Mirrors the original `compact_history` prompt.
const SUMMARIZE_SYS: &str = "You compress a coding-assistant conversation to conserve context. \
    Write a DENSE summary that preserves: the user's goals and explicit requests, decisions made, \
    files/paths touched, commands run and their outcomes, important code/config, and any OPEN or \
    UNFINISHED tasks. Also preserve a REFLECTION section: approaches that were TRIED and FAILED and \
    WHY (errors hit, dead ends, things that did not work) — so the continuation does not repeat \
    them. Use terse bullet points. Do NOT invent anything not in the transcript.";

/// The `/handoff` instruction: unlike compaction (preserve everything densely), a handoff is
/// GOAL-CONDITIONED — extract only what the NEW objective needs and drop the rest. This is the
/// fix for long-context drift that summarize-in-place preserves (the Amp lesson: a fresh thread
/// seeded with relevant context beats an old thread summarized).
const HANDOFF_SYS: &str = "You extract ONLY the context relevant to a NEW goal from a prior \
    conversation: decisions, file paths, constraints, gotchas, and command outcomes that bear on \
    that goal. Omit everything else — unrelated work, dead ends, pleasantries. Terse bullets. Do \
    NOT invent anything not in the transcript.";

/// Build the `/handoff` summarization prompt (goal-conditioned extraction over the transcript).
pub fn handoff_prompt(history: &[Message], goal: &str) -> Vec<Message> {
    // Skip [0] (the system prompt is not conversation) — same convention as compaction's cut.
    let transcript = render_transcript(history.get(1..).unwrap_or_default());
    vec![
        Message::system(HANDOFF_SYS),
        Message::user(format!("NEW GOAL:\n{goal}\n\nPrior conversation:\n\n{transcript}")),
    ]
}

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

/// What the conversation touched, harvested from the tool-call history: files read/edited and
/// skills loaded. Powers the `/compact` summary tree — after the older turns collapse into one
/// dense note, this shows AT A GLANCE the concrete context those turns carried (which files, which
/// skills), the part most worth not losing track of.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Touchpoints {
    /// Distinct file paths that appeared as a `path` argument to a file tool, in first-seen order.
    pub files: Vec<String>,
    /// Distinct skill names loaded via `skill_load`, in first-seen order.
    pub skills: Vec<String>,
}

/// Extract [`Touchpoints`] from `history` (files referenced by file tools + skills loaded). Pure
/// over the message list so it's unit-testable, and order-preserving + de-duplicated so the tree
/// reads like a short memory of the turn. Call this BEFORE compaction — once the older turns are
/// summarized their tool calls are gone, so the harvest must happen while they're still present.
pub fn context_touchpoints(history: &[Message]) -> Touchpoints {
    let mut tp = Touchpoints::default();
    let push = |v: &mut Vec<String>, s: &str| {
        let s = s.trim();
        if !s.is_empty() && !v.iter().any(|e| e == s) {
            v.push(s.to_string());
        }
    };
    for m in history {
        for call in &m.tool_calls {
            let args: serde_json::Value =
                serde_json::from_str(&call.function.arguments).unwrap_or(serde_json::Value::Null);
            match call.function.name.as_str() {
                "file_read" | "file_edit" | "file_write" | "multi_edit" => {
                    if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
                        push(&mut tp.files, p);
                    }
                }
                "skill_load" => {
                    if let Some(n) = args.get("name").and_then(|v| v.as_str()) {
                        push(&mut tp.skills, n);
                    }
                }
                _ => {}
            }
        }
    }
    tp
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

/// How many times `history` has been compacted, read from the boundary marker (P-ctx3). Zero if no
/// boundary note is present. The count lives IN the marker text (`#N`) rather than a side counter,
/// so it is correct after session save/restore (which only round-trips `messages`) and after a
/// handoff — anywhere the boundary note travels, its count travels with it.
pub fn compaction_count(history: &[Message]) -> usize {
    history
        .iter()
        .find(|m| m.role == "system" && m.content.as_deref().is_some_and(|c| c.starts_with(COMPACT_MARKER_PREFIX)))
        .and_then(|m| parse_marker_seq(m.content.as_deref().unwrap_or("")))
        .unwrap_or(0)
}

/// Parse the `#N` sequence number out of a boundary marker's first line. `None` if absent/garbled.
fn parse_marker_seq(marker: &str) -> Option<usize> {
    let first = marker.lines().next().unwrap_or("");
    let hash = first.rfind('#')?;
    let digits: String = first[hash + 1..].chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Build the boundary-note text for the `seq`-th compaction (1-based). Queryable via
/// [`COMPACT_MARKER_PREFIX`] + [`parse_marker_seq`].
fn marker_text(seq: usize, summary: &str) -> String {
    format!("{COMPACT_MARKER_PREFIX} to conserve context · #{seq}]\n{summary}")
}

/// Rebuild `history` in place as: the system prompt (`[0]`) + one summary `system` boundary note +
/// the verbatim tail from `cut`. The summary should already be trimmed. The boundary note carries a
/// running compaction count: the number is read off any PRIOR boundary note (which is about to be
/// summarized into this one) and incremented, so the count accumulates across successive
/// compactions even though each old boundary note is folded into the next summary.
pub fn splice_compacted(history: &mut Vec<Message>, cut: usize, summary: &str) {
    let seq = compaction_count(history) + 1;
    let system_prompt = history[0].clone();
    let tail: Vec<Message> = history[cut..].to_vec();
    let mut rebuilt = Vec::with_capacity(2 + tail.len());
    rebuilt.push(system_prompt);
    rebuilt.push(Message::system(marker_text(seq, summary)));
    rebuilt.extend(tail);
    *history = rebuilt;
}

/// Rough size in tokens — for the before/after trace only. Delegates to the shared estimator so
/// compaction traces agree with the loop guards and the HUD.
fn approx_tokens(history: &[Message]) -> usize {
    history.iter().map(super::estimate_message_tokens).sum()
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

    #[test]
    fn handoff_prompt_embeds_goal_and_skips_system() {
        let history = vec![
            Message::system("SYSTEM PROMPT — never in the transcript"),
            Message::user("old task about the parser"),
            Message::assistant("fixed it in src/parse.rs"),
        ];
        let p = handoff_prompt(&history, "now optimize the lexer");
        assert_eq!(p.len(), 2);
        assert!(p[0].content.as_deref().unwrap().contains("ONLY the context relevant"));
        let usr = p[1].content.as_deref().unwrap();
        assert!(usr.contains("NEW GOAL:\nnow optimize the lexer"), "{usr}");
        assert!(usr.contains("src/parse.rs"), "transcript present");
        assert!(!usr.contains("SYSTEM PROMPT"), "system prompt never leaks into the extraction");
    }

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
    async fn compact_prompt_asks_to_preserve_a_reflection_section() {
        // Reflexion (arXiv:2303.11366) ported to compaction: the summary must carry a REFLECTION
        // section — approaches tried-and-failed and why — so a compacted continuation doesn't repeat
        // the same dead ends. Capture the prompt the summarizer receives and assert the instruction.
        use std::sync::{Arc, Mutex};
        let captured: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        let cap = captured.clone();
        let mut h = vec![
            Message::system("SYS"),
            user("u1"), asst("a1"),
            user("u2"), asst("a2"),
        ];
        compact_history(
            &mut h,
            move |msgs| {
                *cap.lock().unwrap() = msgs[0].content.clone().unwrap_or_default();
                async { Ok("S".to_string()) }
            },
            1,
        )
        .await
        .expect("compaction should succeed");
        let sys = captured.lock().unwrap().to_ascii_uppercase();
        assert!(sys.contains("REFLECTION"), "summarizer instruction must request a reflection section: {sys}");
        assert!(sys.contains("FAILED"), "…and specifically failed approaches: {sys}");
    }

    #[tokio::test]
    async fn compact_history_errors_when_too_short() {
        let mut h = vec![Message::system("SYS"), user("only one turn")];
        let r = compact_history(&mut h, |_m| async { Ok("x".to_string()) }, KEEP_TURNS).await;
        assert!(r.is_err(), "a single-turn conversation can't be compacted");
    }

    #[test]
    fn boundary_marker_is_queryable_and_counts_accumulate() {
        // P-ctx3: a fresh history has zero compactions; the marker is detectable by prefix; and the
        // count accumulates across successive compactions even though each old boundary note is
        // folded into the next summary.
        let base = || vec![
            Message::system("SYS"),
            user("u1"), asst("a1"),
            user("u2"), asst("a2"),
            user("u3"), asst("a3"),
        ];
        let mut h = base();
        assert_eq!(compaction_count(&h), 0, "no boundary yet");
        splice_compacted(&mut h, 3, "SUMMARY_1");
        assert_eq!(compaction_count(&h), 1, "first compaction → #1");
        assert!(
            h[1].content.as_deref().unwrap().starts_with(COMPACT_MARKER_PREFIX),
            "boundary note detectable by prefix"
        );
        // Grow the tail and compact again: the prior #1 boundary sits before the new cut, so it is
        // summarized away — but the sequence must still advance to #2 (read off the old marker).
        h.push(user("u4"));
        h.push(asst("a4"));
        h.push(user("u5"));
        h.push(asst("a5"));
        let cut = plan_compact_cut(&h, 1).expect("compactable");
        splice_compacted(&mut h, cut, "SUMMARY_2");
        assert_eq!(compaction_count(&h), 2, "count accumulates to #2 across compactions");
        // Exactly one boundary note survives (the newest); the old one was folded in.
        let markers = h.iter().filter(|m| {
            m.role == "system" && m.content.as_deref().is_some_and(|c| c.starts_with(COMPACT_MARKER_PREFIX))
        }).count();
        assert_eq!(markers, 1, "only the newest boundary note remains");
    }

    #[test]
    fn parse_marker_seq_handles_missing_and_garbled() {
        assert_eq!(parse_marker_seq("no hash here"), None);
        assert_eq!(parse_marker_seq("[... · #7]\nsummary"), Some(7));
        assert_eq!(parse_marker_seq("[... · #]\nx"), None); // hash with no digits
        assert_eq!(parse_marker_seq("[... · #12] trailing"), Some(12));
    }

    fn tool_call(name: &str, args: &str) -> Message {
        Message::assistant_tool_calls(vec![ToolCall {
            id: "x".into(),
            kind: "function".into(),
            function: FunctionCall { name: name.into(), arguments: args.into() },
        }])
    }

    #[test]
    fn touchpoints_harvest_files_and_skills_ordered_and_deduped() {
        let h = vec![
            Message::system("SYS"),
            user("do the thing"),
            tool_call("file_read", r#"{"path":"src/main.rs"}"#),
            tool_call("skill_load", r#"{"name":"deep-research"}"#),
            tool_call("file_edit", r#"{"path":"src/main.rs","old_string":"a","new_string":"b"}"#),
            tool_call("file_write", r#"{"path":"src/new.rs","content":"x"}"#),
            tool_call("multi_edit", r#"{"path":"src/ui/tui.rs","edits":[]}"#),
            tool_call("skill_load", r#"{"name":"deep-research"}"#),
            tool_call("shell_run", r#"{"command":"ls"}"#),
        ];
        let tp = context_touchpoints(&h);
        // First-seen order, de-duplicated (main.rs appears twice → once), non-file tools ignored.
        assert_eq!(tp.files, vec!["src/main.rs", "src/new.rs", "src/ui/tui.rs"]);
        assert_eq!(tp.skills, vec!["deep-research"]);
    }

    #[test]
    fn touchpoints_empty_when_no_tool_calls_and_survives_garbled_args() {
        let clean = vec![Message::system("SYS"), user("hi"), asst("hello")];
        assert_eq!(context_touchpoints(&clean), Touchpoints::default());
        // Malformed JSON arguments must not panic — they just yield nothing.
        let garbled = vec![tool_call("file_read", "not json{")];
        assert_eq!(context_touchpoints(&garbled), Touchpoints::default());
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
