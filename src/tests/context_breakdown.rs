//! Tests for the /context breakdown report (crate root).

use super::*;

#[test]
fn splits_system_prompt_into_blocks() {
    let sys = "BASE RULES HERE\n\n<environment>\ncwd: /x\n</environment>\n\
                   <user_memory>\n- terse\n</user_memory>\n<skills>\nidx\n</skills>\n";
    let rows = system_block_chars(sys);
    // base instructions is always first.
    assert_eq!(rows[0].0, "base instructions");
    let labels: Vec<&str> = rows.iter().map(|(l, _)| *l).collect();
    assert!(labels.contains(&"environment"));
    assert!(labels.contains(&"user memory"));
    assert!(labels.contains(&"skills index"));
    // absent blocks aren't reported.
    assert!(!labels.contains(&"persona"));
    assert!(!labels.contains(&"agents index"));
    // block char counts + base sum to the whole prompt (nothing double-counted or dropped).
    let sum: usize = rows.iter().map(|(_, c)| *c).sum();
    assert_eq!(sum, sys.chars().count());
}

#[test]
fn base_only_prompt_reports_just_base() {
    let sys = "just the base, no tagged blocks";
    let rows = system_block_chars(sys);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0], ("base instructions", sys.chars().count()));
}

#[test]
fn fold_retrieval_passthrough_when_empty_or_no_index() {
    // Empty / whitespace query → returned verbatim (nothing to retrieve against).
    assert_eq!(fold_retrieval_into_query(""), "");
    assert_eq!(fold_retrieval_into_query("   "), "   ");
    // A real query is either an identity passthrough (no index) or `block\n\n{query}` (index
    // hit). Either way the ORIGINAL query text is preserved intact at the END — the RAG fold
    // only ever PREPENDS attributed context, never rewrites the user's words. (Robust whether or
    // not this repo has a persisted /init index, since tests share the process cwd.)
    let q = "how does the payment flow work";
    let sent = fold_retrieval_into_query(q);
    assert!(
        sent == q || sent.ends_with(&format!("\n\n{q}")),
        "query must be preserved: {sent:?}"
    );
}

#[test]
fn context_fold_preserves_the_user_text_and_never_touches_system_lanes() {
    // Invariant I1: both per-turn blocks ride on the USER turn, so system lanes 0/1 stay
    // byte-stable and the provider's prefix cache keeps covering the transcript tail.
    let q = "does the user prefer pnpm";
    let sent = fold_context_into_query(q);
    assert!(
        sent == q || sent.ends_with(&format!("\n\n{q}")),
        "the user's own words must survive verbatim at the end: {sent:?}"
    );

    // The fold is a pure string transform over the query — it takes no `history`, so there is no
    // path by which it could write a system lane. Assert the shape that makes that true: any
    // injected block is a PREFIX, never a rewrite of the tail.
    let mut history = vec![
        Message::system("stable lane"),
        Message::system("dynamic lane"),
    ];
    let before: Vec<Option<String>> = history.iter().map(|m| m.content.clone()).collect();
    history.push(Message::user(sent));
    strip_recall_blocks(&mut history);
    let after: Vec<Option<String>> = history.iter().take(2).map(|m| m.content.clone()).collect();
    assert_eq!(
        before, after,
        "system lanes must be untouched byte-for-byte"
    );
}

#[test]
fn strip_recall_blocks_drops_our_block_but_keeps_what_the_user_typed() {
    let typed = "why is the build slow";
    let folded = format!(
        "{} (may be stale…):\n[m1] (about you) prefers pnpm\n\n{typed}",
        memory::RECALL_MARKER
    );
    let mut history = vec![
        Message::system("lane"),
        Message::user(folded),
        Message::assistant("some reply"),
        // A user message that merely MENTIONS the phrase mid-sentence must be left alone: only
        // our own folding puts the marker at position 0.
        Message::user(format!("tell me about {} handling", memory::RECALL_MARKER)),
    ];
    strip_recall_blocks(&mut history);

    assert_eq!(
        history[1].content.as_deref(),
        Some(typed),
        "block stripped, question kept"
    );
    assert_eq!(
        history[2].content.as_deref(),
        Some("some reply"),
        "assistant turns untouched"
    );
    assert!(
        history[3]
            .content
            .as_deref()
            .is_some_and(|c| c.contains("handling")),
        "a user message that only mentions the marker must not be truncated"
    );

    // Idempotent: compacting twice must not eat the real message.
    strip_recall_blocks(&mut history);
    assert_eq!(history[1].content.as_deref(), Some(typed));
}

#[test]
fn strip_peels_stacked_recall_and_skill_blocks() {
    // A gated turn carries both, recall outermost (see `fold_context_into_query`). Peeling only
    // the outer one would leave the skill block welded to the user's words forever, so the stack
    // has to unwind — that is why the strip loops instead of testing each marker once.
    let typed = "deploy the staging service please";
    let folded = format!(
            "{} (may be stale…):\n[m1] (about you) prefers pnpm\n\n{} (call skill_load…):\n- deploy-vps: asked to deploy\n\n{typed}",
            memory::RECALL_MARKER,
            skills::SKILL_MARKER
        );
    let mut history = vec![
        Message::system("lane"),
        Message::user(folded),
        // Skill block alone, no recall above it — the other stacking order must work too.
        Message::user(format!(
            "{} (call skill_load…):\n- deploy-vps: asked to deploy\n\nship it",
            skills::SKILL_MARKER
        )),
    ];
    strip_recall_blocks(&mut history);

    assert_eq!(
        history[1].content.as_deref(),
        Some(typed),
        "both blocks peeled, question kept"
    );
    assert_eq!(
        history[2].content.as_deref(),
        Some("ship it"),
        "a lone skill block is peeled too"
    );

    strip_recall_blocks(&mut history);
    assert_eq!(
        history[1].content.as_deref(),
        Some(typed),
        "idempotent: a second compact must not eat the message"
    );
}
