//! `goal_complete` — the DONE-signal tool for goal mode (`/goal <text>`).
//!
//! Goal mode makes the agent run until the goal is genuinely finished — no iteration cap, and
//! transient API failures (429/5xx/timeouts/empty-200) auto-retry with backoff instead of killing
//! the loop. The one thing the agent can't decide unilaterally is *when it's done*: a model that
//! just stops emitting tool calls has historically been treated as "done", which in goal mode is
//! premature. So completion is a two-key handshake — the model must EXPLICITLY call
//! `goal_complete` (this tool) AND the existing verify gate must pass. Only then does the loop
//! return `StopReason::Done`.
//!
//! Mechanism mirrors [`crate::agent::clarify`]: a control-flow tool with a process-global cell.
//! `execute` records the model's completion summary in `PENDING`; the agent loop's goal gate drains
//! it via [`take_pending`] the same turn. A turn that stops WITHOUT this pending set is poked to keep
//! working (the goal isn't done); a turn WITH it set is allowed into the verify gate, which has the
//! final say. Registered top-level-only and ONLY when goal mode is armed (see `builtin.rs`), so the
//! ordinary chat/agent modes never see the extra tool schema.

use crate::agent::tools::Tool;
use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

/// The tool's advertised name — also the key the agent loop's goal gate uses to decide whether the
/// model has declared completion this turn (so a turn that never called it can't drain a stale value).
pub const NAME: &str = "goal_complete";

/// Process-global "goal mode is armed" flag. The tool registry is built once per top-level turn by
/// `default_registry_in`, which takes NO `cfg` — so, exactly like `LSP.is_enabled()`, the registry
/// consults this flag to decide whether to advertise the `goal_complete` tool. Set by the `/goal`
/// slash handler (mirrors `AgentConfig.goal` being `Some`); cleared when goal mode turns off or the
/// goal finishes. Ordinary chat/agent turns leave it `false` and never see the tool schema.
static ARMED: AtomicBool = AtomicBool::new(false);

/// Arm or disarm goal mode. Called by the `/goal` slash handler alongside setting `AgentConfig.goal`.
pub fn arm(on: bool) {
    ARMED.store(on, Ordering::SeqCst);
}

/// Is goal mode armed? Consulted by `default_registry_in` to gate the `goal_complete` tool.
pub fn is_armed() -> bool {
    ARMED.load(Ordering::SeqCst)
}

/// The model's outstanding completion claim, set by `GoalComplete::execute` and drained by the agent
/// loop (`take_pending`) the same turn. `None` whenever no completion has been declared. A turn that
/// contains `goal_complete` runs serially (not concurrency-safe), so there is never a race here.
static PENDING: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));

/// Take (and clear) the pending completion claim, if any. The agent loop calls this when the model
/// stops emitting tool calls; `Some` means "the model declared the goal done this turn → let the
/// verify gate have the final say instead of poking it to keep working".
pub fn take_pending() -> Option<String> {
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).take()
}

/// Non-draining check: is a completion claim currently pending? Used by the goal-mode retry loop's
/// empty-200 detector to tell "the API returned a garbage empty response" (retry) apart from "the
/// model legitimately stopped after declaring completion last turn" (let it flow to the goal gate,
/// which drains the claim via [`take_pending`]). Peeking here must NOT drain, or the goal gate would
/// then see `None` and wrongly poke a model that already finished.
pub fn is_pending() -> bool {
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).is_some()
}

fn set_pending(summary: String) {
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some(summary);
}

/// Clear any pending completion without reading it. Called when goal mode is (re)armed so a stale
/// claim from an earlier goal can't leak into the next one.
pub fn clear() {
    *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// The active goal's description text, mirrored into `AgentConfig.goal` when the REPL builds each
/// turn's config. Set by the `/goal <text>` slash handler alongside [`arm`]`(true)`; cleared on
/// `/goal off` or when the goal finishes (verify-passing `Done`) or is cancelled (Esc). Kept as a
/// process-global companion to `ARMED` so the goal survives across REPL turns without threading a
/// new parameter through every slash-handler call site.
static GOAL_TEXT: Lazy<Mutex<Option<String>>> = Lazy::new(|| Mutex::new(None));

/// Set (or clear) the active goal description. `Some(text)` when a goal is armed; `None` when off.
/// Pair with [`arm`] so `is_armed()` (tool gate) and `current_goal()` (loop gate) stay consistent.
pub fn set_goal(text: Option<String>) {
    *GOAL_TEXT.lock().unwrap_or_else(|e| e.into_inner()) = text;
}

/// The active goal description, if goal mode is on. Read by the REPL when building each turn's
/// `AgentConfig` so `cfg.goal` reflects the live goal across turns.
pub fn current_goal() -> Option<String> {
    GOAL_TEXT.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Serializes every test (here AND in the agent-loop module) that touches the process-global
/// `PENDING` — cargo runs tests in parallel, so without this a concurrent set/take would interleave.
#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

/// The model-facing acknowledgement: confirm the claim was recorded and tell the model to STOP
/// calling tools so the verify gate can run. Pure — no global state — so tests can exercise it
/// without touching `PENDING`.
fn build_ack(summary: &str) -> String {
    let s = summary.trim();
    format!(
        "Completion recorded: \"{s}\". STOP now — do NOT call any more tools. Your work will be \
         verified automatically (typecheck/tests). If verification passes, the goal is done; if it \
         fails, you'll get the errors and must fix them, then call goal_complete again."
    )
}

pub struct GoalComplete;

impl Tool for GoalComplete {
    fn name(&self) -> &str {
        NAME
    }

    fn description(&self) -> &str {
        "Declare the current GOAL fully complete. Call this ONLY when every part of the goal is \
         actually done and you have nothing left to do — not as a status update. Calling it does \
         NOT end the run by itself: your changes are then verified automatically (typecheck/tests), \
         and only a passing verification finishes the goal. If verification fails you'll receive the \
         errors and must fix them, then call goal_complete again. Provide a short summary of what \
         was accomplished."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "summary": {
                    "type": "string",
                    "description": "a short summary of what was accomplished and why the goal is now complete"
                }
            },
            "required": ["summary"],
            "additionalProperties": false
        })
    }

    /// Declaring completion changes nothing on disk and needs no approval.
    fn is_destructive(&self) -> bool {
        false
    }

    /// Control-flow tool with a process-global side effect → must run serially, never in a
    /// parallel batch.
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn execute(&self, args: &Value) -> Result<String> {
        let summary = args
            .get("summary")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .context("missing required non-empty string arg 'summary'")?;
        let ack = build_ack(summary);
        set_pending(summary.to_string());
        Ok(ack)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_ack_mentions_summary_and_stop() {
        let ack = build_ack("  added --version flag  ");
        assert!(
            ack.contains("added --version flag"),
            "summary echoed: {ack}"
        );
        assert!(
            ack.contains("STOP now"),
            "the model must be told to stop calling tools: {ack}"
        );
        assert!(
            ack.contains("verif"),
            "the model must be told verification runs next: {ack}"
        );
    }

    #[test]
    fn execute_sets_pending_and_take_drains_it() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = take_pending(); // clear any leftover from another test
        let ack = GoalComplete
            .execute(&json!({"summary": "did the thing"}))
            .unwrap();
        assert!(ack.contains("did the thing"));
        let pending = take_pending().expect("a claim must be pending after execute");
        assert_eq!(pending, "did the thing");
        assert!(
            take_pending().is_none(),
            "take must drain — a second take yields nothing"
        );
    }

    #[test]
    fn clear_drains_without_reading() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = GoalComplete.execute(&json!({"summary": "x"})).unwrap();
        clear();
        assert!(
            take_pending().is_none(),
            "clear must drain the pending claim"
        );
    }

    #[test]
    fn execute_rejects_empty_or_missing_summary() {
        let _g = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        assert!(GoalComplete.execute(&json!({})).is_err(), "missing summary");
        assert!(
            GoalComplete.execute(&json!({"summary": "   "})).is_err(),
            "blank summary"
        );
        let _ = take_pending();
    }

    #[test]
    fn flags_are_nondestructive_and_serial() {
        assert!(
            !GoalComplete.is_destructive(),
            "declaring completion changes nothing"
        );
        assert!(
            !GoalComplete.is_concurrency_safe(),
            "global side effect → must run serially"
        );
    }
}
