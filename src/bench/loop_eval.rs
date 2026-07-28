//! `aizen bench loop` — offline loop-behavior eval harness (P4 + P0 persistence).
//!
//! The memory bench (`bench/mod.rs`) proves RECALL; this proves LOOP DISCIPLINE — the Section-10
//! metrics the improvement plan tracks: steps/task, loop-stop rate, repeat-call rate,
//! verified-done rate. It drives the REAL `run_agent_loop` with a SCRIPTED fake model (no network,
//! no provider, fully deterministic) over hand-authored scenarios spanning the task shapes the
//! plan calls out: quick answer, small edit, multi-file edit, fix-a-test, research, anti-loop,
//! and P0 harness persistence (todo-poke, confidence gate, hill-climb). Each scenario declares
//! what a HEALTHY loop should do; the harness asserts the loop actually does it and aggregates
//! the metrics.
//!
//! Why a fake model: the loop is generic over its chat fn exactly so it can be driven by a script
//! (`run_agent_loop<F, Fut>`), the same seam the unit tests use. A scripted model emits a fixed
//! sequence of turns regardless of the messages it's handed, so a scenario is a pure fixture:
//! "given these model turns, the loop must reach this outcome in this many steps."

use crate::agent::tools::{Tool, ToolRegistry};
use crate::agent::{AgentConfig, StopReason};
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
fn scripted(
    turns: Vec<ChatTurn>,
) -> impl Fn(Vec<Message>, Vec<ToolDef>) -> std::future::Ready<Result<ChatTurn>> {
    let q = Mutex::new(VecDeque::from(turns));
    move |_m, _d| {
        let next = q
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .pop_front()
            .unwrap_or_else(|| final_turn("stop"));
        std::future::ready(Ok(next))
    }
}

fn tool_turn(name: &str, args: &str) -> ChatTurn {
    ChatTurn {
        content: None,
        tool_calls: vec![ToolCall {
            id: format!("call_{name}"),
            kind: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
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
                function: FunctionCall {
                    name: (*name).into(),
                    arguments: (*args).into(),
                },
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
        Ok(args
            .get("text")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string())
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
    r.register(Box::new(crate::agent::todo::TodoWrite));
    r
}

// ── scenarios ──────────────────────────────────────────────────────────────────

/// What a HEALTHY loop must do for a scenario. Any script that fails its expectation is a
/// harness-caught regression (the loop's discipline changed for the worse).
struct Scenario {
    name: &'static str,
    /// The task SHAPE (for the per-shape rollup): "answer", "edit", "multi", "fix-test",
    /// "research", "anti-loop", "persist".
    shape: &'static str,
    /// The scripted model turns (the loop pops one per iteration; it degrades to a final "stop"
    /// once drained).
    turns: Vec<ChatTurn>,
    /// The stop reason a healthy loop must reach.
    expect_stop: StopReason,
    /// Upper bound on iterations — a healthy loop must not exceed this (catches the "wandered for
    /// 25 steps" regression). `None` skips the check.
    max_iters: Option<usize>,
    /// Optional seed for the process-global todo list (P0 persistence scenarios).
    seed_todos: Option<Vec<crate::agent::todo::Todo>>,
    /// When set, merge onto eval_cfg (P0 flags etc.).
    cfg_overlay: Option<PersistOverlay>,
    /// User task text (default "loop eval task").
    user_task: &'static str,
    /// Substring that must appear in some user/system message (optional assert).
    expect_msg_contains: Option<&'static str>,
}

#[derive(Clone, Copy)]
struct PersistOverlay {
    enable_todo_poke: bool,
    max_todo_poke_attempts: usize,
    enable_confidence_gate: bool,
    enable_hill_climb: bool,
}

fn scen(
    name: &'static str,
    shape: &'static str,
    turns: Vec<ChatTurn>,
    expect_stop: StopReason,
    max_iters: Option<usize>,
) -> Scenario {
    Scenario {
        name,
        shape,
        turns,
        expect_stop,
        max_iters,
        seed_todos: None,
        cfg_overlay: None,
        user_task: "loop eval task",
        expect_msg_contains: None,
    }
}

/// The hand-authored loop scenarios. Deterministic: each is a fixed model-turn script over the
/// offline fixture tools. Verify gate is OFF for all of them (no real toolchain in the harness);
/// the "verified-done" metric is measured structurally by the healthy-Done scenarios reaching Done
/// within budget, not by running `cargo check`.
fn scenarios() -> Vec<Scenario> {
    use crate::agent::todo::{Status, Todo};

    let mut out = vec![
        // ── quick answer: no tools, straight to a final answer ──
        scen(
            "answer-immediately",
            "answer",
            vec![final_turn("42")],
            StopReason::Done,
            Some(1),
        ),
        scen(
            "answer-after-one-read",
            "answer",
            vec![
                tool_turn("echo", r#"{"text":"looked it up"}"#),
                final_turn("here is the answer"),
            ],
            StopReason::Done,
            Some(2),
        ),
        // ── small edit: one read, one "edit" (echo stands in), done ──
        scen(
            "small-edit-then-done",
            "edit",
            vec![
                tool_turn("echo", r#"{"text":"read src/x.rs"}"#),
                tool_turn("echo", r#"{"text":"applied the patch"}"#),
                final_turn("changed src/x.rs"),
            ],
            StopReason::Done,
            Some(3),
        ),
        // ── multi-file: several distinct reads/edits, each surfacing new content ──
        scen(
            "multi-file-edit",
            "multi",
            vec![
                tool_turn("echo", r#"{"text":"read a.rs"}"#),
                tool_turn("echo", r#"{"text":"read b.rs"}"#),
                tool_turn("echo", r#"{"text":"edit a.rs"}"#),
                tool_turn("echo", r#"{"text":"edit b.rs"}"#),
                final_turn("updated a.rs and b.rs"),
            ],
            StopReason::Done,
            Some(5),
        ),
        // ── fix-a-test: read, edit, "run test" (new content each time), done ──
        scen(
            "fix-failing-test",
            "fix-test",
            vec![
                tool_turn("echo", r#"{"text":"read failing test"}"#),
                tool_turn("echo", r#"{"text":"read impl"}"#),
                tool_turn("echo", r#"{"text":"apply fix"}"#),
                tool_turn("echo", r#"{"text":"tests pass now"}"#),
                final_turn("fixed the test"),
            ],
            StopReason::Done,
            Some(5),
        ),
        // ── recover from ONE error then succeed (the healthy fix-the-cause path) ──
        scen(
            "recover-from-one-error",
            "fix-test",
            vec![
                tool_turn("fail", "{}"),
                tool_turn("echo", r#"{"text":"fixed the cause, different call"}"#),
                final_turn("recovered"),
            ],
            StopReason::Done,
            Some(3),
        ),
        // ── research: several distinct searches/fetches, each new, then a cited answer ──
        scen(
            "research-fan-out",
            "research",
            vec![
                tool_turn("echo", r#"{"text":"search angle 1"}"#),
                tool_turn("echo", r#"{"text":"search angle 2"}"#),
                tool_turn("echo", r#"{"text":"fetch top result"}"#),
                final_turn("answer with citation"),
            ],
            StopReason::Done,
            Some(4),
        ),
        // ── ANTI-LOOP: exact same failing call, over and over → must STOP (Divergence) ──
        scen(
            "identical-failing-call-diverges",
            "anti-loop",
            vec![
                tool_turn("fail", "{}"),
                tool_turn("fail", "{}"),
                tool_turn("fail", "{}"),
                tool_turn("fail", "{}"),
                tool_turn("fail", "{}"),
                tool_turn("fail", "{}"),
                tool_turn("fail", "{}"),
                tool_turn("fail", "{}"),
            ],
            StopReason::Divergence,
            None,
        ),
        // ── ANTI-LOOP: A,B,A,B oscillation ──
        scen(
            "ab-oscillation-diverges",
            "anti-loop",
            vec![
                tool_turn("const_read", r#"{"i":"a"}"#),
                tool_turn("fail", "{}"),
                tool_turn("const_read", r#"{"i":"a"}"#),
                tool_turn("fail", "{}"),
                tool_turn("const_read", r#"{"i":"a"}"#),
                tool_turn("fail", "{}"),
                tool_turn("const_read", r#"{"i":"a"}"#),
                tool_turn("fail", "{}"),
            ],
            StopReason::Divergence,
            None,
        ),
        // ── ANTI-LOOP: same const-read repeated (successful but useless, W4) ──
        scen(
            "useless-reread-stops",
            "anti-loop",
            vec![
                tool_turn("const_read", r#"{"i":"x"}"#),
                tool_turn("const_read", r#"{"i":"x"}"#),
                tool_turn("const_read", r#"{"i":"x"}"#),
                tool_turn("const_read", r#"{"i":"x"}"#),
                tool_turn("const_read", r#"{"i":"x"}"#),
                tool_turn("const_read", r#"{"i":"x"}"#),
                tool_turn("const_read", r#"{"i":"x"}"#),
            ],
            StopReason::Divergence,
            None,
        ),
        // ── ANTI-LOOP: padding a failing call with a throwaway success (W3) ──
        scen(
            "padded-fail-still-stops",
            "anti-loop",
            vec![
                multi_tool_turn(&[("fail", "{}"), ("const_read", r#"{"i":"pad"}"#)]),
                multi_tool_turn(&[("fail", "{}"), ("const_read", r#"{"i":"pad"}"#)]),
                multi_tool_turn(&[("fail", "{}"), ("const_read", r#"{"i":"pad"}"#)]),
                multi_tool_turn(&[("fail", "{}"), ("const_read", r#"{"i":"pad"}"#)]),
                multi_tool_turn(&[("fail", "{}"), ("const_read", r#"{"i":"pad"}"#)]),
                multi_tool_turn(&[("fail", "{}"), ("const_read", r#"{"i":"pad"}"#)]),
            ],
            StopReason::Divergence,
            None,
        ),
        // ── progressive reads that each surface NEW content are NOT diverging ──
        scen(
            "distinct-reads-not-diverging",
            "research",
            vec![
                tool_turn("echo", r#"{"text":"chunk 1"}"#),
                tool_turn("echo", r#"{"text":"chunk 2"}"#),
                tool_turn("echo", r#"{"text":"chunk 3"}"#),
                tool_turn("echo", r#"{"text":"chunk 4"}"#),
                final_turn("assembled from 4 distinct reads"),
            ],
            StopReason::Done,
            Some(5),
        ),
        // ── MaxIters: progress forever → step cap cleanly ──
        scen(
            "endless-progress-hits-maxiters",
            "anti-loop",
            (0..30)
                .map(|i| tool_turn("echo", &format!(r#"{{"text":"new content {i}"}}"#)))
                .collect(),
            StopReason::MaxIters,
            None,
        ),
        // ── two-step edit with a mid-course correction ──
        scen(
            "edit-with-correction",
            "edit",
            vec![
                tool_turn("echo", r#"{"text":"read config"}"#),
                tool_turn("echo", r#"{"text":"first attempt"}"#),
                tool_turn("echo", r#"{"text":"re-read to get exact text"}"#),
                tool_turn("echo", r#"{"text":"correct edit"}"#),
                final_turn("config updated"),
            ],
            StopReason::Done,
            Some(5),
        ),
        // ── minimal research: a single search answers it ──
        scen(
            "single-search-suffices",
            "research",
            vec![
                tool_turn("echo", r#"{"text":"one good search"}"#),
                final_turn("answered from the snippet"),
            ],
            StopReason::Done,
            Some(2),
        ),
    ];

    // ── P0.1: incomplete todos block Done until poke budget exhausts ──
    out.push(Scenario {
        name: "poke_blocks_early_done",
        shape: "persist",
        turns: vec![
            final_turn("done early"),
            final_turn("still claiming done"),
            final_turn("exhausted poke budget"),
        ],
        expect_stop: StopReason::Done,
        max_iters: Some(3),
        seed_todos: Some(vec![
            Todo::new("done-bit", Status::Done),
            Todo::new("still-open", Status::Pending),
        ]),
        cfg_overlay: Some(PersistOverlay {
            enable_todo_poke: true,
            max_todo_poke_attempts: 2,
            enable_confidence_gate: false,
            enable_hill_climb: false,
        }),
        user_task: "multi-step work",
        expect_msg_contains: Some("[todo-poke]"),
    });

    // ── P0.1: no todos → no poke ──
    out.push(Scenario {
        name: "no_poke_without_todos",
        shape: "persist",
        turns: vec![tool_turn("echo", r#"{"text":"edit"}"#), final_turn("done")],
        expect_stop: StopReason::Done,
        max_iters: Some(2),
        seed_todos: Some(vec![]),
        cfg_overlay: Some(PersistOverlay {
            enable_todo_poke: true,
            max_todo_poke_attempts: 2,
            enable_confidence_gate: false,
            enable_hill_climb: false,
        }),
        user_task: "small edit",
        expect_msg_contains: None,
    });

    // ── P0.2: confidence spike → one-shot re-check ──
    out.push(Scenario {
        name: "confidence_spike_recheck",
        shape: "persist",
        turns: vec![
            tool_turn(
                "todo_write",
                r#"{"todos":[{"content":"ship it","status":"in_progress","confidence":40}]}"#,
            ),
            tool_turn(
                "todo_write",
                r#"{"todos":[{"content":"ship it","status":"done","confidence":100}]}"#,
            ),
            final_turn("done"),
            final_turn("done after recheck"),
        ],
        expect_stop: StopReason::Done,
        max_iters: Some(4),
        seed_todos: Some(vec![]),
        cfg_overlay: Some(PersistOverlay {
            enable_todo_poke: false,
            max_todo_poke_attempts: 0,
            enable_confidence_gate: true,
            enable_hill_climb: false,
        }),
        user_task: "ship feature",
        expect_msg_contains: Some("[confidence-gate]"),
    });

    // ── P0.3: optimize keyword → hill-climb reframe ──
    out.push(Scenario {
        name: "hill_climb_reframe",
        shape: "persist",
        turns: vec![
            tool_turn("echo", r#"{"text":"measure"}"#),
            final_turn("improved"),
        ],
        expect_stop: StopReason::Done,
        max_iters: Some(2),
        seed_todos: None,
        cfg_overlay: Some(PersistOverlay {
            enable_todo_poke: false,
            max_todo_poke_attempts: 0,
            enable_confidence_gate: false,
            enable_hill_climb: true,
        }),
        user_task: "please optimize the float-print hot path",
        expect_msg_contains: Some("[hill-climb]"),
    });

    out
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
        context_window: 0,         // guard off — scripts are short and synthetic
        todo_reminder_every: 0,    // recitation reads the process-global list; irrelevant here
        // P0 harness: off by default in eval_cfg; persistence scenarios opt in explicitly.
        enable_todo_poke: false,
        enable_confidence_gate: false,
        enable_hill_climb: false,
        ..AgentConfig::default()
    }
}

/// Run one scenario against the REAL loop, return `(passed, iters, stop)`.
async fn run_scenario(s: &Scenario) -> (bool, usize, StopReason) {
    // Serialize process-global todo mutations across scenarios.
    let _g = crate::agent::todo::TEST_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(todos) = &s.seed_todos {
        crate::agent::todo::set(todos.clone());
    } else {
        crate::agent::todo::clear();
    }

    let registry = eval_registry();
    let mut cfg = eval_cfg();
    if let Some(o) = s.cfg_overlay {
        cfg.enable_todo_poke = o.enable_todo_poke;
        cfg.max_todo_poke_attempts = o.max_todo_poke_attempts;
        cfg.enable_confidence_gate = o.enable_confidence_gate;
        cfg.enable_hill_climb = o.enable_hill_climb;
    }

    // Capture messages for expect_msg_contains via run_agent_loop on a local buffer.
    let mut messages = vec![Message::system("sys"), Message::user(s.user_task)];
    let outcome = crate::agent::run_agent_loop(
        scripted(clone_turns(&s.turns)),
        &cfg,
        &registry,
        &mut messages,
    )
    .await
    .expect("eval scenarios never error the loop itself");

    let stop_ok = outcome.stop == s.expect_stop;
    let iters_ok = s.max_iters.is_none_or(|m| outcome.iters <= m);
    let msg_ok = match s.expect_msg_contains {
        None => true,
        Some(needle) => messages
            .iter()
            .any(|m| m.content.as_deref().is_some_and(|c| c.contains(needle))),
    };
    // no_poke_without_todos: assert absence of poke when expect_msg is None and poke was enabled.
    let no_spurious_poke = if s.name == "no_poke_without_todos" {
        !messages.iter().any(|m| {
            m.content
                .as_deref()
                .is_some_and(|c| c.starts_with("[todo-poke]"))
        })
    } else {
        true
    };

    crate::agent::todo::clear();
    (
        stop_ok && iters_ok && msg_ok && no_spurious_poke,
        outcome.iters,
        outcome.stop,
    )
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
    let mut m = LoopMetrics {
        total: scens.len(),
        ..Default::default()
    };
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
            s.name,
            s.shape,
            format!("{:?}", s.expect_stop),
            format!("{stop:?}"),
            iters
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
            println!(
                "mean steps/task (healthy-Done scenarios): {:.1}",
                m.done_iters as f64 / m.verified_done as f64
            );
        }
    }
    println!(
        "loop-stop (abnormal-on-healthy-task) rate: {:.0}% ({}/{})",
        if m.expected_done > 0 {
            100.0 * m.unexpected_abnormal as f64 / m.expected_done as f64
        } else {
            0.0
        },
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
