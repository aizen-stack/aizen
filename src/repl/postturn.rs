//! Everything that happens AFTER a turn's last token: auto-compaction, the secretary pass, passive
//! memory learning, persona evolution, and the `/compact` · `/handoff` entry points.
//!
//! All of it is best-effort and bounded — a failed or slow learning pass must leave the conversation
//! exactly as the turn left it, which is why the whole block runs under one overall timeout.

use crate::agent;
use crate::agent::prompt_lanes::strip_recall_blocks;
use crate::core::endpoint::{http_client, resolve_endpoint};
use crate::core::types::ToolDef;
use crate::core::{cli_config, types};
use crate::llm::client;
use crate::skills as skill;
use crate::ui::context_report::{fmt_k, resolve_ctx_window, session_tokens};
use crate::ui::{icons, splash, tui};
use crate::{
    auto_skill_learn_enabled, compact_threshold_pct, extract_json_object, persona_evolve_enabled,
    summarizer_endpoint, COMPACT_KEEP_TURNS,
};
use crate::{memory, persona};
use anyhow::Result;
use console::style;
use types::Message;

/// After a completed turn: if auto-compact is enabled and context usage crossed the threshold,
/// summarize older turns in place. Best-effort — a failed summary leaves the conversation intact.
pub(crate) async fn maybe_auto_compact(
    history: &mut Vec<Message>,
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
) {
    let threshold = compact_threshold_pct();
    if threshold == 0 {
        return; // disabled
    }
    let (window, _) = resolve_ctx_window(model);
    let pct = session_tokens(history) as f64 / window as f64 * 100.0;
    if pct < threshold as f64 {
        return;
    }
    // The prefix cache is about to be invalidated anyway, so this is the one free moment to drop
    // the stale recall blocks accumulated on older user turns (see `strip_recall_blocks`).
    strip_recall_blocks(history);
    // tui::emit_line routes through the sticky footer when active, else prints a plain line.
    tui::emit_line(
        &style(format!(
            "↯ context {pct:.0}% ≥ {threshold}% — auto-compacting…"
        ))
        .dim()
        .to_string(),
    );
    match compact_history(history, http, base, key, model).await {
        Ok((b, a)) => tui::emit_line(
            &style(format!(
                "↯ auto-compacted: ~{} → ~{} tok",
                fmt_k(b),
                fmt_k(a)
            ))
            .color256(splash::ACCENT)
            .to_string(),
        ),
        Err(e) => tui::emit_line(&format!("{} {e}", style("auto-compact skipped:").dim())),
    }
}

/// Wrap a background / chore model call in the SAME per-call wall-clock deadline a sub-agent gets
/// ([`crate::agent::task_tool::subagent_call_timeout`], default 300s, `AIZEN_SUBAGENT_CALL_SECS`).
///
/// Every one of these is a NON-streaming `chat_with_tools` call — compaction / handoff summaries, the
/// end-of-turn secretary, persona reflection, memory reconcile, the oracle reviewer, persona distill.
/// None is streamed, so the streaming path's inter-event stall watchdog never applies; the shared
/// client carries no total-request ceiling (removed so a long *streamed* turn isn't cut — see
/// `http_client`); and `read_timeout` resets on every byte, so a gateway that keepalive-drips without
/// ever finishing the body parks the background task (or, for the by-hand ones, the CLI) forever. A
/// flat per-call cap is exactly right here — one call, one answer, no legitimate multi-minute stream.
/// On timeout it returns an ordinary `Err`, which every caller already treats as best-effort.
pub(crate) async fn chore_chat(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
    msgs: &[Message],
    tools: &[ToolDef],
) -> Result<client::ChatTurn> {
    let deadline = crate::agent::task_tool::subagent_call_timeout();
    match tokio::time::timeout(
        deadline,
        client::chat_with_tools(http, base, key, model, msgs, tools),
    )
    .await
    {
        Ok(r) => r,
        Err(_) => Err(anyhow::anyhow!(
            "chore model call exceeded {}s with no response (raise AIZEN_SUBAGENT_CALL_SECS)",
            deadline.as_secs()
        )),
    }
}

/// Is the `summarizer` role pointed at its OWN endpoint, or does it fall through to the main model?
///
/// Decides the secretary's input ceiling. When it falls through, every chore call bills the model
/// the user is actually coding with — on a large-context model that is the difference between a
/// chore and a real cost — so the transcript is capped much harder.
fn summarizer_is_dedicated(base: &str, key: &str, model: &str) -> bool {
    let ep = summarizer_endpoint(base, key, model);
    ep.model != model || ep.base_url != base
}

/// The end-of-turn secretary: ONE gated model call that files what the turn was worth.
///
/// Replaces `maybe_learn_memory` (regex extraction) + `maybe_learn_skill` (a second call) and folds
/// the persona episode in. Those two ran in OPPOSITE ORDERS in the retained and plain REPL loops,
/// so which of them saw the turn first depended on which loop you were in; one call cannot disagree
/// with itself.
///
/// Best-effort throughout: any failure means this turn taught nothing, never that the turn broke.
pub(crate) async fn maybe_run_secretary(
    history: &[Message],
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
) {
    use crate::memory::learning::secretary;

    if !memory_auto_learn_enabled() {
        return;
    }
    let start = match history.iter().rposition(|m| m.role == "user") {
        Some(i) => i,
        None => return,
    };
    let turn = &history[start..];

    // The user's ACTUAL words: history holds the folded message, so the recall block we injected
    // this turn has to come off first. Feeding it back would let the secretary re-emit a fact it was
    // just shown, and local reconciliation would read that as agreement.
    let user_text = turn
        .first()
        .and_then(|m| m.content.as_deref())
        .map(memory::strip_recall_prefix)
        .unwrap_or("")
        .trim()
        .to_string();
    if user_text.is_empty() {
        return;
    }
    // A turn that authored a CHARACTER was describing a fiction, not the user. Mining it leaks a
    // `persona-…` "fact" into user memory (it did, once — it polluted the verbosity profile).
    if memory::learning::turn_authored_persona(history) {
        return;
    }

    let tool_calls: usize = turn
        .iter()
        .filter(|m| m.role == "assistant")
        .map(|m| m.tool_calls.len())
        .sum();
    let reason = secretary::gate(&user_text, tool_calls, turn_recovered_from_dead_end(turn));
    if !reason.fires() {
        return; // the common case: no model call at all
    }

    // Show the secretary the handles it may cite, with the text each one stood for.
    let injected: Vec<(String, String)> = {
        let live = memory::pending::current();
        if live.is_empty() {
            Vec::new()
        } else {
            let all = memory::store::load_all().unwrap_or_default();
            live.iter()
                .filter_map(|p| {
                    all.iter()
                        .find(|e| e.id == p.id)
                        .map(|e| (p.handle.clone(), e.body.clone()))
                })
                .collect()
        }
    };
    let injected_ids: Vec<String> = memory::pending::current()
        .into_iter()
        .map(|p| p.id)
        .collect();

    // A signal-only turn gets the SHORT transcript regardless of configuration: the durable content
    // is in what the user said, and a tool log would crowd it out of the budget.
    let cap =
        if reason == secretary::GateReason::Signal || !summarizer_is_dedicated(base, key, model) {
            secretary::CAP_TOKENS_SHARED_MODEL
        } else {
            secretary::CAP_TOKENS_OWN_ROLE
        };
    let input = secretary::build_input(&user_text, &render_transcript(turn), &injected, cap);

    let ep = summarizer_endpoint(base, key, model);
    let msgs = [
        Message::system(secretary::system_prompt()),
        Message::user(input),
    ];
    // Counted before the call, not after: a call that errors was still billed, and the point of the
    // number is cost per turn. Counting only successes would understate exactly the spend the gate
    // exists to control.
    memory::stats::note_secretary_call();
    let resp = match chore_chat(http, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[]).await {
        Ok(t) => t,
        Err(_) => return, // best-effort; never disrupt the REPL
    };
    // `parse` never errors: garbage in yields an empty output, so a confused model costs one call.
    let out = secretary::parse(&resp.content.unwrap_or_default());

    // §8 metric 2 (injected-vs-used) is recorded HERE, before the empty-output early return: a gated
    // turn that was shown five facts and reported none of them useful is the single most informative
    // sample the ratio has. Dropping it would leave only the turns where recall worked, and the
    // metric would read high for exactly the store that needs fixing.
    //
    // Both halves come from one place so they cannot drift: the denominator is what the ledger
    // injected this turn, the numerator is the subset of THOSE handles the model cited (invented
    // handles resolve to nothing). Only gated turns are counted — an ungated turn was never asked.
    if !injected_ids.is_empty() {
        let used = memory::pending::resolve_used(&out.used).len() as u64;
        let shown = injected_ids.len() as u64;
        memory::stats::note_recall(shown, used);
        memory::learning::audit::recall(repl_session_id(), shown, used);
    }

    if out.is_empty() {
        return;
    }

    let report = secretary::apply_facts(&out, &injected_ids, repl_session_id());
    let confirmed_by_use = secretary::apply_used(&out);

    // Persona episode — CHARACTER only, and only when a character is actually active.
    if let Some(ep_prop) = out.episode.as_ref() {
        if persona_evolve_enabled() {
            if let Some(p) = persona::active() {
                let slug = skill::sanitize_name(&p.name);
                let _ = persona::self_mem::record_episode(&slug, &ep_prop.text, ep_prop.importance);
            }
        }
    }

    // Skill — save fresh, or fold into the existing one when the model asked to refine.
    // DEDUP (fix C, 2026-08-06): before auto-creating a NEW skill, compare the proposed `when` +
    // `steps` against every existing skill with `match_similarity`. Three identical "verify GitHub
    // Actions YAML" skills were observed on the same day because the only collision key was the slug,
    // and minor wording changes in the name produced different slugs. Now a new skill whose trigger
    // resembles an existing one gets routed to `refine` instead of spawning a duplicate.
    if let Some(sk) = out.skill.as_ref() {
        if auto_skill_learn_enabled() {
            use crate::memory::learning::match_text;
            let slug = skill::sanitize_name(&sk.name);
            let all_skills = skill::list();
            let exact_exists = all_skills
                .iter()
                .any(|s| skill::sanitize_name(&s.name) == slug);
            // Check if any existing skill has a semantically similar trigger.
            let similar_skill = if !exact_exists {
                all_skills.iter().find(|s| {
                    let trigger_sim = match_text::match_similarity(&sk.when, &s.when);
                    let body_sim = match_text::match_similarity(&sk.steps, &s.body);
                    // Both trigger AND body must resemble — trigger alone would merge skills that
                    // fire in the same situation but do different things.
                    trigger_sim >= 0.45 && body_sim >= 0.35
                })
            } else {
                None
            };
            let done = if exact_exists {
                // Only fold when the model MEANT to; otherwise a same-named skill is a collision to
                // leave alone, not a licence to overwrite the user's procedure.
                sk.refine && skill::refine(&sk.name, &sk.steps, None, Some(&sk.when)).is_ok()
            } else if let Some(existing) = similar_skill {
                // Similar enough to refine rather than duplicate. Route to the existing skill.
                skill::refine(&existing.name, &sk.steps, None, Some(&sk.when)).is_ok()
            } else {
                skill::save_scoped(&sk.name, "", &sk.when, &sk.steps, true).is_ok()
            };
            if done {
                let label = if exact_exists || similar_skill.is_some() {
                    "refined"
                } else {
                    "learned"
                };
                let display_name = similar_skill.map(|s| s.name.as_str()).unwrap_or(&sk.name);
                tui::emit_line(
                    &style(format!(
                        "{}{label} skill '{display_name}' — /skills to view",
                        icons::g(icons::learned()),
                    ))
                    .color256(splash::ACCENT)
                    .to_string(),
                );
            }
        }
    }

    let n_new = report.added.len();
    let n_conf = report.confirmed.len() + confirmed_by_use;
    let n_queue = report.queued_review.len();
    if n_new > 0 || n_conf > 0 || n_queue > 0 {
        let mut parts: Vec<String> = Vec::new();
        if n_new > 0 {
            parts.push(format!("remembered {n_new}"));
        }
        if n_conf > 0 {
            parts.push(format!("confirmed {n_conf}"));
        }
        if n_queue > 0 {
            parts.push(format!("{n_queue} to review"));
        }
        tui::emit_line(
            &style(format!(
                "{}{} — /memory to view",
                icons::g(icons::learned()),
                parts.join(", ")
            ))
            .color256(splash::ACCENT)
            .dim()
            .to_string(),
        );
    }
}

/// Did this turn RECOVER from a dead end — a tool result errored, then a LATER tool result in the
/// same turn succeeded? That recovery is a hard-won procedure worth distilling even on a short turn.
/// Tool errors are fed back as result strings starting with `error:` (the loop's convention).
pub(crate) fn turn_recovered_from_dead_end(turn: &[Message]) -> bool {
    let mut saw_error = false;
    for m in turn.iter().filter(|m| m.role == "tool") {
        let is_err = m
            .content
            .as_deref()
            .unwrap_or("")
            .trim_start()
            .to_ascii_lowercase()
            .starts_with("error:");
        if is_err {
            saw_error = true;
        } else if saw_error {
            return true; // a success after an earlier error → the agent worked through a dead end
        }
    }
    false
}

/// One stable session id for the whole REPL process, so per-turn auto-learn reinforces facts
/// across turns of ONE session (not a fresh "session" each turn, which would over-count
/// `session_count` and wrongly accelerate review/promotion).
fn repl_session_id() -> &'static str {
    use std::sync::OnceLock;
    static ID: OnceLock<String> = OnceLock::new();
    ID.get_or_init(crate::memory::learning::default_session_id)
}

pub(crate) fn memory_auto_learn_enabled() -> bool {
    cli_config::load().memory_auto_learn.unwrap_or(true)
}

/// After a completed turn: passively learn durable user/project facts from the user's last message.
/// FREE — regex extraction, no model call — through the SAME pipeline as `aizen memory learn`
/// (sanitize-to-fact → write-time threat-scan → confidence-route → consolidate → store, with
/// anti-bloat). Core promotion stays human-gated (`auto_confirm_core = Some(false)`): a would-be
/// core fact is downgraded to a normal store entry and NEVER silently mutates the always-on frozen
/// prefix (prefix-cache byte-stability is sacred). Best-effort + visible; never disrupts the REPL.
pub(crate) fn maybe_learn_memory(history: &[Message]) {
    use crate::memory::learning::{self, LearnOptions};
    if !memory_auto_learn_enabled() {
        return;
    }
    let user_text = match history
        .iter()
        .rev()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.clone())
    {
        Some(t) => t,
        None => return,
    };
    if user_text.trim().is_empty() {
        return;
    }
    // If THIS turn authored a character (the `persona_create` tool fired), the user's message was
    // describing a FICTIONAL persona, not stating their own preferences — mining it would leak a
    // `persona-…` "fact" into user memory. Skip learning for the whole turn. (The regex intent-gate
    // inside `ingest` is the first, heuristic line of defense; this fact-based gate catches phrasings
    // it misses. Lives as a unit-tested helper so this loop can't silently drop it in a refactor.)
    if learning::turn_authored_persona(history) {
        return;
    }
    let opts = LearnOptions {
        session_id: repl_session_id().to_string(),
        auto_confirm_core: Some(false), // never auto-mutate the frozen core; downgrade to store
        dry_run: false,
    };
    let report = match learning::ingest(&user_text, &opts) {
        Ok(r) => r,
        Err(_) => return, // best-effort; never disrupt the REPL
    };
    let n_durable = report.added.len() + report.reinforced.len();
    let n_session = report.session_notes.len();
    if n_durable > 0 {
        tui::emit_line(
            &style(format!(
                "{}remembered {n_durable} fact{} — /memory to view",
                icons::g(icons::learned()),
                if n_durable == 1 { "" } else { "s" }
            ))
            .color256(splash::ACCENT)
            .dim()
            .to_string(),
        );
    } else if n_session > 0 {
        // Inferred → session working memory only (not durable). Quiet, dim.
        tui::emit_line(
            &style(format!(
                "{}noted {n_session} for this session (not saved permanently)",
                icons::g(icons::learned()),
            ))
            .dim()
            .to_string(),
        );
    }
}

/// After a completed turn: if a persona is active, distill its accumulated episodes into durable
/// character insights when enough formative weight has piled up.
///
/// This used to also RECORD the turn's episode from a regex gate. That half moved into
/// [`maybe_run_secretary`], which already reads the finished turn — two writers meant one formative
/// moment landed twice, once as the gate's templated body and once in the model's own words. What
/// remains is the periodic tier: reflection is about the accumulation, not about this turn, so it
/// needs no `history` at all.
///
/// Best-effort + visible — never disrupts the REPL.
pub(crate) async fn maybe_evolve_persona(
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
) {
    if !persona_evolve_enabled() {
        return;
    }
    let persona = match persona::active() {
        Some(p) => p,
        None => return, // no character active → nothing to evolve
    };
    let slug = skill::sanitize_name(&persona.name);
    if persona::self_mem::should_reflect(&slug) {
        run_persona_reflection(&persona, &slug, http, base, key, model).await;
    }
}

/// The reflection call: synthesize recent episodes into 1-3 durable insights for this character.
async fn run_persona_reflection(
    persona: &persona::Persona,
    slug: &str,
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
) {
    let episodes = persona::self_mem::recent_episode_bodies(slug, 20);
    if episodes.len() < persona::self_mem::REFLECT_MIN_EPISODES {
        return;
    }
    let (sys, usr) =
        persona::reflect::build_reflection_prompt(&persona.name, &persona.role, &episodes);
    // Chore-class synthesis call → billed to the summarizer role, like every other harness chore.
    let ep = summarizer_endpoint(base, key, model);
    let resp = match chore_chat(
        http,
        &ep.base_url,
        &ep.api_key,
        &ep.model,
        &[Message::system(sys), Message::user(usr)],
        &[],
    )
    .await
    {
        Ok(t) => t,
        Err(_) => return, // best-effort; never disrupt the REPL
    };
    let content = resp.content.unwrap_or_default();
    let json = match extract_json_object(&content) {
        Some(j) => j,
        None => return,
    };
    let insights = persona::reflect::parse_insights(json);
    if insights.is_empty() {
        return;
    }
    let mut saved = 0usize;
    for ins in &insights {
        if let Ok(id) = persona::self_mem::save_insight(slug, &ins.text, ins.importance) {
            saved += 1;
            // Cross-kind Hebbian edge: an insight distilled while these facts were in play is
            // associated with them. Best-effort — a graph write never affects the reflection.
            persona::self_mem::note_insight_cofire(slug, &id);
        }
    }
    if saved > 0 {
        tui::emit_line(
            &style(format!(
                "{}{} reflected — +{saved} insight(s) from recent sessions (/persona to view)",
                icons::g(icons::learned()),
                persona.name
            ))
            .color256(splash::ACCENT)
            .to_string(),
        );
    }
}

/// Render conversation messages into a compact transcript (delegates to the shared compaction core).
pub(crate) fn render_transcript(msgs: &[Message]) -> String {
    agent::compact::render_transcript(msgs)
}

/// Summarize older turns to free context. Thin wrapper over [`agent::compact::compact_history`] that
/// supplies a NON-streaming summarize closure over this session's endpoint. Returns
/// (tokens_before, tokens_after). Same core the agent loop uses, so the REPL and `aizen serve` compact
/// identically.
pub(crate) async fn compact_history(
    history: &mut Vec<Message>,
    http: &reqwest::Client,
    base: &str,
    key: &str,
    model: &str,
) -> Result<(usize, usize)> {
    let sum_ep = summarizer_endpoint(base, key, model);
    let summarize = move |msgs: Vec<Message>| {
        let ep = sum_ep.clone();
        async move {
            chore_chat(http, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[])
                .await
                .map(|t| t.content.unwrap_or_default())
        }
    };
    agent::compact::compact_history(history, summarize, COMPACT_KEEP_TURNS).await
}

/// `/compact` — resolve the endpoint, then summarize older turns now (manual compaction).
pub(crate) async fn compact_now(history: &mut Vec<Message>) -> Result<(usize, usize)> {
    let (base, key, model) = resolve_endpoint(None, None, None)?;
    let http = http_client()?;
    compact_history(history, &http, &base, &key, &model).await
}

/// `/handoff` — one goal-conditioned extraction call over the current history (routed through the
/// summarizer role, like compaction). Returns the extraction; the caller rebuilds the thread.
pub(crate) async fn handoff_now(history: &[Message], goal: &str) -> Result<String> {
    let (base, key, model) = resolve_endpoint(None, None, None)?;
    let http = http_client()?;
    if history.len() < 2 {
        anyhow::bail!("nothing to hand off yet — the conversation is empty");
    }
    let ep = summarizer_endpoint(&base, &key, &model);
    let prompt = agent::compact::handoff_prompt(history, goal);
    let summary = chore_chat(&http, &ep.base_url, &ep.api_key, &ep.model, &prompt, &[])
        .await?
        .content
        .unwrap_or_default();
    if summary.trim().is_empty() {
        anyhow::bail!("the model returned an empty handoff summary");
    }
    Ok(summary.trim().to_string())
}
