//! One agent turn, shared verbatim by both REPL surfaces.
//!
//! The two loops differ in exactly two things — how a line arrives, and whether Esc can race the
//! model call. Everything else was written out twice and drifted, so it lives here once: building
//! the turn's tool registry, seating the user message, running the turn, and finishing it.

use crate::agent::prompt_lanes::{
    fold_context_into_query, refresh_dynamic_prompt_lane, update_system_prompt,
};
use crate::agent::{self, AgentConfig, AgentOutcome, StopReason};
use crate::core::session_store::{
    autosave_last, autosave_session, publish_live_history, update_live_history,
};
use crate::core::types::ToolDef;
use crate::core::{cli_config, types};
use crate::llm::client;
use crate::repl::postturn::{
    chore_chat, maybe_auto_compact, maybe_evolve_persona, maybe_run_secretary,
};
use crate::ui::context_report::resolve_ctx_window;
use crate::ui::image_input;
use crate::ui::{splash, theme, tui};
use crate::{
    approval_mode, cancellable_slash, cancellable_slash_labeled, eager_enabled, summarizer_endpoint,
};
use anyhow::Result;
use console::style;
use types::Message;

/// Pull drag-dropped / pasted image paths out of a typed line, leaving the prose behind.
///
/// The other half of Ctrl-O clipboard attach: only real image files are lifted, so a message that
/// merely mentions a path keeps its text intact.
pub(crate) fn lift_image_attachments(line: &mut String, images: &mut Vec<String>) {
    if line.is_empty() {
        return;
    }
    let (cleaned, from_line) = image_input::extract_image_attachments(line);
    if !from_line.is_empty() {
        images.extend(from_line);
        *line = cleaned;
    }
}

/// Build this turn's tool registry against the resolved endpoint.
pub(crate) fn build_turn_registry(
    http: &reqwest::Client,
    ep: &cli_config::ResolvedEndpoint,
) -> Result<agent::tools::ToolRegistry> {
    agent::builtin::default_registry_with_task(
        http.clone(),
        ep.base_url.clone(),
        ep.api_key.clone(),
        ep.model.clone(),
        approval_mode(),
        resolve_ctx_window(&ep.model).0,
        None, // cwd IS the project in the REPL
    )
}

/// The agent config for one interactive turn.
///
/// `enable_steering` is the only thing the two surfaces disagree on: the retained REPL has a
/// mailbox the user can type into mid-turn, the plain one does not. Everything else — approval
/// mode, context window, self-review, LSP state, goal mode, the mid-turn snapshot — is a reading of
/// live config that must not be allowed to differ between them, which is why it is written once.
pub(crate) fn turn_agent_config(
    cancel: crate::core::cancel::TurnCancel,
    model: &str,
    enable_steering: bool,
) -> AgentConfig {
    AgentConfig {
        approval_mode: approval_mode(),
        cancel,
        context_window: resolve_ctx_window(model).0,
        enable_self_review: cli_config::self_review_enabled(&cli_config::load()),
        // Reflect the live manager state (honours `/lsp off` for this turn).
        enable_lsp: crate::agent::lsp::LSP.is_enabled(),
        // Goal mode (set by `/goal <text>`): threads the live goal into this turn so the loop runs
        // cap-free with smart retry until the goal is declared and verified.
        goal: crate::agent::goal::current_goal(),
        // Only the interactive top-level turn reads the steering mailbox — a course correction the
        // user typed is aimed at THIS task, not at whatever a delegated sub-agent is doing.
        enable_steering,
        // Keep the exit-flush snapshot current DURING the turn, not just at its edges.
        on_progress: Some(publish_live_history),
        // The user is sitting here waiting: retry a 429/5xx blip many times (like the Claude CLI)
        // with FAST backoff (`interactive_backoff_ms`), so the whole chain still fits in ~30–40s and
        // then reports a clear error. 10 is a CEILING, not a cost — a gateway that recovers on try 2
        // stops at 2. `/goal` does not take this branch (goal mode retries transient errors
        // indefinitely, see agent/mod.rs), so raising this never shortens a goal run.
        max_transient_retries: 10,
        ..AgentConfig::default()
    }
}

/// Fold this turn's retrieved context into the outgoing message and seat it in history.
///
/// `line` stays the clean text the user typed — the fold only affects what is SENT, so the
/// checkpoint, the display and the persisted transcript all keep the original. The dynamic prompt
/// lane is refreshed AFTER the fold, never before: recall seats this turn's handle→id ledger and the
/// `<skills>` lane ranks itself by affinity to exactly those facts, so refreshing first would build
/// the index against the PREVIOUS turn's recall.
pub(crate) fn seat_user_message(
    line: &str,
    images: Vec<String>,
    history: &mut Vec<Message>,
    model: &str,
) {
    let sent = fold_context_into_query(line);
    refresh_dynamic_prompt_lane(history, model);
    if images.is_empty() {
        history.push(Message::user(sent));
    } else {
        tui::emit_line(
            &style(format!("📎 {} image(s) attached", images.len()))
                .color256(splash::ACCENT)
                .to_string(),
        );
        history.push(Message::user_with_images(sent, images));
    }
    // Refresh the exit-flush snapshot the moment the user turn lands, so an abrupt window close
    // mid-turn still persists the question (the per-turn autosave only runs on success).
    update_live_history(history);
}

// ── the shared turn: everything both REPL surfaces do identically ─────────────────────────────
// `run_menu_sticky` and `run_menu_plain` differ in exactly two things — how a line arrives, and
// whether Esc can race the model call. Everything else about a turn is the same work, and it used
// to be written out twice. It drifted: the plain loop once ran the skill, persona and memory passes
// in a different order than the retained one, and to this day it was the copy that never learned
// about goal-mode completion, the post-turn timeout ceiling, or the recovery checkpoints. The two
// functions below are the parts that were always meant to be one thing.
//
// They are surface-agnostic because `tui::emit_line` already is: it renders through the retained
// backend when one is running and prints append-only when none is, so the same call reads correctly
// on both. Nothing here may use `println!` — see `tui::note_line`.

/// Run one agent turn against `ep`, with the model wiring both REPLs were building by hand.
///
/// The three closures (stream the turn, summarize for mid-loop compaction, optionally consult the
/// `oracle` role for self-review) are pure functions of the endpoint, so there was never a reason
/// for two copies. The caller keeps ownership of cancellation: the retained REPL races this future
/// against Esc, the plain one simply awaits it.
pub(crate) async fn run_agent_turn(
    http: &reqwest::Client,
    ep: &cli_config::ResolvedEndpoint,
    cfg: &AgentConfig,
    registry: &agent::tools::ToolRegistry,
    history: &mut Vec<Message>,
) -> Result<AgentOutcome> {
    let base = ep.base_url.as_str();
    let key = ep.api_key.as_str();
    let model = ep.model.as_str();
    let eager_on = eager_enabled();
    let chat = move |msgs: Vec<Message>, defs: Vec<ToolDef>| async move {
        if eager_on {
            // Read-only calls start the moment their streamed args complete.
            let starter = agent::eager_starter(registry, cfg);
            client::stream_chat_with_tools_eager(
                http,
                base,
                key,
                model,
                &msgs,
                &defs,
                Some(&starter),
            )
            .await
        } else {
            client::stream_chat_with_tools(http, base, key, model, &msgs, &defs).await
        }
    };
    // Non-streaming summarizer for mid-loop auto-compaction (keeps the streamed display clean).
    let sum_ep = summarizer_endpoint(base, key, model);
    let summarize = move |msgs: Vec<Message>| {
        let ep = sum_ep.clone();
        async move {
            chore_chat(http, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[])
                .await
                .map(|t| t.content.unwrap_or_default())
        }
    };
    // Optional oracle for self-review: only when `roles.oracle` names a stronger reviewer model;
    // otherwise the loop falls back to nudge-mode.
    let oracle = cli_config::role_configured("oracle")
        .then(|| cli_config::resolve_role("oracle", ep))
        .map(|role| {
            move |msgs: Vec<Message>| {
                let role = role.clone();
                async move {
                    chore_chat(http, &role.base_url, &role.api_key, &role.model, &msgs, &[])
                        .await
                        .map(|t| t.content.unwrap_or_default())
                }
            }
        });
    agent::run_agent_loop_full(chat, summarize, oracle, cfg, registry, history).await
}

/// How long the post-turn learning passes may take in total before the REPL gives up on them.
///
/// Each call already has its own 300s ceiling (`chore_chat` → `subagent_call_timeout`), but three of
/// them in a row can strand an idle-looking REPL for fifteen minutes. On timeout the user sees a
/// skip line instead of a spinner that never stops.
const POST_TURN_OVERALL_TIMEOUT_SECS: u64 = 600;

/// Everything a turn that reached the model must do afterwards, on either surface.
///
/// Ordering is load-bearing and was the thing that drifted: the learning passes read the FULL detail
/// of the turn, so they must run before auto-compaction summarizes it away, and persistence must
/// happen last and unconditionally — a cancelled learning pass is not a reason to lose the
/// transcript.
pub(crate) async fn finish_turn(
    outcome: &AgentOutcome,
    persona_before: Option<String>,
    history: &mut Vec<Message>,
    http: &reqwest::Client,
    ep: &cli_config::ResolvedEndpoint,
) {
    // ABNORMAL STOP, SAID OUT LOUD. The loop can end for reasons that are NOT success — the repair
    // budget ran out with the tree still broken, the step cap was hit mid-task, the model started
    // repeating itself — and in each case it has usually already streamed a confident closing
    // paragraph. Silence here makes those read exactly like `Done`, and the passes below would then
    // file a red tree as a finished task.
    surface_abnormal_stop(outcome);
    // Goal mode finishes only on a verify-passing `Done`. Clear it here so the next turn is an
    // ordinary capped turn again; Esc leaves the goal armed on purpose, so the user can retry.
    if crate::agent::goal::current_goal().is_some() && matches!(outcome.stop, StopReason::Done) {
        crate::agent::goal::set_goal(None);
        crate::agent::goal::arm(false);
        crate::agent::goal::clear();
        tui::emit_line(
            &style("🎯 goal complete — verified. goal mode off.")
                .color256(splash::ACCENT)
                .to_string(),
        );
    }
    // An EMPTY answer from a SINGLE model call (no tool work, no streamed text) used to vanish
    // silently — a rate limit swallowed into an empty 200, a content filter, or a gateway that
    // streams `[DONE]` with no deltas looked identical to "still idle". `iters <= 1` keeps a turn
    // that DID do tool work and merely ended without a closing sentence from being flagged.
    let empty = outcome
        .final_text
        .as_deref()
        .map(str::trim)
        .unwrap_or("")
        .is_empty();
    if empty && outcome.iters <= 1 {
        tui::emit_line(&format!(
            "{} the model returned an empty response — no text and no tool calls. Likely a rate limit, content filter, or a gateway that closed the stream early. Try again, or /model to switch.",
            theme::warn("⚠ empty reply:")
        ));
    }
    // The agent may have created or switched personas mid-turn (the `persona_create` tool). Resync
    // the system prompt at the turn boundary so the new character is live from the next message —
    // prefix-cache safe, because index 1 is rewritten between turns rather than during one.
    let persona_after = cli_config::load().persona;
    if persona_after != persona_before {
        update_system_prompt(history, &ep.model);
        if let Some(name) = persona_after {
            tui::emit_line(
                &style(format!("🎭 now playing: {name} (from your next message)"))
                    .color256(splash::ACCENT)
                    .to_string(),
            );
        }
    }
    // The learning passes are model calls made after the turn's own token was disarmed, so without
    // re-arming, Esc would take the idle branch while the REPL sat awaiting them: to the user the
    // turn had visibly ended and the app was wedged anyway. Cancelling here skips the remaining
    // learning, which is always optional work.
    let learning = cancellable_slash_labeled("learning from this turn…", async {
        maybe_run_secretary(history, http, &ep.base_url, &ep.api_key, &ep.model).await;
        maybe_evolve_persona(http, &ep.base_url, &ep.api_key, &ep.model).await;
        maybe_auto_compact(history, http, &ep.base_url, &ep.api_key, &ep.model).await;
    });
    let learned = match tokio::time::timeout(
        std::time::Duration::from_secs(POST_TURN_OVERALL_TIMEOUT_SECS),
        learning,
    )
    .await
    {
        Ok(result) => result,
        Err(_) => {
            tui::emit_line(
                &theme::muted("⏱ post-turn learning exceeded timeout — skipped.").to_string(),
            );
            None
        }
    };
    if learned.is_none() {
        tui::emit_line(&theme::muted("⏹ skipped the post-turn learning passes.").to_string());
    }
    // Persistence is NOT optional, so it sits outside that block: a cancelled learning pass must
    // still leave the conversation on disk. `autosave_session` names the session with a model call,
    // so it is cancellable too — the local-only writer keeps the transcript either way.
    if cancellable_slash(autosave_session(
        history,
        http,
        &ep.base_url,
        &ep.api_key,
        &ep.model,
    ))
    .await
    .is_none()
    {
        autosave_last(history, Some(&ep.model));
    }
}

/// Tell the user, unmistakably, when a turn ended for a reason that is NOT success.
///
/// The agent loop can return with the work unfinished or the tree broken, and in those cases the
/// model has usually ALREADY streamed a confident closing paragraph — so silence here means the
/// failure is indistinguishable from `Done`, and the post-turn passes go on to file it as a normal
/// episode and store it as a normal session. That is the one failure mode worth spending screen
/// space on: a wrong answer the user has no reason to doubt.
///
/// Each line names the recovery move, because the state differs: `VerificationFailed` means edits
/// LANDED and the checker never went green (so the tree is the thing to look at), while `MaxIters`
/// and `Divergence` mean the work simply stopped short (so continuing is the move). `Done` prints
/// nothing — the answer already speaks for itself. `Cancelled` / `AwaitingInput` never reach here:
/// their callers own dedicated arms upstream.
fn surface_abnormal_stop(outcome: &AgentOutcome) {
    let line = match &outcome.stop {
        StopReason::Done => return,
        StopReason::VerificationFailed => format!(
            "⚠ edits were made but verification never passed ({} steps). The tree is likely broken \
             — `/diff` to see what changed, `/rewind` to undo, or tell me to keep fixing.",
            outcome.iters
        ),
        // Reaching here now means the loop ALREADY granted itself every continuation it was allowed
        // (see `AgentConfig::max_continuations`) — so this is a genuinely long task, not the old
        // "cut off at step 50" case. Say that, rather than implying one more nudge would have done it.
        StopReason::MaxIters => format!(
            "⚠ ran out of step budget after {} steps, including the automatic continuations — the \
             task may be incomplete. Say \"continue\" to carry on from here.",
            outcome.iters
        ),
        // Both signature loops and evidence-flat exploration reach here. The final synthesis above
        // has already returned the best answer available; this line states why tool use stopped.
        StopReason::Divergence => format!(
            "⚠ stopped after {} steps: recent attempts added no new evidence. The answer above is the \
             best result from the established facts; say \"continue\" to try a different approach.",
            outcome.iters
        ),
        // Both have dedicated arms in every caller (Esc / `clarify` pause), so reaching this is a
        // wiring slip rather than a real state — still say something instead of swallowing it.
        StopReason::Cancelled => format!("⚠ stopped: cancelled after {} step(s).", outcome.iters),
        StopReason::AwaitingInput(q) => format!("❓ {q}"),
        // Only reachable if a wall-clock budget was set on this run (no top-level default), so name
        // the knob — otherwise the user cannot tell a deadline from a step limit or a crash.
        StopReason::Deadline => format!(
            "⚠ stopped: wall-clock budget reached after {} step(s) — the task may be incomplete. \
             Say \"continue\" to carry on, or raise AIZEN_SUBAGENT_WALL_SECS.",
            outcome.iters
        ),
    };
    let painted = theme::err(line).to_string();
    if tui::active() {
        tui::emit_line(&painted);
    } else {
        eprintln!("{painted}");
    }
}

/// Render a `clarify` question prominently and yield to the input box. `display` is the tool's
/// stored text: the question on the first line, any numbered options on the following lines.
/// Routes through `tui::emit_line` under the sticky TUI, else plain stdout — so the user just types
/// their answer next (it becomes the agent's next user turn). The dim `↳` hint sits below.
pub(crate) fn show_clarify(display: &str) {
    let mut lines = display.lines();
    let q = lines.next().unwrap_or("");
    let head = format!(
        "{} {}",
        style("❓").color256(splash::ACCENT).bold(),
        style(q).bold()
    );
    let opts: Vec<String> = lines
        .map(|l| style(l).color256(splash::ACCENT).to_string())
        .collect();
    let hint = style("↳ type your answer below to continue")
        .dim()
        .to_string();
    if tui::active() {
        tui::emit_line(&head);
        for o in &opts {
            tui::emit_line(o);
        }
        tui::emit_line(&hint);
    } else {
        println!("{head}");
        for o in &opts {
            println!("{o}");
        }
        println!("{hint}");
    }
}
