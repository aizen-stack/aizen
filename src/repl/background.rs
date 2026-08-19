//! Work the REPL starts and then forgets about: the endpoint health chip, the memory reconcile
//! sweep, and the aside worker that answers `?` questions while a turn is running.
//!
//! Every one of these is best-effort and must never block or fail a turn. They own their own
//! short-timeout HTTP client precisely so a dead endpoint degrades a chip instead of a request.

use crate::core::endpoint::{http_client, resolve_base_key, resolve_endpoint};
use crate::core::session_store::live_history_slot;
use crate::core::types;
use crate::llm::client;
use crate::memory;
use crate::repl::postturn::{chore_chat, memory_auto_learn_enabled};
use crate::summarizer_endpoint;
use crate::ui::{theme, tui};
use anyhow::{Context, Result};
use console::style;
use types::Message;

/// Short-timeout client for the health probe only — a dead endpoint must fail the chip fast, not
/// wait out the chat client's 300s read timeout. Connect + total request each capped at 4s.
fn health_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION"), " health"))
        .connect_timeout(std::time::Duration::from_secs(4))
        .timeout(std::time::Duration::from_secs(4))
        .build()
        .context("building health HTTP client")
}

/// How often the idle `●` chip re-probes the provider. Confirmed: 60s.
const HEALTH_POLL_SECS: u64 = 60;
/// A successful `GET /models` slower than this is painted yellow (unstable). Confirmed: 2s.
pub(crate) const HEALTH_SLOW_MS: u128 = 2_000;

/// Classify a single probe outcome into the idle-chip colour. Pure so it can be unit-tested
/// without a network. Rules (user-confirmed):
/// - Ok + latency ≤ 2s → green (`Ok`)
/// - Ok + latency > 2s → yellow (`Unstable`)
/// - Err classified Transient (429/5xx/timeout/transport) → yellow (`Unstable`)
/// - Err classified Permanent (400/401/403/404) → red (`Down`)
/// - Missing config (no base/key) is treated as Permanent → red
pub(crate) fn classify_health_probe(
    result: Result<std::time::Duration, anyhow::Error>,
) -> tui::HealthKind {
    match result {
        Ok(latency) if latency.as_millis() > HEALTH_SLOW_MS => tui::HealthKind::Unstable,
        Ok(_) => tui::HealthKind::Ok,
        Err(e) => match client::classify_api_error(&e) {
            client::ApiErrorKind::Permanent => tui::HealthKind::Down,
            client::ApiErrorKind::Transient => tui::HealthKind::Unstable,
            // A probe request can't overflow a context window, but the arm must exist; a 413
            // from a health probe says the endpoint is reachable and objecting — yellow, not red.
            client::ApiErrorKind::ContextOverflow => tui::HealthKind::Unstable,
        },
    }
}

/// Spawn the once-per-session batch reconciliation (M2b), off the hot path.
///
/// Three properties make an automatic pass that RETIRES facts acceptable here:
///
/// - **It fires rarely.** `should_run` gates on ≥8 waiting pairs or ≥7 days since the last pass, so a
///   store with nothing to resolve never pays a call.
/// - **It cannot run twice.** `batch_pass` takes the judge as `FnOnce`, and this task is spawned once
///   per REPL start, so "≤1 model call per session" is structural rather than remembered.
/// - **Everything it does is reversible.** Retirement is `supersedes:` + `revive`, never a delete, and
///   the summary line names what changed so the user can see it happened at all — a silent pass that
///   rewrites memory is the thing this design refuses.
///
/// Fully best-effort: any failure leaves the store exactly as it was and says nothing.
pub(crate) fn spawn_reconcile_pass() {
    tokio::spawn(async move {
        if !memory_auto_learn_enabled() {
            return; // the same switch that governs learning governs correcting
        }
        let Ok((pairs, live)) = memory::reconcile_inputs() else {
            return;
        };
        let today = memory::bloat::decay::today();
        if !memory::learning::reconcile::should_run(
            pairs.len(),
            memory::learning::reconcile::last_run().as_deref(),
            &today,
        ) {
            return;
        }
        let Ok((base, key, model)) = resolve_endpoint(None, None, None) else {
            return;
        };
        let Ok(http) = http_client() else { return };
        let ep = summarizer_endpoint(&base, &key, &model);
        let judge = |sys: &str, user: &str| -> Option<String> {
            let msgs = [
                Message::system(sys.to_string()),
                Message::user(user.to_string()),
            ];
            let fut = chore_chat(&http, &ep.base_url, &ep.api_key, &ep.model, &msgs, &[]);
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(fut)
                    .ok()?
                    .content
            })
        };
        let report = memory::learning::reconcile::batch_pass(
            &pairs,
            judge,
            false, // this path APPLIES; the CLI is the dry-run surface
            &memory::learning::default_session_id(),
            &live,
        );
        // One line, and only when something actually changed. A background pass that narrates itself
        // every session is noise; one that changes memory in silence is worse.
        let acted = report
            .applied
            .iter()
            .filter(|a| !matches!(a.action, memory::learning::reconcile::Action::Review { .. }))
            .count();
        // Retirements are counted separately: "reconciled 3 facts" reads like bookkeeping, but a row
        // leaving the active view is the change a user would want to know about, and it is the half
        // that needs the undo hint. Only removals that reported no failure are counted.
        let dropped = report
            .applied
            .iter()
            .filter(|a| {
                matches!(
                    &a.action,
                    memory::learning::reconcile::Action::Confirm {
                        redundant: Some(_),
                        ..
                    }
                ) && !a.note.contains("kept")
            })
            .count();
        if acted > 0 {
            let what = if dropped > 0 {
                format!("reconciled {acted} memory fact(s), retiring {dropped} duplicate(s)")
            } else {
                format!("reconciled {acted} memory fact(s)")
            };
            tui::emit_line(
                &style(format!(
                    "⚖ {what} — `aizen memory list --superseded` to review, `revive <id>` to undo"
                ))
                .dim()
                .to_string(),
            );
        }
    });
}

/// Probe the newly selected provider immediately instead of waiting for the next 60-second poll tick.
pub(crate) fn spawn_health_probe_once() {
    tokio::spawn(async move {
        let kind = match (health_http_client(), resolve_base_key(None, None)) {
            (Ok(http), Ok((base, key))) => {
                let t0 = std::time::Instant::now();
                classify_health_probe(
                    client::probe_models(&http, &base, &key)
                        .await
                        .map(|_| t0.elapsed()),
                )
            }
            _ => tui::HealthKind::Down,
        };
        tui::set_health(kind);
    });
}

/// Spawn a background task that paints the idle `● ready` chip from a real `GET /models` probe.
/// Runs once immediately, then every [`HEALTH_POLL_SECS`]. Lives for the process (the REPL owns
/// the runtime); each tick re-resolves base_url/api_key so a mid-session `/config` takes effect
/// without a restart. Failures never surface as text — only as the chip colour.
pub(crate) fn spawn_health_poller() {
    tokio::spawn(async move {
        let http = match health_http_client() {
            Ok(c) => c,
            Err(_) => {
                tui::set_health(tui::HealthKind::Down);
                return;
            }
        };
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(HEALTH_POLL_SECS));
        // The first tick completes immediately (tokio interval behaviour) → first probe is eager.
        loop {
            interval.tick().await;
            let kind = match resolve_base_key(None, None) {
                Ok((base, key)) => {
                    let t0 = std::time::Instant::now();
                    let result = client::probe_models(&http, &base, &key)
                        .await
                        .map(|_| t0.elapsed());
                    classify_health_probe(result)
                }
                // Not configured yet → permanent unavailability until /config. Don't lean on
                // classify_api_error (which would paint yellow for a message without an HTTP code).
                Err(_) => tui::HealthKind::Down,
            };
            tui::set_health(kind);
        }
    });
}

/// Spawn the long-lived off-to-the-side Q&A worker. It owns an unbounded channel (armed into
/// `core::aside`) and answers `?`-prefixed questions one at a time, WITHOUT touching the turn in
/// flight: it clones the read-only live-conversation snapshot, makes ONE tool-less model call, and
/// prints the answer through `tui::emit_line` (which the retained renderer serializes with the main
/// stream on its single render thread, so a mid-turn aside can never corrupt the frame). It never
/// mutates `history`, never arms cancel, never flips `WORKING` — the running turn is oblivious.
///
/// Errors are shown inline and swallowed: a failed side question must never take down the worker
/// (which would silently disable the feature for the rest of the session) nor the REPL.
pub(crate) fn spawn_aside_worker(http: reqwest::Client) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    crate::core::aside::arm(tx);
    tokio::spawn(async move {
        while let Some(question) = rx.recv().await {
            // Resolve the endpoint fresh per question: the user may have switched models with
            // `/model` since the worker was spawned, and an aside should follow that choice.
            let (base_url, api_key, model) = match resolve_endpoint(None, None, None) {
                Ok(t) => t,
                Err(_) => {
                    tui::emit_line(
                        &style("  ⁇ side question skipped — no model configured (/config).")
                            .dim()
                            .to_string(),
                    );
                    continue;
                }
            };
            // Read-only snapshot of the live conversation (kept current DURING the turn via
            // `on_progress`); cloned so we never hold the lock across the await.
            let snapshot = live_history_slot()
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            let msgs = crate::core::aside::build_messages(&snapshot, &question);
            // Echo the question so the answer has a visible anchor in the transcript (dim, with a
            // `⁇` glyph, so it reads as an out-of-band aside distinct from a `❯` user turn).
            tui::emit_line(
                &style(format!("  ⁇ {question}"))
                    .color256(theme::MUTED)
                    .to_string(),
            );
            // ONE tool-less, non-streaming call. Empty tool slice ⇒ no tools offered.
            //
            // Wrapped in the SAME per-call deadline a sub-agent gets (`subagent_call_timeout`,
            // default 300s, `AIZEN_SUBAGENT_CALL_SECS`): the shared client carries no total-request
            // ceiling (removing that is what unblocks a legitimately long streamed turn — see
            // `http_client`), and `chat_with_tools` reads its body with `.json().await`, outside any
            // deadline. `read_timeout` resets on every byte, so a gateway that keepalive-drips
            // without ever finishing the body would park this worker forever, silently killing the
            // aside feature for the rest of the session. This is not a streamed answer, so a flat
            // per-call cap is exactly right — no inter-event watchdog applies here.
            let call = client::chat_with_tools(&http, &base_url, &api_key, &model, &msgs, &[]);
            let deadline = crate::agent::task_tool::subagent_call_timeout();
            let outcome = match tokio::time::timeout(deadline, call).await {
                Ok(r) => r,
                Err(_) => Err(anyhow::anyhow!(
                    "side question timed out after {}s with no response",
                    deadline.as_secs()
                )),
            };
            match outcome {
                Ok(turn) => {
                    let answer = turn.content.unwrap_or_default();
                    if answer.trim().is_empty() {
                        tui::emit_line(&style("  ⁇ (no answer)").dim().to_string());
                    } else {
                        let shown = crate::ui::markdown::render_plain_blocks(answer.trim());
                        // Prefix every line dimly so the whole aside block reads as a margin note
                        // beside the main work, not as the model's task output.
                        for line in shown.lines() {
                            tui::emit_line(
                                &style(format!("  {line}"))
                                    .color256(theme::MUTED)
                                    .to_string(),
                            );
                        }
                    }
                }
                Err(e) => {
                    tui::emit_line(
                        &style(format!("  ⁇ side question failed: {e}"))
                            .color256(theme::WARN)
                            .to_string(),
                    );
                }
            }
        }
    });
}
