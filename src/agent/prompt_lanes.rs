//! How the system prompt is assembled, and what each turn folds into the message it sends.
//!
//! The prompt is not one string but a sequence of LANES at the head of the conversation, ordered
//! most-stable-first: the frozen core, the persona, the skills index, then the per-turn dynamic
//! lane. That order is the whole point — a provider's prefix cache holds only while the leading
//! bytes are unchanged, so anything that varies per turn must come last, and anything that varies
//! per project must not be spliced into a lane that a cached prefix already covers.
//!
//! Retrieval works the same way and for the same reason: memory recall and codebase hits are folded
//! into the SENT user message, never into a system lane, so the cached prefix survives a turn that
//! retrieves. The stored history keeps the clean text the user typed.

use crate::agent;
use crate::core::types::Message;
use crate::memory;
use crate::persona;
use crate::ui::tui;
use crate::*;

/// Build prompt lanes around a caller-selected frozen core. Keeping the lifecycle choice OUT of this
/// helper makes every call site say whether it is opening a fresh conversation (refresh/adopt) or
/// merely rewriting lanes inside the current one (read the already-adopted bytes).
pub(crate) fn system_prompt_bundle_with_core(model: &str, frozen: &str) -> agent::PromptBundle {
    system_prompt_bundle_in(model, frozen, None)
}

/// As `system_prompt_bundle_with_core`, but stating an EXPLICIT working directory in the prompt.
/// The hostbot daemon passes its lane's cwd: the prompt tells the model where it is working, and
/// under several concurrent bots the process cwd is not that answer for any of them.
pub(crate) fn system_prompt_bundle_in(
    model: &str,
    frozen: &str,
    root: Option<&std::path::Path>,
) -> agent::PromptBundle {
    let cwd = root
        .map(|p| p.display().to_string())
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.display().to_string())
        })
        .unwrap_or_else(|| ".".to_string());
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let mut bundle = agent::build_top_level_system_prompt_bundle(
        &cwd,
        std::env::consts::OS,
        &date,
        model,
        Some(frozen),
    );
    // L2 session working memory (temporary, budget-capped). Empty → no tag (zero cost).
    let sess_budget = memory::settings().session_mem_max_tokens;
    if let Some(block) = memory::session_mem::process_prompt_block(sess_budget) {
        bundle.dynamic.push('\n');
        bundle.dynamic.push_str(&block);
        bundle.dynamic.push('\n');
    }
    bundle
}

/// A TRUE conversation boundary: promote pending memory, rebuild from the current store, and adopt
/// the result before constructing a fresh prompt prefix. Startup, `/clear`, `/handoff`, session load,
/// and one-shot/captured runs are the only callers that should use this path.
pub(crate) fn refreshed_system_prompt_bundle(model: &str) -> agent::PromptBundle {
    let frozen = memory::refresh_frozen_core();
    system_prompt_bundle_with_core(model, &frozen)
}

/// Same-conversation lane rewrite: reuse the already-adopted core byte-for-byte. A retrieval,
/// reinforcement, or memory write during this conversation may stage `core.next`, but must not
/// mutate the cached prefix or promote it before the next conversation boundary.
pub(crate) fn active_system_prompt_bundle(model: &str) -> agent::PromptBundle {
    let frozen = memory::active_frozen_core();
    system_prompt_bundle_with_core(model, &frozen)
}

/// The two bundle builders above, parameterized by the hostbot lane's working directory and persona.
/// Both are needed together because the persona override must be armed for the (synchronous) prompt
/// assembly and released immediately after — see `persona::with_override`.
pub(crate) fn hostbot_prompt_bundle(
    model: &str,
    root: &std::path::Path,
    persona: Option<String>,
    boundary: bool,
) -> agent::PromptBundle {
    persona::with_override(persona, || {
        let frozen = if boundary {
            memory::refresh_frozen_core()
        } else {
            memory::active_frozen_core()
        };
        system_prompt_bundle_in(model, &frozen, Some(root))
    })
}

/// Seed both system lanes for a brand-new conversation.
pub(crate) fn seed_prompt_lanes(history: &mut Vec<Message>, model: &str) {
    history.clear();
    let bundle = refreshed_system_prompt_bundle(model);
    history.push(Message::system(bundle.stable));
    if !bundle.dynamic.trim().is_empty() {
        history.push(Message::system(bundle.dynamic));
    }
}

/// Replace a persisted zero/one-system legacy prefix with the current two-lane prompt.
/// Histories already carrying both lanes are left byte-identical.
pub(crate) fn migrate_legacy_prompt_lanes(history: &mut Vec<Message>, model: &str) {
    let lead = agent::compact::leading_system_count(history);
    if lead >= 2 {
        return;
    }
    let tail = history.get(lead..).unwrap_or_default().to_vec();
    seed_prompt_lanes(history, model);
    history.extend(tail);
}

/// Per-turn budget (tokens, chars/4 estimate) for the `/init` codebase-retrieval block folded into
/// the CURRENT user turn (see [`fold_retrieval_into_query`]). Small enough to stay well under the
/// frozen-core/session budgets but big enough for ~5-8 chunks with attribution.
pub(crate) const CODEBASE_RETRIEVAL_BUDGET_TOKENS: usize = 1500;

/// Fresh user-turn boundary: refresh only the dynamic lane, preserving stable index 0 byte-for-byte.
pub(crate) fn refresh_dynamic_prompt_lane(history: &mut Vec<Message>, model: &str) {
    migrate_legacy_prompt_lanes(history, model);
    let dynamic = active_system_prompt_bundle(model).dynamic;
    let lead = agent::compact::leading_system_count(history);
    if dynamic.trim().is_empty() {
        if lead > 1 {
            history.remove(1);
        }
    } else if lead > 1 {
        history[1] = Message::system(dynamic);
    } else {
        history.insert(1, Message::system(dynamic));
    }
}

/// Rewrite BOTH system lanes in place, preserving every non-system message.
///
/// For settings changes that alter the STABLE lane — `/model` and `/config` both do, since the model
/// name, prompt tier and `<project_context>` live at index 0 — but must NOT end the conversation.
/// `rebuild_system` cannot serve here: it calls `seed_prompt_lanes`, which starts with
/// `history.clear()`, so using it for a settings change silently threw away the whole chat (the user
/// went to `/config` to retune the context and came back to an empty thread).
///
/// Session working memory is deliberately KEPT: this is the same conversation, so its scratch notes
/// are still valid. That is the other half of why `/config` must not route through `rebuild_system`,
/// which drops them as part of starting a new thread.
/// Splice a caller-selected prompt bundle over the leading prompt lanes while preserving every
/// conversation message (including a handoff seed, which is a third system message but NOT part of
/// the two-lane prefix).
pub(crate) fn splice_prompt_lanes(history: &mut Vec<Message>, bundle: agent::PromptBundle) {
    let lead = agent::compact::leading_system_count(history);
    let mut lanes = vec![Message::system(bundle.stable)];
    if !bundle.dynamic.trim().is_empty() {
        lanes.push(Message::system(bundle.dynamic));
    }
    history.splice(0..lead, lanes);
}

/// Same-conversation rewrite (`/config`, `/model`, persona change): keep the active core stable.
pub(crate) fn refresh_prompt_lanes_in_place(history: &mut Vec<Message>, model: &str) {
    splice_prompt_lanes(history, active_system_prompt_bundle(model));
}

/// Thread switch (`/resume`, session/time-machine restore): refresh/adopt memory for the new
/// conversation before rebuilding the current-project prompt lanes around its saved transcript.
pub(crate) fn refresh_prompt_lanes_for_thread_switch(history: &mut Vec<Message>, model: &str) {
    splice_prompt_lanes(history, refreshed_system_prompt_bundle(model));
}

/// Automatic codebase RAG, folded into the CURRENT user turn (NOT the dynamic system lane).
///
/// When `/init` has built an index, the top-ranked chunks (path + line range + real content,
/// source-attributed) are prepended to the user's message so the model sees relevant code before it
/// even calls a tool. Placing it on the user turn — the volatile, already-uncached message — keeps
/// index 1 (the dynamic system lane) byte-stable, so the provider's prefix cache still covers the
/// whole transcript tail up to the last stable turn. Folding into the dynamic lane instead would
/// vary index 1 every turn and force the entire transcript after it to re-bill uncached (the
/// Anthropic prefix-cache breakpoint sits on the last stable assistant/tool message).
///
/// Returns the message content to send. The caller keeps the ORIGINAL `query` for checkpoint /
/// display / persisted history — only the sent content carries the (ephemeral, per-turn) block.
/// No-op passthrough when there is no index / no query terms / nothing clears the relevance gate.
pub(crate) fn fold_retrieval_into_query(query: &str) -> String {
    if query.trim().is_empty() {
        return query.to_string();
    }
    // Kick a background drift check: if source files changed since the last /init, an incremental
    // rebuild runs off-turn so the NEXT turn sees fresh context. Never blocks this turn (#17).
    crate::agent::codebase::ensure_fresh();
    match crate::agent::codebase::retrieval_block(query, CODEBASE_RETRIEVAL_BUDGET_TOKENS) {
        Some(block) => format!("{block}\n\n{query}"),
        None => query.to_string(),
    }
}

/// Per-turn budget (tokens) for the memory recall block folded into the CURRENT user turn.
/// Deliberately an order of magnitude under the codebase budget: this carries a handful of
/// one-line facts, not source, and it is spent on every gated turn.
pub(crate) const MEMORY_RECALL_BUDGET_TOKENS: usize = 300;

/// Fold BOTH per-turn context blocks into the sent content: memory recall, then codebase RAG.
///
/// Same discipline as [`fold_retrieval_into_query`] and for the same reason — the blocks ride on the
/// **user turn**, which is already uncached, so system lanes 0/1 stay byte-stable and the provider's
/// prefix cache keeps covering the transcript tail (invariant I1).
///
/// Memory goes FIRST so the standing facts ("reply in Vietnamese", "windows-sys is pinned") are read
/// before the code they qualify. The recall block also seats its handle→id pairs in the pending
/// ledger, which is what lets a later `used` report confirm only facts that were actually shown.
///
/// `query` itself is never modified: the caller keeps it for checkpoint / display / persisted
/// history, so the durable transcript holds the user's real words, not our scaffolding.
pub(crate) fn fold_context_into_query(query: &str) -> String {
    // The turn counter lives here because this is the one point BOTH REPL loops pass through exactly
    // once per user message. Counting inside the agent loop would count iterations, and metric 1's
    // denominator ("live facts per turn") has to mean turns the user drove.
    memory::stats::note_turn();
    let mut out = fold_retrieval_into_query(query);
    // Skills that actually fit THIS question, gated on the same coverage threshold as recall. The
    // always-on `<skills>` index names every applicable procedure regardless of the request; this
    // block is what makes the fitting ones salient without spending the system lane's byte-stable
    // budget on the ones that don't. Folded ABOVE the code but BELOW the facts, matching the
    // "standing truth → how-to → source" reading order.
    if let Some(block) = skills::turn_block(query, skills::SKILL_TURN_BUDGET_TOKENS) {
        out = format!("{block}\n\n{out}");
    }
    if let Some((block, pairs)) = memory::recall_block(query, MEMORY_RECALL_BUDGET_TOKENS) {
        memory::pending::open_turn(pairs);
        out = format!("{block}\n\n{out}");
    }
    out
}

/// Drop per-turn context blocks (memory recall, gated skills) from user turns already in `history`.
///
/// Each block was true for the turn it rode in on. Left in place they accumulate — ten turns of
/// standing facts re-stated ten times — and, worse, an older block can contradict a newer one with
/// nothing in the transcript marking which came later, so the model has to guess.
///
/// Called only from [`maybe_auto_compact`], at the moment the prefix cache is being invalidated
/// anyway: rewriting a user turn at any other time would break cache coverage for the whole tail,
/// costing more than the tokens it saves.
///
/// Matches on [`memory::RECALL_MARKER`] / [`skills::SKILL_MARKER`] at the start of the content and
/// cuts through the first blank line. Anything the user actually typed survives, including a message
/// that merely mentions the phrase — a marker has to be at position 0, which only our own folding
/// produces. Both are peeled in a loop because a turn carries them stacked (recall, then skills):
/// removing the outer one promotes the inner one to position 0.
pub(crate) fn strip_recall_blocks(history: &mut [Message]) {
    for m in history.iter_mut() {
        if m.role != "user" {
            continue;
        }
        let Some(content) = m.content.as_deref() else {
            continue;
        };
        let mut cur = content;
        loop {
            let next = strip_skill_prefix(memory::strip_recall_prefix(cur));
            if next.len() == cur.len() {
                break;
            }
            cur = next;
        }
        if cur.len() != content.len() {
            m.content = Some(cur.to_string());
        }
    }
}

/// Peel one leading gated-skill block, mirroring [`memory::strip_recall_prefix`].
pub(crate) fn strip_skill_prefix(content: &str) -> &str {
    if !content.starts_with(skills::SKILL_MARKER) {
        return content;
    }
    match content.split_once("\n\n") {
        Some((_, rest)) => rest,
        None => content,
    }
}

/// Everything a THREAD SWITCH must reset besides history itself: session scratch memory, todos,
/// the cost tally, destructive-op session grants, and browser page @refs. `/clear`, `/handoff`,
/// `/resume`, `/sessions` restore and `/recover` all route here so a fresh or restored thread
/// never inherits the previous one's state (the classic leak: a restored conversation still
/// "allowed" the old thread's destructive ops and showed its cost).
pub(crate) fn reset_per_session_state() {
    memory::session_mem::clear_process_session_mem();
    // The new transcript never contained the old recall block, so its handles now point at facts
    // the model cannot see — and a stale `last_ids` would suppress the first block of the new
    // thread as a "duplicate" of one that is no longer in context.
    memory::pending::clear();
    crate::agent::todo::clear();
    client::cost_meter().reset();
    tui::reset_session_allow();
    #[cfg(feature = "browser")]
    crate::agent::browser::release_active();
}

/// Reset the conversation to just the system prompt (fresh session / model change). Rebuilds the
/// frozen core from the current memory store so newly added `type=user` facts / STYLE are injected.
/// Drops session working memory — a new thread does not inherit this session's scratch notes.
pub(crate) fn rebuild_system(history: &mut Vec<Message>, model: &str) {
    memory::session_mem::clear_process_session_mem();
    seed_prompt_lanes(history, model);
}

/// Replace the system lanes in place WITHOUT clearing the conversation — used when switching
/// persona mid-chat so the new character applies but the history is preserved.
pub(crate) fn update_system_prompt(history: &mut Vec<Message>, model: &str) {
    refresh_dynamic_prompt_lane(history, model);
}
