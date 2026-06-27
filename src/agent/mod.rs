//! The agent harness loop — the lean 6-step state machine (see
//! `.claude/plans/260620-cli-harness/plan.md` §2.1).
//!
//! per turn: CALL model (with tools) → CLASSIFY (a non-empty structured `tool_calls[]` is
//! the signal — NOT `finish_reason`) → if none, the content is the final answer (DONE) →
//! else append the assistant tool-call turn, EXECUTE each call (validate args → gate
//! destructive ops → truncate result → feed errors back), append `tool` results → check
//! CONVERGENCE (divergence break + one-shot auto-extend near the cap) → loop.
//!
//! The loop is generic over the chat fn so it's driven by a scripted fake model in tests
//! (no live calls). Production passes a closure over `client::chat_with_tools`.

#[cfg(feature = "browser")]
pub mod browser;
pub mod builtin;
pub mod clarify;
pub mod cmd_guard;
pub mod mcp;
pub mod mcp_oauth;
pub mod process;
pub mod search;
pub mod task_tool;
pub mod todo;
pub mod tools;
pub mod verify_gate;
pub mod web_tools;
pub mod workflow;

use crate::client::ChatTurn;
use crate::types::{Message, ToolCall, ToolDef};
use anyhow::Result;
use console::style;
use std::future::Future;
use tools::ToolRegistry;

/// The static (cached-prefix) base system prompt — see `system-prompt.md`.
pub const SYSTEM_BASE: &str = include_str!("system_prompt.md");

/// Assemble the full system prompt: static base → stable-dynamic `<environment>` →
/// volatile `<user_memory>` (the frozen core). Ordered for prefix-cache stability — the
/// static base stays byte-identical across turns/sessions so the upstream prefix stays warm.
pub fn build_system_prompt(
    cwd: &str,
    os: &str,
    date: &str,
    model: &str,
    frozen_core: Option<&str>,
) -> String {
    let mut s = String::from(SYSTEM_BASE.trim_end());
    s.push_str("\n\n<environment>\n");
    s.push_str(&format!("cwd: {cwd}\nos: {os}\ndate: {date}\nmodel: {model}\n"));
    s.push_str("</environment>\n");
    // Durable AGENT operating-identity (who the agent IS across every persona/project) — ABOVE the
    // persona costume and the user model. HOME-only + sanitized + fail-closed (see `crate::soul`).
    if let Some(soul) = crate::soul::prompt_block() {
        s.push_str("\n<agent_identity>\n");
        s.push_str(soul.trim());
        s.push_str("\n</agent_identity>\n");
    }
    // Active character card (who the agent IS) — before user_memory (who the user is).
    if let Some(p) = crate::persona::prompt_block() {
        s.push_str("\n<persona>\n");
        s.push_str(p.trim());
        s.push_str("\n</persona>\n");
        // The character's accumulated experience (who it has BECOME) — only meaningful with a
        // persona active, so nested under it.
        if let Some(sb) = crate::persona::self_block() {
            s.push_str("\n<self>\n");
            s.push_str(sb.trim());
            s.push_str("\n</self>\n");
        }
    }
    if let Some(fc) = frozen_core {
        let fc = fc.trim();
        if !fc.is_empty() {
            s.push_str("\n<user_memory>\n");
            s.push_str(fc);
            s.push_str("\n</user_memory>\n");
        }
    }
    // Compact index of saved skills (procedures); full bodies are pulled on demand via skill_load.
    if let Some(idx) = crate::skill::prompt_index() {
        s.push_str("\n<skills>\n");
        s.push_str(&idx);
        s.push_str("\n</skills>\n");
    }
    s
}

#[derive(Debug, Clone)]
pub struct AgentConfig {
    /// Hard step cap before the one-shot auto-extend.
    pub max_iters: usize,
    /// Extended cap after the single auto-extend (the extension's anti-throttle lesson:
    /// don't hard-stop a converging task — nudge once and grant more room).
    pub auto_extend_to: usize,
    /// Per-tool result truncation (chars). Bounds history growth cheaply.
    pub max_tool_result_chars: usize,
    /// Pre-authorize destructive ops (the `--yes` escape hatch / `/yolo`). The hard blocklist in
    /// `cmd_guard` still applies underneath — `auto_approve` skips the *prompt*, never the floor.
    pub auto_approve: bool,
    /// `smart` approval tier: auto-run read-only-shaped shell commands (`ls`/`cat`/`rg`/`git status`/
    /// `cargo check` …) without a prompt, while writes/network/installs/deletes still ask. Independent
    /// of `auto_approve`; the hard blocklist applies regardless. Default OFF (= classic `manual`).
    pub smart_approve: bool,
    /// Suppress the stderr progress trace (tests set this).
    pub quiet: bool,
    /// Run a fast typecheck/build (cargo check / tsc) once after an editing run, before
    /// reporting Done; on failure inject the errors and grant one fix turn (F2 verify gate).
    pub enable_verify_gate: bool,
    /// Wall-clock cap (seconds) for the verify-gate subprocess.
    pub verify_gate_timeout_secs: u64,
    /// The model's context window in tokens, for the mid-loop context guard. A single run-away
    /// loop (reading many large files) can blow past the window BEFORE control returns to the
    /// REPL's auto-compact; when the running history crosses ~90% of this, the loop injects a
    /// one-time "wrap up" nudge. `0` (default) disables the guard — set by the interactive/one-shot
    /// callers from the resolved window; sub-agents leave it 0 (they are bounded + quiet).
    pub context_window: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iters: 25,
            auto_extend_to: 50,
            max_tool_result_chars: 4096,
            auto_approve: false,
            smart_approve: false,
            quiet: false,
            enable_verify_gate: true,
            verify_gate_timeout_secs: 90,
            context_window: 0,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum StopReason {
    /// The model returned a final answer (no tool calls).
    Done,
    /// The model repeated the exact same tool call(s) two turns running.
    Divergence,
    /// The (auto-extended) step cap was exhausted.
    MaxIters,
    /// The model invoked `clarify` — the turn is PAUSED pending the user's answer (the carried
    /// string is the user-facing question + options). The caller surfaces it and the next user
    /// message re-enters the loop as the answer. History is left valid (the assistant tool-call
    /// turn and its tool result are already appended).
    AwaitingInput(String),
}

#[derive(Debug)]
pub struct AgentOutcome {
    /// The model's final answer (already streamed to stdout by `ng agent`; kept for callers
    /// that consume the loop programmatically + the tests).
    #[allow(dead_code)]
    pub final_text: Option<String>,
    pub iters: usize,
    pub stop: StopReason,
}

/// Run the agent loop to completion.
///
/// `chat(messages, tool_defs) -> ChatTurn` is the model call (injected for testability).
pub async fn run_agent<F, Fut>(
    chat: F,
    cfg: &AgentConfig,
    registry: &ToolRegistry,
    system_prompt: &str,
    user_task: &str,
) -> Result<AgentOutcome>
where
    F: Fn(Vec<Message>, Vec<ToolDef>) -> Fut,
    Fut: Future<Output = Result<ChatTurn>>,
{
    let mut messages = vec![Message::system(system_prompt), Message::user(user_task)];
    run_agent_loop(chat, cfg, registry, &mut messages).await
}

/// The agent loop over an EXISTING conversation: `messages` already holds the system prompt and
/// every turn so far (the unified chat+agent REPL appends a user turn then calls this). Drives
/// tool calls to a final answer; on Done the final assistant text is pushed onto `messages` so a
/// multi-turn caller keeps context. Single-shot callers go through `run_agent`.
pub async fn run_agent_loop<F, Fut>(
    chat: F,
    cfg: &AgentConfig,
    registry: &ToolRegistry,
    messages: &mut Vec<Message>,
) -> Result<AgentOutcome>
where
    F: Fn(Vec<Message>, Vec<ToolDef>) -> Fut,
    Fut: Future<Output = Result<ChatTurn>>,
{
    let defs = registry.defs();
    let mut cap = cfg.max_iters;
    let mut extended = false;
    let mut last_sig: Option<String> = None;
    let mut recovery_used = false;
    let mut verify_nudged = false;
    let mut made_edits = false;
    let mut context_warned = false;
    let mut iter = 0usize;

    while iter < cap {
        // MID-LOOP CONTEXT GUARD: a single run-away loop (e.g. reading many large files) can blow
        // past the window BEFORE control returns to the REPL's auto-compact (which only runs
        // between turns). When the running history crosses ~90% of the window, inject a ONE-TIME
        // "wrap up" nudge so the model acts on what it has rather than overflowing. Pure arithmetic
        // (chars/4) — NOT a mid-loop summarization model call, and no tokenizer dep. Disabled when
        // `context_window == 0` (sub-agents / unconfigured).
        if cfg.context_window > 0 && !context_warned && estimate_tokens(messages) * 100 >= cfg.context_window * 90 {
            context_warned = true;
            messages.push(Message::system(
                "Context is nearly full (~90% of the window). Wrap up now: stop gathering more, act \
                 on what you already have, and give your final answer — or state what is blocking you.",
            ));
        }

        let turn = chat(messages.clone(), defs.clone()).await?;

        // CLASSIFY: the structured tool_calls array is the source of truth (a gateway may
        // emit finish_reason="stop"/"end_turn" alongside tool calls — ignore it here).
        if turn.tool_calls.is_empty() {
            // VERIFY GATE (F2): after an editing run, run a fast typecheck once before Done.
            // On failure, record the premature "done", inject the errors, and grant one fix
            // turn. Best-effort: an unknown project / missing toolchain → no-op.
            if cfg.enable_verify_gate && made_edits && !verify_nudged {
                verify_nudged = true;
                // Canonicalize to match the tool-registry root (`builtin::resolve_root`), so the
                // gate typechecks the same tree the file tools were confined to.
                let cwd = std::env::current_dir()
                    .and_then(|p| p.canonicalize())
                    .unwrap_or_else(|_| std::path::PathBuf::from("."));
                if let Some(result) =
                    verify_gate::run_verify_gate(&cwd, cfg.verify_gate_timeout_secs).await
                {
                    if !cfg.quiet {
                        let line = format!(
                            "→ verify: {} {}",
                            result.command,
                            if result.passed { "passed" } else { "FAILED" }
                        );
                        if crate::tui::active() {
                            crate::tui::emit_line(&line);
                        } else {
                            eprintln!("{line}");
                        }
                    }
                    if !result.passed {
                        // Record the premature "done" before the user gate-failure message.
                        // Normalize content to "" (never null): an assistant turn with neither
                        // content nor tool_calls is malformed (400) on strict gateways.
                        messages.push(Message {
                            role: "assistant".to_string(),
                            content: Some(turn.content.clone().unwrap_or_default()),
                            tool_calls: Vec::new(),
                            tool_call_id: None,
                            images: Vec::new(),
                            cache_control: None,
                        });
                        messages.push(Message::user(verify_gate::format_gate_failure(&result)));
                        iter += 1;
                        continue;
                    }
                }
            }
            // Push the final assistant text so a multi-turn caller (REPL) keeps context.
            if let Some(t) = &turn.content {
                if !t.trim().is_empty() {
                    messages.push(Message::assistant(t.clone()));
                }
            }
            return Ok(AgentOutcome {
                final_text: turn.content,
                iters: iter + 1,
                stop: StopReason::Done,
            });
        }

        // DIVERGENCE: identical tool call(s) two turns in a row = no progress. Self-resolve
        // first (the extension's R0/R1 lite): one corrective nudge before giving up. The
        // repeated call is NOT executed/appended (so no dangling tool_calls in history).
        let sig = turn_signature(&turn.tool_calls);
        if last_sig.as_deref() == Some(sig.as_str()) {
            if !recovery_used {
                recovery_used = true;
                messages.push(Message::system(
                    "You repeated the same tool call(s) with no new information. Take a DIFFERENT approach, or stop and explain what is blocking you.",
                ));
                iter += 1;
                continue;
            }
            return Ok(AgentOutcome {
                final_text: turn.content,
                iters: iter + 1,
                stop: StopReason::Divergence,
            });
        }
        last_sig = Some(sig);

        // APPEND the assistant tool-call turn (preserving any pre-tool content).
        let calls = turn.tool_calls.clone();
        messages.push(Message {
            role: "assistant".to_string(),
            content: turn.content.clone(),
            tool_calls: calls.clone(),
            tool_call_id: None,
            images: Vec::new(),
            cache_control: None,
        });

        // EXECUTE the call(s). Read-only batches run in parallel; any write/shell turn runs
        // serially (approval + ordering preserved). Results come back in ORIGINAL call order.
        let results = execute_calls(registry, &calls, cfg);

        // Arm the verify gate only if a destructive tool actually SUCCEEDED this turn — a
        // denied/errored edit changed nothing, so it must not make the gate blame the tree.
        if turn_made_edits(registry, &calls, &results) {
            made_edits = true;
        }
        for (id, result) in results {
            messages.push(Message::tool_result(id, result));
        }

        iter += 1;

        // CLARIFY YIELD: a `clarify` call this turn posed a question and PAUSED forward progress —
        // stop and hand back to the user (their next message is the answer, re-entering this loop).
        // Checked after the tool results are appended so `messages` is a valid, resumable history.
        // Gated on a clarify call actually firing this turn (not just "is the cell non-empty") so a
        // turn that never asked can't drain a stale value.
        if calls.iter().any(|c| c.function.name == clarify::NAME) {
            if let Some(question) = clarify::take_pending() {
                return Ok(AgentOutcome {
                    final_text: None,
                    iters: iter,
                    stop: StopReason::AwaitingInput(question),
                });
            }
        }

        // CONVERGENCE: near the cap, nudge once and grant the extended cap (pressure, not gate).
        if iter >= cap && !extended && cfg.auto_extend_to > cap {
            extended = true;
            cap = cfg.auto_extend_to;
            messages.push(Message::system(
                "You are nearing the step limit. Finish the task now, or stop and state what is blocking you.",
            ));
        }
    }

    Ok(AgentOutcome { final_text: None, iters: iter, stop: StopReason::MaxIters })
}

/// Hard cap on concurrent tool threads in a parallel batch. Conservative for a single-binary
/// CLI whose safe tools are I/O-bound (file reads, memory lookups) — enough to overlap latency
/// without oversubscribing. Not configurable in v1 (no measured need for a `--threads` flag).
const MAX_PARALLEL: usize = 5;

/// Execute a turn's tool calls, returning `(tool_call_id, result_text)` in ORIGINAL call order.
///
/// Partition rule (safety before performance): if ANY call targets a destructive /
/// non-concurrency-safe / unknown tool, the WHOLE batch runs serially — this keeps approval
/// gating, ordering, and the `assistant.tool_calls → tool` pairing byte-identical for every
/// write/shell turn. Otherwise, a batch of ≥2 read-only calls runs concurrently; a lone call
/// runs serially (thread spawn overhead > benefit). Results are re-stitched into the original
/// `calls` order regardless of completion order (out-of-order results → upstream 400).
fn execute_calls(
    registry: &ToolRegistry,
    calls: &[ToolCall],
    cfg: &AgentConfig,
) -> Vec<(String, String)> {
    let all_parallel_safe = calls.iter().all(|tc| is_parallel_safe(registry, tc));
    if all_parallel_safe && calls.len() >= 2 {
        return execute_parallel(registry, calls, cfg);
    }
    // Serial path — also the destructive/unknown fallback. `execute_one` keeps approval gating
    // and turns unknown tools / errors into feedback strings.
    calls
        .iter()
        .map(|tc| {
            let r = truncate_result(&execute_one(registry, tc, cfg), cfg.max_tool_result_chars);
            (tc.id.clone(), r)
        })
        .collect()
}

/// A call is parallel-safe iff its tool exists, is read-only (`!is_destructive`), and declares
/// itself concurrency-safe. Unknown tools are NOT safe → serial fallback (where `execute_one`
/// produces the "unknown tool" feedback the model recovers from).
fn is_parallel_safe(registry: &ToolRegistry, tc: &ToolCall) -> bool {
    match registry.get(&tc.function.name) {
        Some(t) => !t.is_destructive() && t.is_concurrency_safe(),
        None => false,
    }
}

/// Did a destructive tool actually SUCCEED this turn? Arms the verify gate. A denied or errored
/// edit (result starts with `error:`) changed nothing, so it must NOT arm — otherwise the gate
/// would run a typecheck and blame the agent for pre-existing breakage. `results` is in `calls`
/// order (the `execute_calls` contract).
fn turn_made_edits(
    registry: &ToolRegistry,
    calls: &[ToolCall],
    results: &[(String, String)],
) -> bool {
    calls.iter().zip(results).any(|(tc, (_, result))| {
        registry.get(&tc.function.name).map(|t| t.is_destructive()).unwrap_or(false)
            && !result.starts_with("error:")
    })
}

/// Run a batch of (already verified) parallel-safe calls concurrently via `std::thread::scope`
/// (zero new crates; needs `Tool: Send + Sync`). At most `MAX_PARALLEL` threads run at once
/// (the batch is windowed). Each thread runs the unchanged `execute_one`; a panicking tool
/// thread degrades to an error string (fail-soft, no sibling abort).
///
/// Results are stitched back POSITIONALLY (each window's results are aligned with its slice of
/// `calls`, and windows run in order), so the final Vec is in original `calls` order without
/// keying on `tc.id` — deliberately NOT id-keyed, because gateways can emit duplicate or empty
/// tool-call ids (and `ToolCallAccumulator` synthesizes `call_{i}` ids that can collide), which
/// would silently mis-assign results.
fn execute_parallel(
    registry: &ToolRegistry,
    calls: &[ToolCall],
    cfg: &AgentConfig,
) -> Vec<(String, String)> {
    let mut results: Vec<String> = Vec::with_capacity(calls.len());
    for window in calls.chunks(MAX_PARALLEL) {
        let window_results: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = window
                .iter()
                .map(|tc| {
                    scope.spawn(move || {
                        truncate_result(&execute_one(registry, tc, cfg), cfg.max_tool_result_chars)
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|h| h.join().unwrap_or_else(|_| "error: tool thread panicked".to_string()))
                .collect()
        });
        results.extend(window_results);
    }
    calls.iter().map(|tc| tc.id.clone()).zip(results).collect()
}

/// Execute one tool call into a result string (never panics; failures become feedback).
fn execute_one(registry: &ToolRegistry, tc: &ToolCall, cfg: &AgentConfig) -> String {
    // Parse the STRINGIFIED arguments; empty → `{}`.
    let args: serde_json::Value = if tc.function.arguments.trim().is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str(&tc.function.arguments) {
            Ok(v) => v,
            Err(e) => return format!("error: invalid JSON arguments: {e}"),
        }
    };

    let tool = match registry.get(&tc.function.name) {
        Some(t) => t,
        None => return format!("error: unknown tool '{}'", tc.function.name),
    };

    // Shell commands pass the hard safety floor FIRST — before any `/yolo` bypass. A categorically
    // catastrophic command (rm -rf /, mkfs, dd to a raw device, fork bomb, curl|sh) is refused with
    // no override; `smart` mode may auto-clear a read-only command past the approval prompt.
    let mut smart_allow = false;
    // Both `shell_run` and a background `process` start run an arbitrary command → guard them
    // identically, so going background can't sidestep the floor.
    let guarded_command: Option<&str> = match tc.function.name.as_str() {
        "shell_run" => args.get("command").and_then(|v| v.as_str()),
        "process" if args.get("action").and_then(|v| v.as_str()) == Some("start") => {
            args.get("command").and_then(|v| v.as_str())
        }
        _ => None,
    };
    if let Some(command) = guarded_command {
        match cmd_guard::classify(command) {
            cmd_guard::Verdict::Blocked(reason) => {
                let line = format!(
                    "{} {}",
                    style("⛔ blocked").red().bold(),
                    style(format!("{reason} — refused (hard safety floor, not overridable)")).dim()
                );
                if crate::tui::active() {
                    crate::tui::emit_line(&line);
                } else if !cfg.quiet {
                    eprintln!("{line}");
                }
                return format!(
                    "error: blocked by the hard safety floor: {reason}. This command is refused \
                     unconditionally (even under /yolo). Choose a narrower, safer command."
                );
            }
            cmd_guard::Verdict::Allow => smart_allow = cfg.smart_approve,
            cmd_guard::Verdict::Ask => {}
        }
    }

    if tool.is_destructive() && !cfg.auto_approve && !smart_allow && !approve(tool.name(), &args) {
        return "error: the user declined this action".to_string();
    }

    if !cfg.quiet {
        // A clean one-line trace: accent the tool name, dim a human-readable summary of the
        // salient argument (the raw command / file / query — unescaped, first line, clipped).
        let line = format!(
            "{} {}",
            style(format!("⚙ {}", tc.function.name)).color256(crate::splash::ACCENT),
            style(tool_trace(&tc.function.name, &args)).dim()
        );
        if crate::tui::active() {
            crate::tui::emit_line(&line); // into the scroll region above the pinned box
        } else {
            eprintln!("{line}");
        }
    }
    match tool.execute(&args) {
        Ok(out) => out,
        Err(e) => format!("error: {e}"),
    }
}

/// Order-insensitive signature of a turn's tool calls, for divergence detection.
fn turn_signature(calls: &[ToolCall]) -> String {
    let mut sigs: Vec<String> = calls
        .iter()
        .map(|c| format!("{}({})", c.function.name, c.function.arguments.trim()))
        .collect();
    sigs.sort();
    sigs.join("|")
}

/// Rough token estimate (chars/4, no tokenizer dep) over message text — mirrors `main.rs`'s
/// `session_tokens` so the mid-loop guard and the HUD agree on size. Cheap to recompute each turn.
fn estimate_tokens(messages: &[Message]) -> usize {
    let chars: usize =
        messages.iter().filter_map(|m| m.content.as_ref()).map(|c| c.chars().count()).sum();
    chars / 4
}

/// Truncate a tool result to `max` chars, head+tail, marking the elision.
pub fn truncate_result(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if max < 24 {
        return s.chars().take(max).collect();
    }
    let head_len = max * 2 / 3;
    let tail_len = max - head_len;
    let head: String = s.chars().take(head_len).collect();
    let tail: String = {
        let v: Vec<char> = s.chars().collect();
        v[v.len() - tail_len..].iter().collect()
    };
    let omitted = n - head_len - tail_len;
    format!("{head}\n…[{omitted} chars truncated]…\n{tail}")
}

/// Interactive approval for a destructive op. Non-TTY → safe-deny (mirrors the memory
/// subsystem's `confirm_core`): scripts/CI never auto-run a destructive tool. EXCEPTION: under the
/// `ng serve` daemon (non-TTY but Telegram-connected), route the approval to the owner's phone
/// (inline ✓/✗) instead of denying — this is the unattended "approve rm -rf from your phone" path.
fn approve(tool: &str, args: &serde_json::Value) -> bool {
    use std::io::{IsTerminal, Write};
    // Under the sticky TUI the background input thread owns stdin, so we can't run a blocking y/N
    // read inline. Instead, route a per-action prompt THROUGH that thread: `ask_approval` blocks
    // until it presses [y]es / [n]o / [a]llow-all-session. (Destructive tools force the serial path,
    // so we're on a tokio worker where block_in_place is valid — same invariant as the telegram bridge.)
    if crate::tui::active() {
        let prompt = format!(
            "{} {}  {}",
            style(format!("⚙ {tool}")).color256(crate::splash::ACCENT),
            style(tool_trace(tool, args)).dim(),
            style("— approve? [y]es · [n]o · [a]llow all this session").color256(crate::theme::WARN)
        );
        return tokio::task::block_in_place(|| crate::tui::ask_approval(&prompt));
    }
    if !std::io::stdin().is_terminal() {
        if crate::telegram::daemon_is_active() && crate::telegram::is_configured() {
            let prompt = format!("{tool} {}", compact_args(args));
            // Bridge to the async approval on the current (multi-thread) runtime; the serve poll
            // loop runs on another worker and delivers the callback.
            if let Some(v) = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(crate::telegram::request_approval(&prompt))
            }) {
                return v;
            }
        }
        return false;
    }
    print!(
        "{} {}  {} ",
        style(format!("⚙ {tool}")).color256(crate::splash::ACCENT),
        tool_trace(tool, args),
        style("— run it? [y/N]:").dim()
    );
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_lowercase().as_str(), "y" | "yes")
}

fn compact_args(v: &serde_json::Value) -> String {
    let s = serde_json::to_string(v).unwrap_or_default();
    if s.chars().count() > 80 {
        let mut t: String = s.chars().take(77).collect();
        t.push_str("...");
        t
    } else {
        s
    }
}

/// A human-readable one-line trace of a tool call for the TUI — the *salient* argument shown
/// unescaped (real newlines collapsed, clipped), instead of raw escaped JSON. Falls back to
/// `compact_args` for tools whose key field we don't recognise.
fn tool_trace(name: &str, args: &serde_json::Value) -> String {
    let field = |k: &str| args.get(k).and_then(|v| v.as_str());
    let salient = match name {
        "shell_run" | "bash" | "powershell" | "shell" => field("command").or_else(|| field("cmd")),
        "file_edit" | "multi_edit" | "edit_file" | "file_write" | "write_file" | "apply_patch" => {
            field("path").or_else(|| field("file"))
        }
        "file_read" | "read_file" => field("path").or_else(|| field("file")),
        "find_symbols" | "lsp_query" => field("query").or_else(|| field("name")),
        "clarify" | "memory_ask" | "telegram_ask" => field("question"),
        n if n.ends_with("_search") || n == "search" => field("query").or_else(|| field("q")),
        _ => None,
    };
    match salient {
        Some(s) => first_line_clip(s, 72),
        None => compact_args(args),
    }
}

/// First non-blank line of `s`, inner runs of whitespace collapsed, clipped to `max` chars with
/// a `…` marker (and a `↵` hint if more lines followed). Char-safe.
fn first_line_clip(s: &str, max: usize) -> String {
    let first = s.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
    let multiline = s.lines().filter(|l| !l.trim().is_empty()).count() > 1;
    let collapsed = first.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut out: String = collapsed.chars().take(max).collect();
    if collapsed.chars().count() > max {
        out.push('…');
    } else if multiline {
        out.push_str(" ↵");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::tools::Tool;
    use super::*;
    use crate::types::FunctionCall;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // ── test tools ──────────────────────────────────────────────────────────
    struct EchoTool;
    impl Tool for EchoTool {
        fn name(&self) -> &str {
            "echo"
        }
        fn description(&self) -> &str {
            "echo back the `text` arg"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{"text":{"type":"string"}},"required":["text"]})
        }
        fn execute(&self, args: &serde_json::Value) -> Result<String> {
            Ok(args.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string())
        }
    }

    struct FailTool;
    impl Tool for FailTool {
        fn name(&self) -> &str {
            "fail"
        }
        fn description(&self) -> &str {
            "always errors"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{}})
        }
        fn execute(&self, _args: &serde_json::Value) -> Result<String> {
            anyhow::bail!("boom")
        }
    }

    struct DeleteTool;
    impl Tool for DeleteTool {
        fn name(&self) -> &str {
            "delete"
        }
        fn description(&self) -> &str {
            "destructive"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{}})
        }
        fn is_destructive(&self) -> bool {
            true
        }
        fn execute(&self, _args: &serde_json::Value) -> Result<String> {
            Ok("deleted".into())
        }
    }

    /// A stand-in for the real `shell_run` so the `cmd_guard` floor (keyed on the "shell_run" name)
    /// can be exercised. Returns a sentinel iff it actually ran (i.e. wasn't blocked/declined).
    struct ShellStub;
    impl Tool for ShellStub {
        fn name(&self) -> &str {
            "shell_run"
        }
        fn description(&self) -> &str {
            "stub shell"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{"command":{"type":"string"}}})
        }
        fn is_destructive(&self) -> bool {
            true
        }
        fn execute(&self, _args: &serde_json::Value) -> Result<String> {
            Ok("RAN".into())
        }
    }

    // ── helpers ──────────────────────────────────────────────────────────────
    fn tool_turn(name: &str, args: &str) -> ChatTurn {
        ChatTurn {
            content: None,
            tool_calls: vec![ToolCall {
                id: format!("call_{name}"),
                kind: "function".into(),
                function: FunctionCall { name: name.into(), arguments: args.into() },
            }],
            finish_reason: Some("stop".into()), // deliberately NOT "tool_calls" — must still detect
        }
    }
    fn final_turn(text: &str) -> ChatTurn {
        ChatTurn { content: Some(text.into()), tool_calls: vec![], finish_reason: Some("stop".into()) }
    }
    fn call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall { id: id.into(), kind: "function".into(), function: FunctionCall { name: name.into(), arguments: args.into() } }
    }

    fn registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register(Box::new(EchoTool));
        r.register(Box::new(FailTool));
        r.register(Box::new(DeleteTool));
        r
    }

    fn cfg() -> AgentConfig {
        // Verify gate OFF in unit tests (it spawns a real `cargo check`, non-hermetic).
        AgentConfig {
            max_iters: 5,
            auto_extend_to: 5,
            max_tool_result_chars: 4096,
            auto_approve: false,
            smart_approve: false,
            quiet: true,
            enable_verify_gate: false,
            verify_gate_timeout_secs: 90,
            context_window: 0, // guard off by default in tests; the guard test sets it explicitly
        }
    }

    /// A scripted fake model: pops the next turn; empties → a final "stop".
    fn scripted(turns: Vec<ChatTurn>) -> impl Fn(Vec<Message>, Vec<ToolDef>) -> std::future::Ready<Result<ChatTurn>> {
        let q = Mutex::new(VecDeque::from(turns));
        move |_m, _d| {
            let next = q.lock().unwrap().pop_front().unwrap_or_else(|| final_turn("stop"));
            std::future::ready(Ok(next))
        }
    }

    #[test]
    fn hard_floor_blocks_even_under_yolo() {
        // THE security invariant: a catastrophic command is refused even with auto_approve (yolo) ON.
        // The floor runs BEFORE the approval short-circuit, so /yolo cannot bypass it.
        let mut r = ToolRegistry::new();
        r.register(Box::new(ShellStub));
        let mut c = cfg();
        c.auto_approve = true; // yolo
        let out = execute_one(&r, &call("1", "shell_run", r#"{"command":"rm -rf /"}"#), &c);
        assert!(out.contains("blocked by the hard safety floor"), "got: {out}");
        assert!(!out.contains("RAN"), "the command must NOT have executed");
    }

    #[test]
    fn smart_auto_runs_readonly_without_approval() {
        // Under `smart` (and non-TTY, where approve() would otherwise deny), a read-only command runs.
        let mut r = ToolRegistry::new();
        r.register(Box::new(ShellStub));
        let mut c = cfg();
        c.smart_approve = true;
        let out = execute_one(&r, &call("1", "shell_run", r#"{"command":"ls -la"}"#), &c);
        assert_eq!(out, "RAN", "read-only shell should auto-run under smart");
    }

    #[test]
    fn smart_still_asks_for_writes() {
        // A write-shaped command under smart (non-TTY) → safe-deny, NOT auto-run.
        let mut r = ToolRegistry::new();
        r.register(Box::new(ShellStub));
        let mut c = cfg();
        c.smart_approve = true; // not yolo
        let out = execute_one(&r, &call("1", "shell_run", r#"{"command":"rm -rf node_modules"}"#), &c);
        assert!(!out.contains("RAN"), "a write must not auto-run under smart; got: {out}");
    }

    #[tokio::test]
    async fn final_answer_immediately_is_done() {
        let r = registry();
        let out = run_agent(scripted(vec![final_turn("hello")]), &cfg(), &r, "sys", "task")
            .await
            .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(out.final_text.as_deref(), Some("hello"));
        assert_eq!(out.iters, 1);
    }

    #[tokio::test]
    async fn detects_tools_despite_finish_reason_stop_then_finishes() {
        let r = registry();
        let out = run_agent(
            scripted(vec![tool_turn("echo", r#"{"text":"hi"}"#), final_turn("done")]),
            &cfg(),
            &r,
            "sys",
            "task",
        )
        .await
        .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(out.final_text.as_deref(), Some("done"));
        assert_eq!(out.iters, 2);
    }

    #[tokio::test]
    async fn identical_tool_calls_diverge_after_one_recovery() {
        let r = registry();
        // 3 identical: turn1 exec, turn2 nudge (self-resolve), turn3 still identical → diverge.
        let same = || tool_turn("echo", r#"{"text":"x"}"#);
        let out = run_agent(scripted(vec![same(), same(), same()]), &cfg(), &r, "sys", "task")
            .await
            .unwrap();
        assert_eq!(out.stop, StopReason::Divergence);
    }

    #[tokio::test]
    async fn self_resolve_nudge_lets_it_recover() {
        let r = registry();
        // turn1 A, turn2 A (→ nudge), turn3 a DIFFERENT call (model took the hint), turn4 final.
        let out = run_agent(
            scripted(vec![
                tool_turn("echo", r#"{"text":"x"}"#),
                tool_turn("echo", r#"{"text":"x"}"#),
                tool_turn("echo", r#"{"text":"y"}"#),
                final_turn("recovered"),
            ]),
            &cfg(),
            &r,
            "sys",
            "task",
        )
        .await
        .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(out.final_text.as_deref(), Some("recovered"));
    }

    #[tokio::test]
    async fn unknown_tool_error_is_fed_back_not_fatal() {
        let r = registry();
        // turn1 calls a nonexistent tool → loop feeds an error result, keeps going → turn2 finishes.
        let out = run_agent(
            scripted(vec![tool_turn("nope", "{}"), final_turn("recovered")]),
            &cfg(),
            &r,
            "sys",
            "task",
        )
        .await
        .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(out.final_text.as_deref(), Some("recovered"));
    }

    #[tokio::test]
    async fn tool_error_is_fed_back_not_fatal() {
        let r = registry();
        let out = run_agent(
            scripted(vec![tool_turn("fail", "{}"), final_turn("ok")]),
            &cfg(),
            &r,
            "sys",
            "task",
        )
        .await
        .unwrap();
        assert_eq!(out.stop, StopReason::Done);
    }

    #[tokio::test]
    async fn invalid_json_args_is_fed_back_not_fatal() {
        let r = registry();
        let out = run_agent(
            scripted(vec![tool_turn("echo", "{not json"), final_turn("ok")]),
            &cfg(),
            &r,
            "sys",
            "task",
        )
        .await
        .unwrap();
        assert_eq!(out.stop, StopReason::Done);
    }

    #[tokio::test]
    async fn destructive_denied_non_tty_then_model_finishes() {
        let r = registry();
        // auto_approve=false + non-TTY test env → safe-deny → "declined" fed back → model stops.
        let out = run_agent(
            scripted(vec![tool_turn("delete", "{}"), final_turn("stopped")]),
            &cfg(),
            &r,
            "sys",
            "task",
        )
        .await
        .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(out.final_text.as_deref(), Some("stopped"));
    }

    #[tokio::test]
    async fn exhausts_cap_with_distinct_calls() {
        let r = registry();
        // distinct args each turn → no divergence → hits the cap (no auto-extend: to == max).
        let turns = vec![
            tool_turn("echo", r#"{"text":"1"}"#),
            tool_turn("echo", r#"{"text":"2"}"#),
            tool_turn("echo", r#"{"text":"3"}"#),
            tool_turn("echo", r#"{"text":"4"}"#),
            tool_turn("echo", r#"{"text":"5"}"#),
            tool_turn("echo", r#"{"text":"6"}"#),
        ];
        let out = run_agent(scripted(turns), &cfg(), &r, "sys", "task").await.unwrap();
        assert_eq!(out.stop, StopReason::MaxIters);
        assert_eq!(out.iters, 5);
    }

    #[tokio::test]
    async fn auto_extend_grants_more_room() {
        let r = registry();
        let c = AgentConfig { max_iters: 2, auto_extend_to: 4, quiet: true, ..Default::default() };
        // 3 distinct tool turns then finish: would hit max_iters=2, but auto-extend to 4 lets it finish.
        let turns = vec![
            tool_turn("echo", r#"{"text":"1"}"#),
            tool_turn("echo", r#"{"text":"2"}"#),
            tool_turn("echo", r#"{"text":"3"}"#),
            final_turn("done"),
        ];
        let out = run_agent(scripted(turns), &c, &r, "sys", "task").await.unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert!(out.iters > 2, "auto-extend should let it run past the initial cap");
    }

    #[tokio::test]
    async fn context_guard_warns_once_when_window_nearly_full() {
        let r = registry();
        let c = AgentConfig { max_iters: 5, auto_extend_to: 5, quiet: true, context_window: 100, ..Default::default() };
        // Prime an oversized history: ~600 chars ≈ 150 tok, well past 90% of the 100-tok window.
        let mut messages = vec![Message::system("sys"), Message::user("x".repeat(600))];
        // Two tool turns then finish — the guard must fire ONCE, not on every iteration.
        let chat = scripted(vec![
            tool_turn("echo", r#"{"text":"a"}"#),
            tool_turn("echo", r#"{"text":"b"}"#),
            final_turn("done"),
        ]);
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(out.stop, StopReason::Done);
        let warnings = messages
            .iter()
            .filter(|m| m.content.as_deref().is_some_and(|c| c.contains("Context is nearly full")))
            .count();
        assert_eq!(warnings, 1, "the budget nudge must fire exactly once, not per-iteration");
    }

    #[tokio::test]
    async fn context_guard_disabled_when_window_zero() {
        let r = registry();
        // context_window defaults to 0 → guard off even with a huge history.
        let c = AgentConfig { max_iters: 5, auto_extend_to: 5, quiet: true, ..Default::default() };
        let mut messages = vec![Message::system("sys"), Message::user("x".repeat(5000))];
        let chat = scripted(vec![tool_turn("echo", r#"{"text":"a"}"#), final_turn("done")]);
        run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert!(
            !messages.iter().any(|m| m.content.as_deref().is_some_and(|c| c.contains("Context is nearly full"))),
            "guard must stay silent when context_window is 0"
        );
    }

    #[test]
    fn execute_calls_parallel_preserves_order() {
        // 3 read-only echo calls in one turn → parallel batch; results re-stitch in CALL order
        // regardless of completion order.
        let r = registry();
        let calls = vec![
            call("1", "echo", r#"{"text":"first"}"#),
            call("2", "echo", r#"{"text":"second"}"#),
            call("3", "echo", r#"{"text":"third"}"#),
        ];
        let results = execute_calls(&r, &calls, &cfg());
        assert_eq!(results, vec![
            ("1".to_string(), "first".to_string()),
            ("2".to_string(), "second".to_string()),
            ("3".to_string(), "third".to_string()),
        ]);
    }

    #[test]
    fn execute_calls_parallel_is_fail_soft() {
        // echo + fail are both read-only/safe → parallel; one tool's error must not drop its
        // sibling's result (fail-soft, no sibling abort).
        let r = registry();
        let calls = vec![call("1", "echo", r#"{"text":"ok"}"#), call("2", "fail", "{}")];
        let results = execute_calls(&r, &calls, &cfg());
        assert_eq!(results[0], ("1".to_string(), "ok".to_string()));
        assert_eq!(results[1].0, "2");
        assert!(results[1].1.contains("boom"), "tool error fed back, got {:?}", results[1].1);
    }

    #[test]
    fn execute_calls_any_destructive_falls_back_to_serial() {
        // delete is destructive → the WHOLE batch runs serially (approval gating preserved).
        // non-TTY test env → delete safe-denied; the safe sibling still executes, order kept.
        let r = registry();
        let calls = vec![call("1", "echo", r#"{"text":"x"}"#), call("2", "delete", "{}")];
        let results = execute_calls(&r, &calls, &cfg());
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], ("1".to_string(), "x".to_string()));
        assert!(results[1].1.contains("declined"), "destructive denied non-TTY, got {:?}", results[1].1);
    }

    #[test]
    fn execute_calls_single_runs_serial() {
        // a lone safe call takes the serial path (len<2) but yields the same result.
        let r = registry();
        let results = execute_calls(&r, &[call("1", "echo", r#"{"text":"solo"}"#)], &cfg());
        assert_eq!(results, vec![("1".to_string(), "solo".to_string())]);
    }

    #[test]
    fn execute_calls_parallel_above_cap_keeps_order() {
        // > MAX_PARALLEL calls → windowed across multiple scopes; order still preserved.
        let r = registry();
        let calls: Vec<ToolCall> = (0..(MAX_PARALLEL * 2 + 1))
            .map(|i| call(&format!("id{i}"), "echo", &format!(r#"{{"text":"v{i}"}}"#)))
            .collect();
        let results = execute_calls(&r, &calls, &cfg());
        assert_eq!(results.len(), calls.len());
        for (i, (id, val)) in results.iter().enumerate() {
            assert_eq!(id, &format!("id{i}"));
            assert_eq!(val, &format!("v{i}"));
        }
    }

    #[tokio::test]
    async fn loop_runs_a_parallel_tool_turn_then_finishes() {
        // end-to-end through run_agent: one turn emits 3 safe tool calls (parallel path), next
        // turn finishes. Proves the loop wires execute_calls correctly.
        let r = registry();
        let multi = ChatTurn {
            content: None,
            tool_calls: vec![
                call("a", "echo", r#"{"text":"1"}"#),
                call("b", "echo", r#"{"text":"2"}"#),
                call("c", "echo", r#"{"text":"3"}"#),
            ],
            finish_reason: Some("tool_calls".into()),
        };
        let out = run_agent(scripted(vec![multi, final_turn("done")]), &cfg(), &r, "sys", "task")
            .await
            .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(out.final_text.as_deref(), Some("done"));
    }

    #[test]
    fn turn_made_edits_only_on_successful_destructive() {
        // The verify gate arms (made_edits=true) only when a destructive tool SUCCEEDED. A
        // read-only turn, an unknown tool, or a denied/errored destructive op must NOT arm.
        let r = registry();
        let res = |id: &str, s: &str| vec![(id.to_string(), s.to_string())];
        assert!(turn_made_edits(&r, &[call("1", "delete", "{}")], &res("1", "deleted")));
        assert!(
            !turn_made_edits(&r, &[call("1", "delete", "{}")], &res("1", "error: the user declined this action")),
            "a denied/errored destructive op must not arm the gate"
        );
        assert!(!turn_made_edits(&r, &[call("1", "echo", "{}")], &res("1", "hi")), "read-only never arms");
        assert!(!turn_made_edits(&r, &[call("1", "nope", "{}")], &res("1", "error: unknown")), "unknown never arms");
        // mixed: one denied destructive + one successful destructive → arms
        let calls = vec![call("1", "delete", "{}"), call("2", "delete", "{}")];
        let results = vec![("1".to_string(), "error: declined".to_string()), ("2".to_string(), "deleted".to_string())];
        assert!(turn_made_edits(&r, &calls, &results));
    }

    #[test]
    fn execute_calls_parallel_tolerates_duplicate_and_empty_ids() {
        // Some gateways reuse ids / emit empty ids; position-based stitching must return BOTH
        // results in original order, never overwrite or drop one (the HIGH bug the review caught).
        let r = registry();
        let calls = vec![
            call("dup", "echo", r#"{"text":"first"}"#),
            call("dup", "echo", r#"{"text":"second"}"#),
            call("", "echo", r#"{"text":"third"}"#),
        ];
        let results = execute_calls(&r, &calls, &cfg());
        assert_eq!(
            results,
            vec![
                ("dup".to_string(), "first".to_string()),
                ("dup".to_string(), "second".to_string()),
                ("".to_string(), "third".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn read_only_run_does_not_invoke_verify_gate() {
        // Gate ENABLED, but the run only reads → made_edits stays false → gate never fires (so
        // no `cargo check` subprocess), and the loop reports Done normally.
        let r = registry();
        let c = AgentConfig { enable_verify_gate: true, quiet: true, ..cfg() };
        let out = run_agent(
            scripted(vec![tool_turn("echo", r#"{"text":"hi"}"#), final_turn("done")]),
            &c,
            &r,
            "sys",
            "task",
        )
        .await
        .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(out.final_text.as_deref(), Some("done"));
    }

    #[test]
    fn truncate_keeps_head_and_tail() {
        let s = "a".repeat(100) + &"b".repeat(100);
        let t = truncate_result(&s, 60);
        assert!(t.chars().count() < s.chars().count());
        assert!(t.contains("truncated"));
        assert!(t.starts_with('a'));
        assert!(t.ends_with('b'));
    }

    #[test]
    fn truncate_noop_when_short() {
        assert_eq!(truncate_result("short", 4096), "short");
    }

    #[test]
    fn system_prompt_static_prefix_and_blocks() {
        let p1 = build_system_prompt("/a", "linux", "2026-06-20", "m", Some("- terse"));
        assert!(p1.starts_with(SYSTEM_BASE.trim_end()), "static base must lead the prompt");
        assert!(p1.contains("cwd: /a"));
        // check the INJECTED block (the base prose also mentions <user_memory>).
        assert!(p1.contains("\n<user_memory>\n") && p1.contains("- terse"));
        // empty frozen core → no injected user_memory block
        let p2 = build_system_prompt("/a", "linux", "2026-06-20", "m", Some("   "));
        assert!(!p2.contains("\n<user_memory>\n"));
        // static prefix byte-identical regardless of dynamic inputs (prefix-cache safety)
        let p3 = build_system_prompt("/b", "macos", "2026-01-01", "n", None);
        assert!(p3.starts_with(SYSTEM_BASE.trim_end()));
    }

    #[tokio::test]
    async fn clarify_yields_awaiting_input_with_valid_history() {
        // The load-bearing wiring: a `clarify` call PAUSES the loop — it returns AwaitingInput
        // carrying the question, having left a valid (resumable) history ending in the tool result.
        let _g = crate::agent::clarify::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = crate::agent::clarify::take_pending(); // clear any leftover from another test
        let mut r = ToolRegistry::new();
        r.register(Box::new(crate::agent::clarify::Clarify));
        let mut messages = vec![Message::system("sys"), Message::user("ambiguous task")];
        let out = run_agent_loop(
            scripted(vec![
                tool_turn("clarify", r#"{"question":"A or B?","options":["A","B"]}"#),
                final_turn("unreached"), // the loop yields before ever reaching this
            ]),
            &cfg(),
            &r,
            &mut messages,
        )
        .await
        .unwrap();
        match out.stop {
            StopReason::AwaitingInput(q) => {
                assert!(q.starts_with("A or B?"), "carries the question: {q}");
                assert!(q.contains("1. A") && q.contains("2. B"), "carries the options: {q}");
            }
            other => panic!("expected AwaitingInput, got {other:?}"),
        }
        // Resumable: last message is the clarify tool result (the user's next turn continues from
        // here), and the loop drained the pending cell.
        assert_eq!(messages.last().unwrap().role, "tool", "history ends in the tool result");
        assert!(crate::agent::clarify::take_pending().is_none(), "loop drained the pending cell");
    }

    #[test]
    fn signature_is_order_insensitive() {
        let a = vec![
            ToolCall { id: "1".into(), kind: "function".into(), function: FunctionCall { name: "x".into(), arguments: "{}".into() } },
            ToolCall { id: "2".into(), kind: "function".into(), function: FunctionCall { name: "y".into(), arguments: "{}".into() } },
        ];
        let b = vec![
            ToolCall { id: "3".into(), kind: "function".into(), function: FunctionCall { name: "y".into(), arguments: "{}".into() } },
            ToolCall { id: "4".into(), kind: "function".into(), function: FunctionCall { name: "x".into(), arguments: "{}".into() } },
        ];
        assert_eq!(turn_signature(&a), turn_signature(&b));
    }
}
