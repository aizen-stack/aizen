//! Off-to-the-side Q&A — "hỏi ngoài lề khi aizen đang làm việc".
//!
//! Steering ([`crate::core::steer`]) answers "fold this INTO the running turn". An aside answers the
//! opposite need: the user has a question that is NOT about the task in flight — "what does this
//! flag do?", "remind me the git incantation" — and wants it answered WITHOUT perturbing the run. A
//! steer mutates the turn's `history`/trajectory; an aside must not touch it at all.
//!
//! Why a dedicated worker rather than the ordinary submission queue: the running turn holds
//! `&mut history` exclusively and blocks the REPL inside its `tokio::select!`, so nothing drains the
//! submission queue until the turn ends — an aside pushed there would sit until the work finished,
//! defeating the point. Instead a long-lived worker task (spawned once by the REPL) owns this
//! channel. On each question it CLONES the read-only live-conversation snapshot
//! (`live_history_slot`, kept current DURING the turn via `on_progress`), makes exactly ONE
//! tool-less model call, and prints the answer through the `tui::emit_line` funnel — which the
//! retained renderer serializes with the main stream on a single render thread, so a mid-stream
//! aside can never corrupt the frame. It never appends to `history`, never arms cancel, never sets
//! `WORKING`: the turn in flight is oblivious.
//!
//! Routing mirrors the `>` steer prefix: the input thread strips a leading `?` and calls [`ask`];
//! a `false` return (idle, no worker, or oversized) falls through to the ordinary message path with
//! the marker removed, so the text is delivered either way and the model never sees the `?`.

use std::sync::OnceLock;
use tokio::sync::mpsc::UnboundedSender;

use crate::core::types::Message;

/// Per-aside length cap. An aside is a quick question, not a document — a large paste belongs in its
/// own turn (where it can carry `@file` expansion and tools), not in a tool-less side call.
pub const MAX_CHARS: usize = 2000;

/// How many trailing natural-language turns of the live conversation to hand the aside as context.
/// Short by design: enough to answer "what is it doing right now?" without re-sending the whole
/// transcript at full price for a throwaway question.
pub const CONTEXT_TAIL: usize = 6;

/// Per-context-message clamp so one enormous assistant turn in the tail can't blow up the aside
/// request. The gist is what an aside needs, not the full text.
const PER_MSG_CHARS: usize = 500;

static SENDER: OnceLock<UnboundedSender<String>> = OnceLock::new();

/// Install the worker's sending half. Called once by the REPL when it spawns the aside worker.
/// Idempotent: a second arm (should not happen — one REPL) is ignored rather than panicking.
pub fn arm(tx: UnboundedSender<String>) {
    let _ = SENDER.set(tx);
}

/// Is an aside worker running (i.e. are we in the interactive sticky REPL)?
///
/// `offer` already returns `false` when no worker is armed, so callers branch on that instead of
/// asking first — one check rather than two, with no window between them.
#[allow(dead_code)]
pub fn is_available() -> bool {
    SENDER.get().is_some()
}

/// Offer a side question to the worker. Returns `false` — the caller should fall back to a normal
/// submission — when no worker is armed, the text is blank, or it exceeds [`MAX_CHARS`]. The gate
/// on "a turn is in flight" lives at the call site (an aside only makes sense beside running work);
/// here we only refuse what the worker cannot use.
pub fn ask(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() || trimmed.chars().count() > MAX_CHARS {
        return false;
    }
    match SENDER.get() {
        Some(tx) => tx.send(trimmed.to_string()).is_ok(),
        None => false,
    }
}

/// System instruction that pins the aside as OUT-OF-BAND: answer briefly, act on nothing. The model
/// has no tools on this call, but saying so keeps it from narrating an intent to edit/continue the
/// work it can see in the context tail.
const SYSTEM: &str = "You are answering a QUICK SIDE QUESTION the user typed while a SEPARATE task \
is still running on another thread. This is out-of-band. Answer the question itself, directly and \
concisely. Do NOT try to act on, continue, plan, or modify the task in progress — you have no tools \
here and cannot change files or run anything. If the question is about the running work, use the \
provided context to answer it, but treat that context as read-only background. Keep it short.";

/// Build the tool-less request for one side question: the out-of-band system instruction, a short
/// read-only recap of the live conversation's tail (for "what is it doing?"-style questions), then
/// the question. Tool-call and tool-result turns are dropped from the recap — an aside needs the
/// gist, not the mechanics, and a dangling `tool_calls` without its result would make a strict
/// gateway 400 the side call.
pub fn build_messages(snapshot: &[Message], question: &str) -> Vec<Message> {
    let mut msgs = vec![Message::system(SYSTEM)];

    let tail: Vec<&Message> = snapshot
        .iter()
        .filter(|m| {
            (m.role == "user" || m.role == "assistant")
                && m.tool_calls.is_empty()
                && m.content
                    .as_deref()
                    .map(|c| !c.trim().is_empty())
                    .unwrap_or(false)
        })
        .collect();
    let start = tail.len().saturating_sub(CONTEXT_TAIL);
    if start < tail.len() {
        let recap = tail[start..]
            .iter()
            .map(|m| {
                let who = if m.role == "user" {
                    "User"
                } else {
                    "Assistant"
                };
                let body = truncate_chars(m.content.as_deref().unwrap_or("").trim(), PER_MSG_CHARS);
                format!("{who}: {body}")
            })
            .collect::<Vec<_>>()
            .join("\n");
        // Fold the recap into a user/assistant exchange rather than a second system message, so a
        // provider that only honours one system turn still sees it, and it reads as background the
        // model has already acknowledged.
        msgs.push(Message::user(format!(
            "For context, here are the last messages of the work in progress (read-only \
             background — do not act on it):\n\n{recap}"
        )));
        msgs.push(Message::assistant(
            "Understood — that's the background. What's your side question?",
        ));
    }

    msgs.push(Message::user(question.to_string()));
    msgs
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ask_refuses_blank_and_oversized_and_unarmed() {
        // SENDER is process-global and never armed in the unit-test binary, so `ask` always returns
        // false here — which is exactly the "no worker" contract we want to assert. Blank and
        // oversized are refused BEFORE the sender is even consulted.
        assert!(!ask("   \n  "), "blank aside is not a question");
        let huge = "x".repeat(MAX_CHARS + 1);
        assert!(!huge.is_empty());
        assert!(!ask(&huge), "oversized aside belongs in its own turn");
        assert!(
            !ask("what does this flag do?"),
            "no worker armed in the test binary → caller must fall through to a normal message"
        );
    }

    #[test]
    fn build_messages_has_system_then_question_last() {
        let msgs = build_messages(&[], "what is a borrow checker?");
        assert_eq!(msgs.first().unwrap().role, "system");
        assert!(msgs
            .first()
            .unwrap()
            .content
            .as_deref()
            .unwrap()
            .contains("SIDE QUESTION"));
        let last = msgs.last().unwrap();
        assert_eq!(last.role, "user");
        assert_eq!(last.content.as_deref(), Some("what is a borrow checker?"));
        // With no snapshot there is no recap pair — just system + question.
        assert_eq!(msgs.len(), 2, "empty snapshot ⇒ no context recap");
    }

    #[test]
    fn build_messages_recaps_a_short_tail_and_drops_tool_turns() {
        let snapshot = vec![
            Message::user("please refactor the parser"),
            Message::assistant_tool_calls(vec![]), // no content, no tool_calls → filtered as empty
            Message::assistant("Reading src/parser.rs first."),
            Message::tool_result("call_1", "file contents here"), // role=tool → dropped
            Message::user("also keep the old API"),
        ];
        let msgs = build_messages(&snapshot, "wait, what language is this project in?");
        // system + recap-user + recap-assistant + question
        assert_eq!(msgs.len(), 4);
        let recap = msgs[1].content.as_deref().unwrap();
        assert!(recap.contains("User: please refactor the parser"));
        assert!(recap.contains("Assistant: Reading src/parser.rs first."));
        assert!(recap.contains("User: also keep the old API"));
        assert!(
            !recap.contains("file contents here"),
            "tool-result turns are dropped from the recap"
        );
        assert_eq!(
            msgs[2].role, "assistant",
            "recap is folded as an acknowledged exchange"
        );
        assert_eq!(
            msgs.last().unwrap().content.as_deref(),
            Some("wait, what language is this project in?")
        );
    }

    #[test]
    fn recap_is_capped_to_the_last_context_tail_turns() {
        // 20 assistant/user turns → only the last CONTEXT_TAIL appear in the recap.
        let mut snapshot = Vec::new();
        for i in 0..20 {
            snapshot.push(Message::user(format!("message number {i}")));
        }
        let msgs = build_messages(&snapshot, "quick q");
        let recap = msgs[1].content.as_deref().unwrap();
        assert!(
            recap.contains("message number 19"),
            "newest turn is present"
        );
        assert!(
            recap.contains(&format!("message number {}", 20 - CONTEXT_TAIL)),
            "the tail window starts CONTEXT_TAIL back"
        );
        assert!(
            !recap.contains("message number 0"),
            "the oldest turns are outside the tail window"
        );
    }

    #[test]
    fn a_giant_context_message_is_clamped() {
        let big = "word ".repeat(1000); // ~5000 chars, well over PER_MSG_CHARS
        let msgs = build_messages(&[Message::assistant(big)], "q");
        let recap = msgs[1].content.as_deref().unwrap();
        assert!(
            recap.ends_with('…'),
            "an oversized context turn is truncated with an ellipsis"
        );
        assert!(
            recap.chars().count() < 700,
            "clamped near PER_MSG_CHARS, not the full 5000"
        );
    }
}
