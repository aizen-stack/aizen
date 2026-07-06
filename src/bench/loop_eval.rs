//! `aizen bench loop` — offline loop-behavior eval harness (P4).
//!
//! The memory bench (`bench/mod.rs`) proves RECALL; this proves LOOP DISCIPLINE — the Section-10
//! metrics the improvement plan tracks: steps/task, loop-stop rate, repeat-call rate,
//! verified-done rate. It drives the REAL `run_agent_loop` with a SCRIPTED fake model (no network,
//! no provider, fully deterministic) over ~15 hand-authored scenarios spanning the task shapes the
//! plan calls out: quick answer, small edit, multi-file edit, fix-a-test, research, and the
//! failure modes the anti-loop work targets (A/B oscillation, successful-but-useless re-reads,
//! padding a stuck turn). Each scenario declares what a HEALTHY loop should do; the harness asserts
//! the loop actually does it and aggregates the metrics.
//!
//! Why a fake model: the loop is generic over its chat fn exactly so it can be driven by a script
//! (`run_agent_loop<F, Fut>`), the same seam the unit tests use. A scripted model emits a fixed
//! sequence of turns regardless of the messages it's handed, so a scenario is a pure fixture:
//! "given these model turns, the loop must reach this outcome in this many steps."

use crate::agent::tools::{Tool, ToolRegistry};
use crate::agent::{run_agent, AgentConfig, StopReason};
use crate::core::types::{FunctionCall, Message, ToolCall, ToolDef};
use crate::llm::client::ChatTurn;
use anyhow::Result;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

// ── scripted model + fixture tools (production scope, not #[cfg(test)]) ─────────

/// A scripted fake model: pops the next turn each call; empties → a final "stop". Ignores the
/// messages it's handed (a fixture emits a fixed sequence), which is the whole point — the SCENARIO
/// is the script, and the loop's own nudges can't derail a deterministic replay.
fn scripted(turns: Vec<ChatTurn>) -> impl Fn(Vec<Message>, Vec<ToolDef>) -> std::future::Ready<Result<ChatTurn>> {
    let q = Mutex::new(VecDeque::from(turns));
    move |_m, _d| {
        let next = q.lock().unwrap_or_else(|e| e.into_inner()).pop_front().unwrap_or_else(|| final_turn("stop"));
        std::future::ready(Ok(next))
    }
}

fn tool_turn(name: &str, args: &str) -> ChatTurn {
    ChatTurn {
        content: None,
        tool_calls: vec![ToolCall {
            id: format!("call_{name}"),
            kind: "function".into(),
            function: FunctionCall { name: name.into(), arguments: args.into() },
        }],
        finish_reason: Some("stop".into()),
        usage: None,
        eager: Vec::new(),
    }
}

fn final_turn(text: &str) -> ChatTurn {
    ChatTurn {
        content: Some(text.into()),
        tool_calls: vec![],
        finish_reason: Some("stop".into()),
        usage: None,
        eager: Vec::new(),
    }
}

/// A turn with SEVERAL tool calls — models a turn that pads a failing call with a throwaway
/// successful one to "look busy" (the W3/W4 anti-pattern).
fn multi_tool_turn(calls: &[(&str, &str)]) -> ChatTurn {
    ChatTurn {
        content: None,
        tool_calls: calls
            .iter()
            .enumerate()
            .map(|(i, (name, args))| ToolCall {
                id: format!("call_{name}_{i}"),
                kind: "function".into(),
                function: FunctionCall { name: (*name).into(), arguments: (*args).into() },
            })
            .collect(),
        finish_reason: Some("stop".into()),
        usage: None,
        eager: Vec::new(),
    }
}

// ── fixture tools (deterministic, offline) ──────────────────────────────────────

/// Echoes its `text` arg — a benign read-only tool that always returns NEW content when its args
/// differ (a healthy progressive read).
struct Echo;
impl Tool for Echo {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echo back the text arg"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]})
    }
    fn execute(&self, args: &Value) -> Result<String> {
        Ok(args.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string())
    }
}

/// Returns CONSTANT bytes regardless of args — models the successful-but-useless re-read (W4): a
/// call that "succeeds" every time yet surfaces nothing new.
struct Const;
impl Tool for Const {
    fn name(&self) -> &str {
        "const_read"
    }
    fn description(&self) -> &str {
        "returns constant bytes"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type":"object","properties":{"i":{"type":"string"}}})
    }
    fn execute(&self, _args: &Value) -> Result<String> {
        Ok("CONSTANT-CONTENT".to_string())
    }
}

/// Always errors — models a call whose CAUSE the model must fix before retrying (re-issuing it
/// verbatim is the W1 loop the anti-loop guard must catch).
struct Fail;
impl Tool for Fail {
    fn name(&self) -> &str {
        "fail"
    }
    fn description(&self) -> &str {
        "always errors"
    }
    fn parameters(&self) -> Value {
        serde_json::json!({"type":"object","properties":{}})
    }
    fn execute(&self, _args: &Value) -> Result<String> {
        anyhow::bail!("boom")
    }
}

fn eval_registry() -> ToolRegistry {
    let mut r = ToolRegistry::new();
    r.register(Box::new(Echo));
    r.register(Box::new(Const));
    r.register(Box::new(Fail));
    r
}

// ── scenarios ──────────────────────────────────────────────────────────────────

/// What a HEALTHY loop must do for a scenario. Any script that fails its expectation is a
/// harness-caught regression (the loop's discipline changed for the worse).
struct Scenario {
    name: &'static str,
    /// The task SHAPE (for the per-shape rollup): "answer", "edit", "multi", "fix-test",
    /// "research", "anti-loop".
    shape: &'static str,
    /// The scripted model turns (the loop pops one per iteration; it degrades to a final "stop"
    /// once drained).
    turns: Vec<ChatTurn>,
    /// The stop reason a healthy loop must reach.
    expect_stop: StopReason,
    /// Upper bound on iterations — a healthy loop must not exceed this (catches the "wandered for
    /// 25 steps" regression). `None` skips the check.
    max_iters: Option<usize>,
}

/// The ~15 hand-authored loop scenarios. Deterministic: each is a fixed model-turn script over the
/// offline fixture tools. Verify gate is OFF for all of them (no real toolchain in the harness);
/// the "verified-done" metric is measured structurally by the healthy-Done scenarios reaching Done
/// within budget, not by running `cargo check`.
fn scenarios() -> Vec<Scenario> {
    vec![
        // ── quick answer: no tools, straight to a final answer ──
        Scenario {
            name: "answer-immediately",
            shape: "answer",
            turns: vec![final_turn("42")],
            expect_stop: StopReason::Done,
            max_iters: Some(1),
        },
        Scenario {
            name: "answer-after-one-read",
            shape: "answer",
            turns: vec![tool_turn("echo", r#"{"text":"looked it up"}"#), final_turn("here is the answer")],
            expect_stop: StopReason::Done,
            max_iters: Some(2),
        },
        // ── small edit: one read, one "edit" (echo stands in), done ──
        Scenario {
            name: "small-edit-then-done",
            shape: "edit",
            turns: vec![
                tool_turn("echo", r#"{"text":"read src/x.rs"}"#),
                tool_turn("echo", r#"{"text":"applied the patch"}"#),
                final_turn("changed src/x.rs"),
            ],
            expect_stop: StopReason::Done,
            max_iters: Some(3),
        },
        // ── multi-file: several distinct reads/edits, each surfacing new content ──
        Scenario {
            name: "multi-file-edit",
            shape: "multi",
            turns: vec![
                tool_turn("echo", r#"{"text":"read a.rs"}"#),
                tool_turn("echo", r#"{"text":"read b.rs"}"#),
                tool_turn("echo", r#"{"text":"edit a.rs"}"#),
                tool_turn("echo", r#"{"text":"edit b.rs"}"#),
                final_turn("updated a.rs and b.rs"),
            ],
            expect_stop: StopReason::Done,
            max_iters: Some(5),
        },
        // ── fix-a-test: read, edit, "run test" (new content each time), done ──
        Scenario {
            name: "fix-failing-test",
            shape: "fix-test",
            turns: vec![
                tool_turn("echo", r#"{"text":"read failing test"}"#),
                tool_turn("echo", r#"{"text":"read impl"}"#),
                tool_turn("echo", r#"{"text":"apply fix"}"#),
                tool_turn("echo", r#"{"text":"tests pass now"}"#),
                final_turn("fixed the test"),
            ],
            expect_stop: StopReason::Done,
            max_iters: Some(5),
        },
        // ── recover from ONE error then succeed (the healthy fix-the-cause path) ──
        Scenario {
            name: "recover-from-one-error",
            shape: "fix-test",
            turns: vec![
                tool_turn("fail", "{}"),
                tool_turn("echo", r#"{"text":"fixed the cause, different call"}"#),
                final_turn("recovered"),
            ],
            expect_stop: StopReason::Done,
            max_iters: Some(3),
        },
        // ── research: several distinct searches/fetches, each new, then a cited answer ──
        Scenario {
            name: "research-fan-out",
            shape: "research",
            turns: vec![
                tool_turn("echo", r#"{"text":"search angle 1"}"#),
                tool_turn("echo", r#"{"text":"search angle 2"}"#),
                tool_turn("echo", r#"{"text":"fetch top result"}"#),
                final_turn("answer with citation"),
            ],
            expect_stop: StopReason::Done,
            max_iters: Some(4),
        },
        // ── ANTI-LOOP: exact same failing call, over and over → must STOP (Divergence), not run to
        //    the iteration cap. This is W1 — the core anti-loop guarantee.
        Scenario {
            name: "identical-failing-call-diverges",
            shape: "anti-loop",
            turns: vec![
                tool_turn("fail", "{}"),
                tool_turn("fail", "{}"),
                tool_turn("fail", "{}"),
                tool_turn("fail", "{}"),
                tool_turn("fail", "{}"),
                tool_turn("fail", "{}"),
                tool_turn("fail", "{}"),
                tool_turn("fail", "{}"),
            ],
            expect_stop: StopReason::Divergence,
            max_iters: None,
        },
        // ── ANTI-LOOP: A,B,A,B oscillation of two useless calls → must be caught (W1 two-cycle). ──
        Scenario {
            name: "ab-oscillation-diverges",
            shape: "anti-loop",
            turns: vec![
                tool_turn("const_read", r#"{"i":"a"}"#),
                tool_turn("fail", "{}"),
                tool_turn("const_read", r#"{"i":"a"}"#),
                tool_turn("fail", "{}"),
                tool_turn("const_read", r#"{"i":"a"}"#),
                tool_turn("fail", "{}"),
                tool_turn("const_read", r#"{"i":"a"}"#),
                tool_turn("fail", "{}"),
            ],
            expect_stop: StopReason::Divergence,
            max_iters: None,
        },
        // ── ANTI-LOOP: same const-read repeated (successful but useless, W4) → must stop, not spin. ──
        Scenario {
            name: "useless-reread-stops",
            shape: "anti-loop",
            turns: vec![
                tool_turn("const_read", r#"{"i":"x"}"#),
                tool_turn("const_read", r#"{"i":"x"}"#),
                tool_turn("const_read", r#"{"i":"x"}"#),
                tool_turn("const_read", r#"{"i":"x"}"#),
                tool_turn("const_read", r#"{"i":"x"}"#),
                tool_turn("const_read", r#"{"i":"x"}"#),
                tool_turn("const_read", r#"{"i":"x"}"#),
            ],
            expect_stop: StopReason::Divergence,
            max_iters: None,
        },
        // ── ANTI-LOOP: padding a failing call with a throwaway success must NOT reset the streak
        //    (W3) — the loop still converges to a stop instead of looping forever.
        Scenario {
            name: "padded-fail-still-stops",
            shape: "anti-loop",
            turns: vec![
                multi_tool_turn(&[("fail", "{}"), ("const_read", r#"{"i":"pad"}"#)]),
                multi_tool_turn(&[("fail", "{}"), ("const_read", r#"{"i":"pad"}"#)]),
                multi_tool_turn(&[("fail", "{}"), ("const_read", r#"{"i":"pad"}"#)]),
                multi_tool_turn(&[("fail", "{}"), ("const_read", r#"{"i":"pad"}"#)]),
                multi_tool_turn(&[("fail", "{}"), ("const_read", r#"{"i":"pad"}"#)]),
                multi_tool_turn(&[("fail", "{}"), ("const_read", r#"{"i":"pad"}"#)]),
            ],
            expect_stop: StopReason::Divergence,
            max_iters: None,
        },
        // ── progressive reads that each surface NEW content are NOT diverging (guard precision:
        //    it must not false-positive a healthy exploration). ──
        Scenario {
            name: "distinct-reads-not-diverging",
            shape: "research",
            turns: vec![
                tool_turn("echo", r#"{"text":"chunk 1"}"#),
                tool_turn("echo", r#"{"text":"chunk 2"}"#),
                tool_turn("echo", r#"{"text":"chunk 3"}"#),
                tool_turn("echo", r#"{"text":"chunk 4"}"#),
                final_turn("assembled from 4 distinct reads"),
            ],
            expect_stop: StopReason::Done,
            max_iters: Some(5),
        },
        // ── MaxIters: a model that keeps making PROGRESS (new content) but never finishes must hit
        //    the step cap cleanly (a synthesis, not a crash). ──
        Scenario {
            name: "endless-progress-hits-maxiters",
            shape: "anti-loop",
            turns: (0..30).map(|i| tool_turn("echo", &format!(r#"{{"text":"new content {i}"}}"#))).collect(),
            expect_stop: StopReason::MaxIters,
            max_iters: None,
        },
        // ── two-step edit with a mid-course correction (read, wrong edit, re-read, right edit) ──
        Scenario {
            name: "edit-with-correction",
            shape: "edit",
            turns: vec![
                tool_turn("echo", r#"{"text":"read config"}"#),
                tool_turn("echo", r#"{"text":"first attempt"}"#),
                tool_turn("echo", r#"{"text":"re-read to get exact text"}"#),
                tool_turn("echo", r#"{"text":"correct edit"}"#),
                final_turn("config updated"),
            ],
            expect_stop: StopReason::Done,
            max_iters: Some(5),
        },
        // ── minimal research: a single search answers it (no over-fetching) ──
        Scenario {
            name: "single-search-suffices",
            shape: "research",
            turns: vec![tool_turn("echo", r#"{"text":"one good search"}"#), final_turn("answered from the snippet")],
            expect_stop: StopReason::Done,
            max_iters: Some(2),
        },
    ]
}

// ── metrics ──────────────────────────────────────────────────────────────────

/// The Section-10 loop metrics, aggregated across the scenario set.
#[derive(Default)]
struct LoopMetrics {
    total: usize,
    passed: usize,
    /// Scenarios whose healthy expectation was Done and that reached it within budget.
    verified_done: usize,
    /// Scenarios that expected Done (the denominator for verified-done rate).
    expected_done: usize,
    /// Scenarios that ended in an abnormal stop (Divergence/MaxIters) — the denominator here is
    /// only the HEALTHY-Done scenarios, so a high rate means the loop bailed on work it should have
    /// finished.
    unexpected_abnormal: usize,
    /// Sum of iterations over healthy-Done scenarios (for mean steps/task).
    done_iters: usize,
    /// Per-shape pass/total.
    by_shape: HashMap<&'static str, (usize, usize)>,
}

fn eval_cfg() -> AgentConfig {
    AgentConfig {
        max_iters: 12,
        auto_extend_to: 20,
        quiet: true,
        enable_verify_gate: false, // no real toolchain in the harness — this measures LOOP shape
        auto_checkpoint: false,    // cwd may be a real repo — no checkpoint pollution
        context_window: 0,        // guard off — scripts are short and synthetic
        todo_reminder_every: 0,   // recitation reads the process-global list; irrelevant here
        ..AgentConfig::default()
    }
}

/// Run one scenario against the REAL loop, return `(passed, iters, stop)`.
async fn run_scenario(s: &Scenario) -> (bool, usize, StopReason) {
    let registry = eval_registry();
    let cfg = eval_cfg();
    let outcome = run_agent(scripted(clone_turns(&s.turns)), &cfg, &registry, "sys", "loop eval task")
        .await
        .expect("eval scenarios never error the loop itself");
    let stop_ok = outcome.stop == s.expect_stop;
    let iters_ok = s.max_iters.is_none_or(|m| outcome.iters <= m);
    (stop_ok && iters_ok, outcome.iters, outcome.stop)
}

/// `ChatTurn` doesn't derive `Clone` (it carries JoinHandles); scenarios are re-run at most once
/// per process so a shallow field-by-field copy (dropping any eager handles, which fixtures never
/// populate) is exactly as good as a real clone here.
fn clone_turns(turns: &[ChatTurn]) -> Vec<ChatTurn> {
    turns
        .iter()
        .map(|t| ChatTurn {
            content: t.content.clone(),
            tool_calls: t.tool_calls.clone(),
            finish_reason: t.finish_reason.clone(),
            usage: t.usage.clone(),
            eager: Vec::new(),
        })
        .collect()
}

/// `ng bench loop` entry point: run every scenario, print a report, exit non-zero if any regressed.
/// Async because `main` already drives a Tokio runtime — we run on it rather than nesting one.
pub async fn run() -> Result<()> {
    let scens = scenarios();
    let mut m = LoopMetrics { total: scens.len(), ..Default::default() };
    let mut failures: Vec<String> = Vec::new();

    for s in &scens {
        let (passed, iters, stop) = run_scenario(s).await;
        let shape_entry = m.by_shape.entry(s.shape).or_insert((0, 0));
        shape_entry.1 += 1;
        if passed {
            m.passed += 1;
            shape_entry.0 += 1;
        } else {
            failures.push(format!(
                "  - {} ({}): expected {:?} got {:?} in {} iter(s) (max {:?})",
                s.name, s.shape, s.expect_stop, stop, iters, s.max_iters
            ));
        }
        if s.expect_stop == StopReason::Done {
            m.expected_done += 1;
            if passed {
                m.verified_done += 1;
                m.done_iters += iters;
            } else if !matches!(stop, StopReason::Done) {
                m.unexpected_abnormal += 1;
            }
        }
        println!(
            "  {:<32} {:<10} expect={:<12} got={:<12} iters={}",
            s.name, s.shape, format!("{:?}", s.expect_stop), format!("{stop:?}"), iters
        );
    }

    println!();
    println!("scenarios: {}/{} passed", m.passed, m.total);
    let mut shapes: Vec<_> = m.by_shape.iter().collect();
    shapes.sort_by_key(|(k, _)| *k);
    for (shape, (pass, total)) in shapes {
        println!("  {shape:<10} {pass}/{total}");
    }
    if m.expected_done > 0 {
        println!(
            "verified-done rate: {:.0}% ({}/{})",
            100.0 * m.verified_done as f64 / m.expected_done as f64,
            m.verified_done,
            m.expected_done
        );
        if m.verified_done > 0 {
            println!("mean steps/task (healthy-Done scenarios): {:.1}", m.done_iters as f64 / m.verified_done as f64);
        }
    }
    println!(
        "loop-stop (abnormal-on-healthy-task) rate: {:.0}% ({}/{})",
        if m.expected_done > 0 { 100.0 * m.unexpected_abnormal as f64 / m.expected_done as f64 } else { 0.0 },
        m.unexpected_abnormal,
        m.expected_done
    );

    if !failures.is_empty() {
        eprintln!("\nFAILED scenarios:");
        for f in &failures {
            eprintln!("{f}");
        }
        anyhow::bail!("{}/{} loop scenarios regressed", failures.len(), m.total);
    }
    println!("\nLOOP EVAL: PASS ({}/{} scenarios)", m.passed, m.total);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scenario_set_has_at_least_15_covering_every_shape() {
        let s = scenarios();
        assert!(s.len() >= 15, "only {} scenarios", s.len());
        for want in ["answer", "edit", "multi", "fix-test", "research", "anti-loop"] {
            assert!(s.iter().any(|x| x.shape == want), "no scenario covers shape '{want}'");
        }
        // Every name is unique (a duplicate would silently shadow coverage).
        let mut names: Vec<&str> = s.iter().map(|x| x.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), s.len(), "scenario names must be unique");
    }

    #[tokio::test]
    async fn healthy_scenarios_reach_done_within_budget() {
        for s in scenarios().into_iter().filter(|s| s.expect_stop == StopReason::Done) {
            let (passed, iters, stop) = run_scenario(&s).await;
            assert!(passed, "{}: expected Done within {:?}, got {stop:?} in {iters} iter(s)", s.name, s.max_iters);
        }
    }

    #[tokio::test]
    async fn anti_loop_scenarios_actually_diverge_or_cap() {
        for s in scenarios().into_iter().filter(|s| s.shape == "anti-loop") {
            let (passed, iters, stop) = run_scenario(&s).await;
            assert!(passed, "{}: expected {:?}, got {stop:?} in {iters} iter(s)", s.name, s.expect_stop);
            // The whole point of these scenarios: they must NOT reach Done (that would mean the
            // guard missed a genuine loop).
            assert_ne!(stop, StopReason::Done, "{}: anti-loop scenario must not reach Done", s.name);
        }
    }

    #[tokio::test]
    async fn full_run_reports_metrics_without_panicking() {
        // Smoke-test the aggregation path itself (not just individual scenarios) on a small subset.
        let scens: Vec<Scenario> = scenarios().into_iter().take(3).collect();
        for s in &scens {
            let _ = run_scenario(s).await;
        }
    }
}

