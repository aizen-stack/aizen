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

pub mod app_catalog;
#[cfg(feature = "browser")]
pub mod browser;
pub mod builtin;
pub mod clarify;
pub mod cmd_guard;
pub mod compact;
pub mod lsp;
pub mod mcp;
pub mod mcp_oauth;
pub mod process;
pub mod project_context;
pub mod reach;
pub mod repo_map;
pub mod search;
pub mod task_tool;
pub mod todo;
pub mod tools;
pub mod verify_gate;
pub mod web_tools;
pub mod workflow;
pub mod workflow_tool;

use crate::llm::client::ChatTurn;
use crate::core::types::{Message, ToolCall, ToolDef};
use anyhow::Result;
use console::style;
use std::future::Future;
use tools::ToolRegistry;

/// The static (cached-prefix) base system prompt — see `system-prompt.md`.
pub const SYSTEM_BASE: &str = include_str!("system_prompt.md");

/// The STRICT-tier base prompt for small/local models (numbered imperative rules, explicit output
/// contract, tool cheat sheet) — weak models follow commands, not essays. Selected by
/// [`prompt_tier_for`]; ~half the tokens of the full prompt.
pub const SYSTEM_BASE_STRICT: &str = include_str!("system_prompt_strict.md");

/// Which base prompt a model gets. One prompt cannot serve both Claude and a 7B local model —
/// the opencode precedent (`qwen.txt` fallback) applied to aizen's arbitrary-endpoint reality.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PromptTier {
    Full,
    Strict,
}

/// Pick the prompt tier: explicit config override (`"full"`/`"strict"`) wins; otherwise a
/// word-boundary heuristic over the model id — known small/local families and small parameter
/// suffixes go strict. UNKNOWN ⇒ Full (the safe default: a strong model on strict prose loses
/// more than a weak one on full prose).
pub fn prompt_tier_for(model: &str, override_tier: Option<&str>) -> PromptTier {
    match override_tier.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("strict") => return PromptTier::Strict,
        Some("full") => return PromptTier::Full,
        _ => {}
    }
    let m = model.to_ascii_lowercase();
    // Small/local families + the "mini"/"nano" tier markers, matched as whole tokens so e.g.
    // "geminia" or "granite-cloud-ultra" never false-positives on a substring.
    const STRICT_FAMILIES: &[&str] = &["qwen", "llama", "gemma", "phi", "granite", "smollm", "mini", "nano"];
    if STRICT_FAMILIES.iter().any(|k| crate::llm::client::contains_word(&m, k)) {
        return PromptTier::Strict;
    }
    if m.contains("mistral-small") {
        return PromptTier::Strict;
    }
    // Explicit parameter-count suffixes (whole tokens): anything ≤32B gets the strict tier.
    const SMALL_SIZES: &[&str] =
        &["1b", "2b", "3b", "4b", "7b", "8b", "9b", "12b", "13b", "14b", "24b", "27b", "30b", "32b"];
    if SMALL_SIZES.iter().any(|k| crate::llm::client::contains_word(&m, k)) {
        return PromptTier::Strict;
    }
    PromptTier::Full
}

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
    // Tier is a pure function of (model, config): fixed within a session, so the prefix stays
    // byte-stable; every model switch already rebuilds the system prompt.
    let base = match prompt_tier_for(model, crate::core::cli_config::load().prompt_tier.as_deref()) {
        PromptTier::Strict => SYSTEM_BASE_STRICT,
        PromptTier::Full => SYSTEM_BASE,
    };
    let mut s = String::from(base.trim_end());
    s.push_str("\n\n<environment>\n");
    s.push_str(&format!("cwd: {cwd}\nos: {os}\ndate: {date}\nmodel: {model}\n"));
    s.push_str("</environment>\n");
    // Durable AGENT operating-identity (who the agent IS across every persona/project) — ABOVE the
    // persona costume and the user model. HOME-only + sanitized + fail-closed (see `crate::persona::soul`).
    if let Some(soul) = crate::persona::soul::prompt_block() {
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
    if let Some(idx) = crate::skills::prompt_index() {
        s.push_str("\n<skills>\n");
        s.push_str(&idx);
        s.push_str("\n</skills>\n");
    }
    s
}

/// The SLIM sub-agent base: static base (tiered) + `<environment>` + `<skills>` — deliberately NO
/// soul / persona / self / user_memory (a focused sub-agent pays no identity-costume tax: up to
/// ~1.1K tokens saved per spawn, and personal facts never leak into task context) and NO
/// `<agents>` / auto `<project_context>` (top-level-only concerns). `include_project_context`
/// opts a role in explicitly — build/test conventions are exactly what coder/tester need.
pub fn build_subagent_base_prompt(
    cwd: &str,
    os: &str,
    date: &str,
    model: &str,
    include_project_context: bool,
) -> String {
    let base = match prompt_tier_for(model, crate::core::cli_config::load().prompt_tier.as_deref()) {
        PromptTier::Strict => SYSTEM_BASE_STRICT,
        PromptTier::Full => SYSTEM_BASE,
    };
    let mut s = String::from(base.trim_end());
    s.push_str("\n\n<environment>\n");
    s.push_str(&format!("cwd: {cwd}\nos: {os}\ndate: {date}\nmodel: {model}\n"));
    s.push_str("</environment>\n");
    if let Some(idx) = crate::skills::prompt_index() {
        s.push_str("\n<skills>\n");
        s.push_str(&idx);
        s.push_str("\n</skills>\n");
    }
    if include_project_context {
        if let Some(ctx) = project_context::load_project_context(std::path::Path::new(cwd)) {
            s.push_str("\n<project_context>\n");
            s.push_str(&ctx);
            s.push_str("\n</project_context>\n");
        }
    }
    s
}

/// The TOP-LEVEL system prompt: [`build_system_prompt`] plus the `<agents>` index of delegatable
/// specialists. A thin wrapper (NOT a new `build_system_prompt` parameter) so the shared prefix the
/// four existing callers depend on stays byte-stable, and so SUB-AGENTS — which call
/// `build_system_prompt` directly and CANNOT delegate — never see `<agents>`. The block is a pure
/// SUFFIX and absent entirely when no agents are installed, so a user without the feature gets a
/// byte-identical prompt (zero prefix-cache bust).
pub fn build_top_level_system_prompt(
    cwd: &str,
    os: &str,
    date: &str,
    model: &str,
    frozen_core: Option<&str>,
) -> String {
    let mut s = build_system_prompt(cwd, os, date, model, frozen_core);
    // Project conventions (AGENTS.md / CLAUDE.md), top-level only: coder turns inherit the repo's
    // build/test commands and house rules. Pure SUFFIX, absent when no conventions file exists, so
    // a project without one keeps the byte-identical prefix (zero prefix-cache bust).
    if let Some(ctx) = project_context::load_project_context(std::path::Path::new(cwd)) {
        s.push_str("\n<project_context>\n");
        s.push_str(&ctx);
        s.push_str("\n</project_context>\n");
    }
    if let Some(idx) = crate::agents::prompt_index() {
        s.push_str("\n<agents>\n");
        s.push_str(&idx);
        s.push_str("\n</agents>\n");
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
    /// Tool-result clearing: keep the most recent N tool results verbatim; OLDER ones whose body
    /// exceeds `clear_tool_result_min_chars` have their content evicted (the message + `tool_call_id`
    /// stay intact) once history crosses `clear_at_pct` of `context_window`. The cheap,
    /// deterministic first line of defense before summarization compaction. `0` disables clearing.
    pub keep_recent_tool_results: usize,
    /// Min chars before an OLD tool result is worth clearing (small results aren't worth the churn).
    pub clear_tool_result_min_chars: usize,
    /// Clearing arm threshold as a percent of `context_window`. `0` disables clearing.
    pub clear_at_pct: u8,
    /// Batch-clear DOWN TO this percent of the window in one pass (the floor). Big infrequent
    /// mutations beat a per-turn trickle: every mid-history rewrite busts the provider prompt
    /// cache from that byte onward, so clearing rarely-but-thoroughly keeps hit rates alive.
    pub clear_target_pct: u8,
    /// Re-fire only after history grows this many percentage points past the last clear…
    pub clear_step_pct: u8,
    /// …or after this many loop iterations since the last clear, whichever comes first.
    pub clear_cooldown_iters: usize,
    /// Re-show the todo list as a tail reminder every N loop iterations on long runs (recitation
    /// keeps the goal in the model's recent-attention span). `0` disables.
    pub todo_reminder_every: usize,
    /// Mid-loop auto-compaction trigger as a percent of `context_window` (e.g. 80). When history
    /// crosses this AND a summarizer is supplied (via `run_agent_loop_compacting`), older turns are
    /// summarized in place. `0` disables compaction (the loop falls back to the one-shot wrap-up
    /// nudge). Requires `context_window > 0`.
    pub compact_at_pct: u8,
    /// Max gate-triggered fix rounds in the verify/repair loop: after an editing run, a failing
    /// typecheck injects the errors and loops back for a fix, up to this many times (then the model
    /// is allowed to finish). `1` = the old one-shot behavior; `0` disables looping entirely (the
    /// gate still needs `enable_verify_gate`). Only re-fires after the model makes NEW edits.
    pub max_verify_attempts: usize,
    /// One extra SELF-REVIEW turn before Done on runs that edited files (opt-in): re-check the
    /// diff against the request — via the `roles.oracle` model when configured, else a nudge to
    /// this model. Costs one turn per editing task; default OFF until measured worth it.
    pub enable_self_review: bool,
    /// Enable the LSP subsystem (type-aware symbol navigation via a per-language server). Default
    /// OFF: when false, no language server is ever spawned and the LSP tools aren't registered, so
    /// the agent behaves exactly as without LSP. Servers are spawned lazily per project language
    /// only after this is on AND a query needs one. Sub-agents/workflows inherit `false`.
    pub enable_lsp: bool,
    /// Per-request wall-clock cap (seconds) for an LSP query, so a hung server can never block the
    /// agent turn. Mirrors Helix's 20s default.
    pub lsp_request_timeout_secs: u64,
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
            keep_recent_tool_results: 8,
            clear_tool_result_min_chars: 1024,
            clear_at_pct: 60,
            clear_target_pct: 45,
            clear_step_pct: 10,
            clear_cooldown_iters: 6,
            todo_reminder_every: 8,
            compact_at_pct: 80,
            max_verify_attempts: 2,
            enable_self_review: false,
            enable_lsp: false,
            lsp_request_timeout_secs: 20,
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
    /// The model's final answer (already streamed to stdout by `ng agent`; consumed by the
    /// programmatic callers — `main.rs` one-shot, workflow/task sub-agent collection).
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

/// Function-pointer type for the "no summarizer" case, so [`run_agent_loop`]'s signature stays
/// unchanged for its existing callers/tests — compaction is simply OFF on that path.
type NoSummarizer = fn(Vec<Message>) -> std::future::Ready<Result<String>>;

/// The agent loop over an EXISTING conversation: `messages` already holds the system prompt and
/// every turn so far (the unified chat+agent REPL appends a user turn then calls this). Drives
/// tool calls to a final answer; on Done the final assistant text is pushed onto `messages` so a
/// multi-turn caller keeps context. Single-shot callers go through `run_agent`. No mid-loop
/// compaction (tool-result clearing + the wrap-up nudge still apply) — see
/// [`run_agent_loop_compacting`].
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
    let no_summarizer: Option<NoSummarizer> = None;
    run_agent_loop_inner(chat, no_summarizer, None::<NoSummarizer>, cfg, registry, messages).await
}

/// Like [`run_agent_loop`] but with MID-LOOP auto-compaction: when history crosses
/// `cfg.compact_at_pct` of the window, `summarize` (a NON-streaming model call returning the summary
/// text) compresses older turns in place. For multi-turn callers (e.g. `ng serve`) whose sessions
/// can outgrow the window within a single driven turn. Compaction needs ≥2 user turns to cut, so a
/// single-task run (one user turn) falls back to tool-result clearing + the wrap-up nudge.
pub async fn run_agent_loop_compacting<F, Fut, S, SFut>(
    chat: F,
    summarize: S,
    cfg: &AgentConfig,
    registry: &ToolRegistry,
    messages: &mut Vec<Message>,
) -> Result<AgentOutcome>
where
    F: Fn(Vec<Message>, Vec<ToolDef>) -> Fut,
    Fut: Future<Output = Result<ChatTurn>>,
    S: Fn(Vec<Message>) -> SFut,
    SFut: Future<Output = Result<String>>,
{
    run_agent_loop_inner(chat, Some(summarize), None::<NoSummarizer>, cfg, registry, messages).await
}

/// The full-featured entry point: compaction PLUS an optional ORACLE — a (usually stronger) model
/// the opt-in self-review pass consults with the final diff before Done. `None` oracle ⇒
/// self-review (when enabled) degrades to the nudge mode (the model re-reads its own diff).
pub async fn run_agent_loop_full<F, Fut, S, SFut, O, OFut>(
    chat: F,
    summarize: S,
    oracle: Option<O>,
    cfg: &AgentConfig,
    registry: &ToolRegistry,
    messages: &mut Vec<Message>,
) -> Result<AgentOutcome>
where
    F: Fn(Vec<Message>, Vec<ToolDef>) -> Fut,
    Fut: Future<Output = Result<ChatTurn>>,
    S: Fn(Vec<Message>) -> SFut,
    SFut: Future<Output = Result<String>>,
    O: Fn(Vec<Message>) -> OFut,
    OFut: Future<Output = Result<String>>,
{
    run_agent_loop_inner(chat, Some(summarize), oracle, cfg, registry, messages).await
}

async fn run_agent_loop_inner<F, Fut, S, SFut, O, OFut>(
    chat: F,
    summarize: Option<S>,
    oracle: Option<O>,
    cfg: &AgentConfig,
    registry: &ToolRegistry,
    messages: &mut Vec<Message>,
) -> Result<AgentOutcome>
where
    F: Fn(Vec<Message>, Vec<ToolDef>) -> Fut,
    Fut: Future<Output = Result<ChatTurn>>,
    S: Fn(Vec<Message>) -> SFut,
    SFut: Future<Output = Result<String>>,
    O: Fn(Vec<Message>) -> OFut,
    OFut: Future<Output = Result<String>>,
{
    let defs = registry.defs();
    // Tool schemas ride on EVERY request but live in no message — count them once here so the
    // context guards below compare the real request size against the window.
    let schema_overhead = estimate_defs_tokens(&defs);
    let mut cap = cfg.max_iters;
    let mut extended = false;
    let mut last_sig: Option<String> = None;
    let mut recovery_used = false;
    let mut verify_attempts = 0usize;
    let mut made_edits = false;
    // CUMULATIVE edit flag (never consumed, unlike `made_edits`) — arms the one-shot self-review.
    let mut made_any_edits = false;
    let mut self_review_done = false;
    let mut context_warned = false;
    // Provider-reported prompt size at the last usage-carrying call (see `RealAnchor`) —
    // invalidated whenever history is mutated (clearing/compaction shrink what we'd send next).
    let mut real_anchor: Option<RealAnchor> = None;
    // Clearing cadence: (pct-of-window after the last clear, iter at the last clear).
    let mut last_clear: Option<(usize, usize)> = None;
    // Iter of the last todo-recitation reminder (0 = none yet).
    let mut last_todo_reminder = 0usize;
    let mut iter = 0usize;

    while iter < cap {
        // COOPERATIVE CANCEL: a synchronous tool (a long `shell_run`) just aborted because the user
        // pressed Esc. The REPL's `select!` couldn't observe the cancel while that tool ran (no
        // await), so yield HERE — the yield is an await point, letting the `select!` win and drop
        // this turn before we'd otherwise fire another model call. (No-op outside the sticky REPL,
        // where the flag is never set.)
        if crate::ui::tui::cancel_requested() {
            tokio::task::yield_now().await;
        }

        // Effective request size for ALL guards this iteration: estimate (messages + tool schemas)
        // corrected by the provider's last real usage report when we have one. Recomputed after any
        // guard mutates history.
        let mut est_now =
            effective_tokens(estimate_tokens(messages) + schema_overhead, real_anchor.as_ref());

        // TOOL-RESULT CLEARING (cheap, deterministic — runs BEFORE the model call): once history
        // crosses `clear_at_pct` of the window, batch-evict stale tool-result bodies DOWN TO
        // `clear_target_pct` in one pass. Big infrequent jumps, not a per-turn trickle — every
        // mid-history rewrite invalidates the provider prompt cache from that byte onward, so the
        // cadence (`clear_step_pct` growth or `clear_cooldown_iters`) is what keeps hit rates
        // alive. Error-aware: bulky successes go first; failures are only trimmed (first line
        // survives) when successes alone can't reach the floor. Off when context_window /
        // keep_recent / clear_at_pct is 0.
        if cfg.context_window > 0
            && cfg.keep_recent_tool_results > 0
            && cfg.clear_at_pct > 0
            && est_now * 100 >= cfg.context_window * cfg.clear_at_pct as usize
        {
            let pct = est_now * 100 / cfg.context_window;
            if clearing_due(pct, iter, last_clear, cfg.clear_step_pct, cfg.clear_cooldown_iters) {
                // The floor measures history in RAW estimate units (chars/4), but `est_now` — which
                // armed this pass — is anchor-corrected. With an active anchor, effective = raw + K
                // for a constant offset K (= real-minus-estimate at anchor time), so a target
                // expressed in window units must be shifted into raw space by that same K; otherwise
                // raw is already below the window target and the eviction loop no-ops every cadence
                // step (common once the provider reports more tokens than chars/4 — code, Vietnamese).
                // No anchor ⇒ est_now == raw ⇒ offset 0 ⇒ identical to the plain window target.
                let raw_now = estimate_tokens(messages) + schema_overhead;
                let anchor_offset = est_now.saturating_sub(raw_now);
                let target = (cfg.context_window * cfg.clear_target_pct as usize / 100)
                    .saturating_sub(anchor_offset);
                let stats = clear_tool_results_to_floor(
                    messages,
                    cfg.keep_recent_tool_results,
                    cfg.clear_tool_result_min_chars,
                    target,
                    schema_overhead,
                );
                if stats.cleared + stats.failures_trimmed > 0 {
                    // History shrank under the anchor's feet — the next real usage report re-anchors.
                    real_anchor = None;
                    est_now = estimate_tokens(messages) + schema_overhead;
                    if !cfg.quiet {
                        let line = format!(
                            "→ context: cleared ~{} chars ({} result(s), {} failure(s) trimmed)",
                            stats.chars_reclaimed, stats.cleared, stats.failures_trimmed
                        );
                        if crate::ui::tui::active() {
                            crate::ui::tui::emit_line(&line);
                        } else {
                            eprintln!("{line}");
                        }
                    }
                }
                // Arm the cadence even when nothing was clearable — re-scanning the same
                // un-clearable history every iteration buys nothing.
                last_clear = Some((est_now * 100 / cfg.context_window, iter));
            }
        }

        // MID-LOOP AUTO-COMPACTION (multi-turn callers only): once history crosses `compact_at_pct`
        // of the window, summarize older turns in place (keeping the last KEEP_TURNS verbatim) —
        // cheaper than overflowing, and it carries forward more than the wrap-up nudge. Falls through
        // when the conversation is too short to cut (one user turn → clearing above is its defense)
        // or when no summarizer was supplied (the plain `run_agent_loop` path).
        if let Some(ref summarize) = summarize {
            if cfg.compact_at_pct > 0
                && cfg.context_window > 0
                && est_now * 100 >= cfg.context_window * cfg.compact_at_pct as usize
            {
                if let Ok((before, after)) =
                    compact::compact_history(messages, summarize, compact::KEEP_TURNS).await
                {
                    context_warned = false; // history shrank — let the wrap-up nudge re-arm if it refills
                    real_anchor = None; // spliced history invalidates the anchor
                    est_now = estimate_tokens(messages) + schema_overhead;
                    if !cfg.quiet {
                        let line = format!("→ context: auto-compacted ~{before} → ~{after} tok");
                        if crate::ui::tui::active() {
                            crate::ui::tui::emit_line(&line);
                        } else {
                            eprintln!("{line}");
                        }
                    }
                }
            }
        }

        // MID-LOOP CONTEXT GUARD: a single run-away loop (e.g. reading many large files) can blow
        // past the window BEFORE control returns to the REPL's auto-compact (which only runs
        // between turns). When the running history crosses ~90% of the window, inject a ONE-TIME
        // "wrap up" nudge so the model acts on what it has rather than overflowing. Pure arithmetic
        // (chars/4 + the real-usage anchor) — NOT a mid-loop summarization model call, and no
        // tokenizer dep. Disabled when `context_window == 0` (sub-agents / unconfigured).
        let nudge_pushed =
            if cfg.context_window > 0 && !context_warned && est_now * 100 >= cfg.context_window * 90 {
                context_warned = true;
                push_nudge(
                    messages,
                    NUDGE_CONTEXT,
                    "Context is nearly full (~90% of the window). Wrap up now: stop gathering more, act \
                     on what you already have, and give your final answer — or state what is blocking you.",
                );
                true
            } else {
                false
            };

        // Roll back the just-appended nudge if the model call fails, so a network/gateway error
        // doesn't strand an unanswered system message at the tail of history (the REPL's error path
        // only pops a trailing `user` message, so it wouldn't clean this up).
        let mut turn = match chat(messages.clone(), defs.clone()).await {
            Ok(t) => t,
            Err(e) => {
                if nudge_pushed {
                    messages.pop();
                }
                return Err(e);
            }
        };

        // REAL-USAGE ANCHOR: when the provider reports how many prompt tokens THIS request really
        // was, trust that over chars/4 — the guards then track growth as (estimate delta) on top of
        // the real base. `est_now` was the estimate of the exact request just sent, so the pair is
        // coherent. Insane reports (cumulative gateways, tool-exclusive counts) fail the clamp.
        if let Some(p) = turn.usage.as_ref().and_then(|u| u.prompt_tokens) {
            let real = p as usize;
            if accept_anchor(real, est_now) {
                real_anchor = Some(RealAnchor { tokens: real, est_at: est_now });
            }
        }

        // Per-turn token trace: real provider numbers + the live cache hit-rate. This is the
        // KV-cache health signal — a sudden drop to 0% cached mid-session means something is
        // rewriting the prefix (and multiplying cost/latency).
        if !cfg.quiet {
            if let Some(u) = &turn.usage {
                if let Some(p) = u.prompt_tokens {
                    let cached = u.cache_read();
                    let cache_part = if cached > 0 && p > 0 {
                        format!(" · {} cached ({}%)", fmt_tok(cached), cached * 100 / p)
                    } else {
                        String::new()
                    };
                    let line = format!(
                        "→ tokens: {} in{cache_part} · {} out",
                        fmt_tok(p),
                        fmt_tok(u.completion_tokens.unwrap_or(0))
                    );
                    if crate::ui::tui::active() {
                        crate::ui::tui::emit_line(&line);
                    } else {
                        eprintln!("{line}");
                    }
                }
            }
        }

        // CLASSIFY: the structured tool_calls array is the source of truth (a gateway may
        // emit finish_reason="stop"/"end_turn" alongside tool calls — ignore it here).
        if turn.tool_calls.is_empty() {
            // VERIFY/REPAIR GATE (F2): after an editing run, run a fast typecheck before Done. On
            // failure, record the premature "done", inject the errors, and loop back for a fix — up
            // to `max_verify_attempts` rounds (a bounded repair loop, not one-shot). `made_edits` is
            // consumed here so the gate re-fires only after the model makes NEW edits; a model that
            // re-asserts "done" without editing is allowed to finish. Best-effort: an unknown
            // project / missing toolchain → no-op.
            if cfg.enable_verify_gate && made_edits && verify_attempts < cfg.max_verify_attempts {
                made_edits = false; // consume — re-arm only on fresh edits next round
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
                            "→ verify: {} {} (attempt {}/{})",
                            result.command,
                            if result.passed { "passed" } else { "FAILED" },
                            verify_attempts + 1,
                            cfg.max_verify_attempts,
                        );
                        if crate::ui::tui::active() {
                            crate::ui::tui::emit_line(&line);
                        } else {
                            eprintln!("{line}");
                        }
                    }
                    if !result.passed {
                        verify_attempts += 1;
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
            // SELF-REVIEW (opt-in, once per run): after the verify gate is satisfied and before
            // Done, spend ONE extra turn checking the work against the original request. Oracle
            // mode (roles.oracle configured → closure supplied) has a stronger model review the
            // `git diff` — its findings come back as a fix-or-rebut turn; an LGTM costs nothing
            // extra. Nudge mode makes THIS model re-read its own diff.
            if cfg.enable_self_review && made_any_edits && !self_review_done {
                self_review_done = true;
                let injected = match &oracle {
                    Some(o) => oracle_review(o, messages).await.map(|findings| {
                        format!(
                            "[self-review]\n{findings}\n\nFix anything valid above, or state briefly why a point does not apply — then give your final answer."
                        )
                    }),
                    None => Some(SELF_REVIEW_NUDGE.to_string()),
                };
                if let Some(text) = injected {
                    // Record the premature "done" so the review turn reads coherently.
                    if let Some(t) = &turn.content {
                        if !t.trim().is_empty() {
                            messages.push(Message::assistant(t.clone()));
                        }
                    }
                    messages.push(Message::user(text));
                    iter += 1;
                    continue;
                }
                // Oracle said LGTM (or no diff to review) → fall through to Done, no extra turn.
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
                push_nudge(
                    messages,
                    NUDGE_DIVERGENCE,
                    "You repeated the same tool call(s) with no new information. Take a DIFFERENT approach, or stop and explain what is blocking you.",
                );
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

        // PRE-FILL: append the assistant tool-call turn AND one placeholder result per call in a
        // single synchronous block (no await in between) — history is VALID from this instant, so
        // the REPL's `select!` dropping this future mid-batch (Esc) can never leave a dangling
        // `assistant.tool_calls` (strict gateways 400 on that). Real results overwrite the
        // placeholders as they land inside `execute_calls`.
        let calls = turn.tool_calls.clone();
        messages.push(Message {
            role: "assistant".to_string(),
            content: turn.content.clone(),
            tool_calls: calls.clone(),
            tool_call_id: None,
            images: Vec::new(),
            cache_control: None,
        });
        let base = messages.len();
        for tc in &calls {
            messages.push(Message::tool_result(tc.id.clone(), INTERRUPTED_TOOL_PLACEHOLDER.to_string()));
        }

        // EXECUTE the call(s): barrier-partitioned — consecutive read-only calls run concurrently
        // (spawn_blocking, raced against Esc); each write/shell call is a barrier executed alone
        // with approval on THIS future. Eager starts from the streaming path are adopted by
        // position. Results land in ORIGINAL call order. (A DISCARDED turn — divergence/error —
        // simply drops its eager handles: detached, read-only, harmless.)
        let eager = std::mem::take(&mut turn.eager);
        let results = execute_calls(registry, &calls, cfg, &mut messages[base..], eager).await;

        // Arm the verify gate only if a destructive tool actually SUCCEEDED this turn — a
        // denied/errored edit changed nothing, so it must not make the gate blame the tree.
        if turn_made_edits(registry, &calls, &results) {
            made_edits = true;
            made_any_edits = true;
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

        // TODO RECITATION: on long runs, re-show the task list near the context TAIL every
        // `todo_reminder_every` iterations — the model's recent-attention span is where goals
        // stop drifting (the Manus "recitation" lesson). Replaced via push_nudge, never accreted;
        // skipped on turns that already touched the list. Pairing-safe: inserted after the tool
        // results, so assistant↔tool adjacency is untouched.
        if cfg.todo_reminder_every > 0
            && iter.saturating_sub(last_todo_reminder) >= cfg.todo_reminder_every
            && !calls.iter().any(|c| c.function.name == "todo_write")
        {
            let items = todo::snapshot();
            if !items.is_empty() {
                let mut text = format!("{NUDGE_TODO} (flip items as you finish them):\n");
                for t in &items {
                    let mark = match t.status {
                        todo::Status::Done => "[x]",
                        todo::Status::InProgress => "[>]",
                        todo::Status::Pending => "[ ]",
                    };
                    text.push_str(&format!("{mark} {}\n", t.content));
                    if text.chars().count() > 600 {
                        text.push_str("…\n");
                        break;
                    }
                }
                push_nudge(messages, NUDGE_TODO, text.trim_end());
                last_todo_reminder = iter;
            }
        }

        // CONVERGENCE: near the cap, nudge once and grant the extended cap (pressure, not gate).
        if iter >= cap && !extended && cfg.auto_extend_to > cap {
            extended = true;
            cap = cfg.auto_extend_to;
            push_nudge(
                messages,
                NUDGE_STEP_LIMIT,
                "You are nearing the step limit. Finish the task now, or stop and state what is blocking you.",
            );
        }
    }

    Ok(AgentOutcome { final_text: None, iters: iter, stop: StopReason::MaxIters })
}

/// Hard cap on concurrent tool executions in a parallel run. Conservative for a single-binary
/// CLI whose safe tools are I/O-bound (file reads, network fetches) — enough to overlap latency
/// without oversubscribing. Not configurable in v1 (no measured need for a `--threads` flag).
const MAX_PARALLEL: usize = 5;

/// The pre-fill placeholder for a tool result whose call never completed (the turn future was
/// dropped mid-batch by Esc). Honest AND pairing-valid — strict gateways accept the history.
const INTERRUPTED_TOOL_PLACEHOLDER: &str = "error: interrupted before completion";

/// Execute a turn's tool calls, returning `(tool_call_id, result_text)` in ORIGINAL call order,
/// writing each result into `sink` (the pre-filled placeholder messages, one per call) the moment
/// it lands — so a dropped future loses at most the still-running calls, never a completed one.
///
/// BARRIER PARTITION (sequential observability preserved): walking the calls in order,
/// consecutive parallel-safe calls form a concurrent run (windowed ≤ MAX_PARALLEL, each body on a
/// `spawn_blocking` thread, raced against Esc); every destructive / non-concurrency-safe /
/// unknown call is a BARRIER executed alone — cmd_guard + approval stay on THIS future (the
/// approval prompt runs inside `block_in_place`, so the future is busy and un-droppable during
/// it), and its body is awaited UN-raced (a destructive op's real outcome must be recorded).
/// `[r1 r2 W r3 r4]` ⇒ {r1,r2} concurrent → W → {r3,r4} concurrent: a read after a write always
/// observes post-write state, and writes keep their original relative order. Once a cancel is
/// observed, every remaining call reports "cancelled" without executing — the results vec is
/// always complete and position-aligned (deliberately NOT id-keyed: gateways emit duplicate/empty
/// ids).
async fn execute_calls(
    registry: &ToolRegistry,
    calls: &[ToolCall],
    cfg: &AgentConfig,
    sink: &mut [Message],
    eager: Vec<(usize, tokio::task::JoinHandle<String>)>,
) -> Vec<(String, String)> {
    debug_assert_eq!(sink.len(), calls.len(), "one pre-filled placeholder per call");
    // Eager starts from the streaming path, keyed by position — adopted instead of re-spawned.
    // Their bodies ran quiet; the executor emits the trace at adoption and the result marker at
    // landing so the UX is indistinguishable from a normal run.
    let mut adopted: std::collections::HashSet<usize> = std::collections::HashSet::new();
    let mut eager: std::collections::HashMap<usize, tokio::task::JoinHandle<String>> =
        eager.into_iter().collect();
    // Parse every call's arguments ONCE — used for the safety partition, the gate, and the body.
    let parsed: Vec<Result<serde_json::Value, String>> =
        calls.iter().map(|tc| parse_call_args(&tc.function.arguments)).collect();
    let safe: Vec<bool> = calls
        .iter()
        .zip(&parsed)
        .map(|(tc, args)| match (registry.get(&tc.function.name), args) {
            // Unknown tool / bad args → barrier (the error string surfaces on the serial path).
            (Some(t), Ok(a)) => !t.is_destructive() && t.is_concurrency_safe_for(a),
            _ => false,
        })
        .collect();

    let n = calls.len();
    let mut results: Vec<Option<String>> = (0..n).map(|_| None).collect();
    let land = |k: usize, out: String, results: &mut Vec<Option<String>>, sink: &mut [Message]| {
        sink[k].content = Some(out.clone());
        results[k] = Some(out);
    };

    let mut i = 0usize;
    let mut cancelled = false;
    while i < n {
        if cancelled || crate::ui::tui::cancel_requested() {
            cancelled = true;
            land(i, "error: cancelled by user".to_string(), &mut results, sink);
            i += 1;
            continue;
        }
        if safe[i] {
            // A concurrent run of consecutive safe calls, windowed at MAX_PARALLEL.
            let mut j = i;
            while j < n && safe[j] {
                j += 1;
            }
            'windows: for start in (i..j).step_by(MAX_PARALLEL) {
                let end = (start + MAX_PARALLEL).min(j);
                let handles: Vec<(usize, tokio::task::JoinHandle<String>)> = (start..end)
                    .map(|k| {
                        // ADOPT an eager start when the streaming path already launched this call
                        // (its body ran quiet — emit the standard trace here so the UX is uniform).
                        if let Some(h) = eager.remove(&k) {
                            adopted.insert(k);
                            if !cfg.quiet {
                                if let (Some(tool), Ok(args)) = (registry.get(&calls[k].function.name), &parsed[k]) {
                                    emit_trace(&tool_call_line(tool.name(), args));
                                }
                            }
                            return (k, h);
                        }
                        let tool = registry.get_arc(&calls[k].function.name).expect("safe ⇒ known");
                        let args = parsed[k].clone().expect("safe ⇒ parsed");
                        let quiet = cfg.quiet;
                        let max = cfg.max_tool_result_chars;
                        (k, tokio::task::spawn_blocking(move || run_tool_body(tool, &args, quiet, max)))
                    })
                    .collect();
                for (k, h) in handles {
                    let out = tokio::select! {
                        r = h => r.unwrap_or_else(|_| "error: tool thread panicked".to_string()),
                        _ = crate::ui::tui::cancelled() => {
                            // The blocking body keeps running detached; safe calls are read-only,
                            // so discarding the result is harmless.
                            "error: cancelled by user".to_string()
                        }
                    };
                    if adopted.contains(&k) && !cfg.quiet {
                        emit_tool_result(&calls[k].function.name, &out); // eager body ran quiet
                    }
                    land(k, out, &mut results, sink);
                }
                if crate::ui::tui::cancel_requested() {
                    cancelled = true;
                    break 'windows;
                }
            }
            // Fill any slots of the run skipped by a mid-run cancel.
            for k in i..j {
                if results[k].is_none() {
                    land(k, "error: cancelled by user".to_string(), &mut results, sink);
                }
            }
            i = j;
        } else {
            // BARRIER: gate + approve on this future, body un-raced in spawn_blocking.
            let out = match &parsed[i] {
                Err(e) => e.clone(),
                Ok(args) => match registry.get_arc(&calls[i].function.name) {
                    None => format!("error: unknown tool '{}'", calls[i].function.name),
                    Some(tool) => match gate_and_approve(tool.as_ref(), args, cfg) {
                        Some(denied) => denied,
                        None => {
                            let args = args.clone();
                            let quiet = cfg.quiet;
                            let max = cfg.max_tool_result_chars;
                            tokio::task::spawn_blocking(move || run_tool_body(tool, &args, quiet, max))
                                .await
                                .unwrap_or_else(|_| "error: tool thread panicked".to_string())
                        }
                    },
                },
            };
            land(i, out, &mut results, sink);
            i += 1;
        }
    }

    calls
        .iter()
        .zip(results)
        .map(|(tc, r)| (tc.id.clone(), r.unwrap_or_else(|| INTERRUPTED_TOOL_PLACEHOLDER.to_string())))
        .collect()
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
        let Some(t) = registry.get(&tc.function.name) else { return false };
        if !t.is_destructive() || result.starts_with("error:") {
            return false; // unknown tool, or a denied/errored op — changed nothing.
        }
        // `shell_run` returns Ok("exit N\n…") even when the command FAILED (non-zero exit), so a
        // failed destructive shell op would otherwise arm the gate and let pre-existing breakage be
        // blamed on this turn. Only a clean `exit 0` counts as a real edit. (file_edit/file_write/
        // multi_edit return Err → "error:" on failure, already excluded above.)
        if tc.function.name == "shell_run" {
            return result.starts_with("exit 0");
        }
        true
    })
}

/// Build the EAGER-START hook for the streaming path: a tool call whose arguments just finished
/// streaming may start immediately iff it is parallel-safe AND every call before it in this
/// response was too (the PREFIX rule — an eager read must never observably race a write that the
/// barrier partition would have ordered before it) AND under the MAX_PARALLEL cap AND its args
/// parse. Bodies run QUIET — mid-stream tool traces would garble the markdown renderer; the
/// executor emits the standard trace at adoption time instead.
pub fn eager_starter<'a>(
    registry: &'a ToolRegistry,
    cfg: &'a AgentConfig,
) -> impl Fn(usize, &ToolCall) -> Option<tokio::task::JoinHandle<String>> + Send + Sync + 'a {
    let barrier_hit = std::sync::atomic::AtomicBool::new(false);
    let started = std::sync::atomic::AtomicUsize::new(0);
    let max_chars = cfg.max_tool_result_chars;
    move |_slot, tc| {
        use std::sync::atomic::Ordering::Relaxed;
        if barrier_hit.load(Relaxed) {
            return None;
        }
        let ok = parse_call_args(&tc.function.arguments).ok().and_then(|args| {
            let tool = registry.get_arc(&tc.function.name)?;
            (!tool.is_destructive() && tool.is_concurrency_safe_for(&args)).then_some((tool, args))
        });
        let Some((tool, args)) = ok else {
            // First unsafe/unknown/unparseable call = the barrier: nothing after it starts early.
            barrier_hit.store(true, Relaxed);
            return None;
        };
        if started.fetch_add(1, Relaxed) >= MAX_PARALLEL {
            return None; // over the cap: run normally at execution time
        }
        Some(tokio::task::spawn_blocking(move || run_tool_body(tool, &args, true, max_chars)))
    }
}

/// Parse a call's STRINGIFIED arguments; empty → `{}`. Pure — shared by the safety partition and
/// both execution paths (parsed exactly once per call).
fn parse_call_args(raw: &str) -> Result<serde_json::Value, String> {
    if raw.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(raw).map_err(|e| format!("error: invalid JSON arguments: {e}"))
}

/// The safety gate for a BARRIER call: the hard cmd_guard floor, then interactive approval.
/// Returns `Some(feedback)` when refused/declined, `None` when cleared to run. MUST be called on
/// the loop's own future (never inside `spawn_blocking`): the approval prompt runs via
/// `block_in_place`, which keeps the future busy — the REPL's `select!` cannot drop it mid-prompt
/// and orphan the pending approval (byte-identical semantics to the old serial path).
fn gate_and_approve(tool: &dyn tools::Tool, args: &serde_json::Value, cfg: &AgentConfig) -> Option<String> {
    // Shell commands pass the hard safety floor FIRST — before any `/yolo` bypass. A categorically
    // catastrophic command (rm -rf /, mkfs, dd to a raw device, fork bomb, curl|sh) is refused with
    // no override; `smart` mode may auto-clear a read-only command past the approval prompt.
    let mut smart_allow = false;
    // Both `shell_run` and a background `process` start run an arbitrary command → guard them
    // identically, so going background can't sidestep the floor.
    let guarded_command: Option<&str> = match tool.name() {
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
                if crate::ui::tui::active() {
                    crate::ui::tui::emit_line(&line);
                } else if !cfg.quiet {
                    eprintln!("{line}");
                }
                return Some(format!(
                    "error: blocked by the hard safety floor: {reason}. This command is refused \
                     unconditionally (even under /yolo). Choose a narrower, safer command."
                ));
            }
            cmd_guard::Verdict::Allow => smart_allow = cfg.smart_approve,
            cmd_guard::Verdict::Ask => {}
        }
    }

    if tool.is_destructive() && !cfg.auto_approve && !smart_allow && !approve(tool.name(), args) {
        return Some("error: the user declined this action".to_string());
    }
    None
}

/// The body of one tool call: trace → execute → result marker → truncate. Never panics upward
/// (failures become feedback strings). This is the `spawn_blocking` payload — the existing tool
/// bridges (`block_in_place` + `Handle::block_on`) work unchanged on blocking threads (pinned by
/// `tools::tests::bridge_works_inside_spawn_blocking`).
fn run_tool_body(tool: std::sync::Arc<dyn tools::Tool>, args: &serde_json::Value, quiet: bool, max_chars: usize) -> String {
    if !quiet {
        // The Claude-Code-style event anchor: a moonlight dot `⏺`, the tool name, then the salient
        // argument parenthesised (unescaped, 1st line, clipped) — `⏺ file_edit(src/foo.rs)`.
        emit_trace(&tool_call_line(tool.name(), args));
    }
    let out = match tool.execute(args) {
        Ok(out) => out,
        Err(e) => format!("error: {e}"),
    };
    if !quiet {
        emit_tool_result(tool.name(), &out);
    }
    truncate_result(&out, max_chars)
}

/// The event-anchor line for a tool call — `⏺ name(salient-arg)`: a moonlight dot + tool name, the
/// salient argument parenthesised and dimmed. Shared by the serial path, the eager-adoption path,
/// and the approval prompt so every surface renders a call identically.
fn tool_call_line(name: &str, args: &serde_json::Value) -> String {
    format!(
        "{} {}{}",
        crate::ui::theme::accent("⏺"),
        crate::ui::theme::accent(name),
        crate::ui::theme::accent_dim(format!("({})", tool_trace(name, args)))
    )
}

/// Emit a trace line into the scroll region (sticky TUI) or stderr (plain / one-shot path).
fn emit_trace(line: &str) {
    if crate::ui::tui::active() {
        crate::ui::tui::emit_line(line);
    } else {
        eprintln!("{line}");
    }
}

/// After a tool runs, emit an informative result UNDER the call — `  ⎿ <summary>` — where the
/// summary carries real signal (lines read, matches found, `+adds −dels`, exit code, …) instead of
/// a bare check. A failure renders the corner + reason in salmon. For edits, the changed lines then
/// print as a compact colour diff (added = green `+`, removed = salmon `−`). The full tool output
/// still goes to the model; only this digest reaches the terminal, so the transcript stays clean.
fn emit_tool_result(name: &str, out: &str) {
    let (ok, summary) = summarize_result(name, out);
    let corner = if ok { crate::ui::theme::faint("⎿") } else { crate::ui::theme::err("⎿") };
    if ok {
        emit_trace(&format!("  {corner} {}", crate::ui::theme::faint(&summary)));
    } else {
        emit_trace(&format!("  {corner} {}", crate::ui::theme::err(&summary)));
    }
    if !out.trim_start().starts_with("error:") && is_edit_tool(name) {
        emit_edit_diff(out);
    }
}

fn is_edit_tool(name: &str) -> bool {
    matches!(name, "file_edit" | "multi_edit" | "edit_file" | "apply_patch")
}

/// Count added / removed lines in a unified-diff-bearing result: lines beginning `+` / `-` at
/// column 0. The `…(N dòng …)` cap notes begin with `…` and context lines with a space, so neither
/// is miscounted.
fn count_diff(out: &str) -> (usize, usize) {
    let (mut add, mut del) = (0usize, 0usize);
    for l in out.lines() {
        match l.as_bytes().first() {
            Some(b'+') => add += 1,
            Some(b'-') => del += 1,
            _ => {}
        }
    }
    (add, del)
}

/// `edited src/foo.rs (2 replacement(s))` → `sửa src/foo.rs`; `created src/foo.rs` → `tạo src/foo.rs`.
fn edit_target(head: &str) -> String {
    let mut it = head.split_whitespace();
    match (it.next(), it.next()) {
        (Some("created"), Some(path)) => format!("tạo {path}"),
        (Some(_), Some(path)) => format!("sửa {path}"),
        _ => "sửa".to_string(),
    }
}

/// Render up to a few changed lines of an edit result as a colour diff, indented under the `⎿`
/// (added = green `+`, removed = salmon `−`). Reads the `^[-+]`-prefixed lines the unified
/// `diff_preview` emitted; context / cap-note lines (space- / `…`-prefixed) are skipped.
fn emit_edit_diff(out: &str) {
    const MAX_SHOWN: usize = 8;
    let budget = crate::ui::tui::width().saturating_sub(8).max(16);
    let mut shown = 0usize;
    for l in out.lines() {
        let (is_add, content) = match l.as_bytes().first() {
            Some(b'+') => (true, &l[1..]),
            Some(b'-') => (false, &l[1..]),
            _ => continue,
        };
        if shown == MAX_SHOWN {
            emit_trace(&format!("    {}", crate::ui::theme::faint("… (diff rút gọn)")));
            break;
        }
        let clipped: String = content.chars().take(budget).collect();
        let styled = if is_add {
            crate::ui::theme::ok(format!("+ {clipped}")).to_string()
        } else {
            crate::ui::theme::err(format!("− {clipped}")).to_string()
        };
        emit_trace(&format!("    {styled}"));
        shown += 1;
    }
}

/// Build the `⎿` summary for a tool result, returning `(ok, text)` (`ok=false` → coloured as a
/// failure). Parses the tool's OWN returned string — no `Tool` trait change — minting a concise
/// Vietnamese label for the high-traffic tools and reusing the tool's own one-line header where it
/// already reads well (LSP `N reference(s)`, `search_files` `N match(es) in M file(s)`, `web_crawl`,
/// `todo_write`).
fn summarize_result(name: &str, out: &str) -> (bool, String) {
    let trimmed = out.trim_start();
    if let Some(reason) = trimmed.strip_prefix("error:") {
        return (false, format!("lỗi: {}", first_line_clip(reason.trim(), 60)));
    }
    let first = out.lines().next().unwrap_or("");
    match name {
        "shell_run" | "bash" | "powershell" | "shell" => {
            let code = trimmed.strip_prefix("exit ").and_then(|rest| {
                let tok: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
                tok.parse::<i32>().ok()
            });
            match code {
                Some(0) => (true, "exit 0".to_string()),
                Some(n) => (false, format!("exit {n}")),
                None => (true, "xong".to_string()),
            }
        }
        "file_read" | "read_file" => (true, format!("đọc {} dòng", out.lines().count())),
        "file_glob" => {
            if trimmed.starts_with("(no files") {
                (true, "0 tệp".to_string())
            } else {
                (true, format!("{} tệp", out.lines().count()))
            }
        }
        "file_edit" | "edit_file" | "apply_patch" => {
            if first.starts_with("created") {
                (true, edit_target(first))
            } else {
                let (a, d) = count_diff(out);
                (true, format!("{} · +{a} −{d}", edit_target(first)))
            }
        }
        "multi_edit" => {
            let (a, d) = count_diff(out);
            (true, format!("{} · +{a} −{d}", edit_target(first)))
        }
        "file_write" | "write_file" => (true, "đã ghi".to_string()),
        "memory_search" => {
            if trimmed.starts_with("(no memory") {
                (true, "0 ghi nhớ".to_string())
            } else {
                (true, format!("{} ghi nhớ", out.lines().filter(|l| l.starts_with('[')).count()))
            }
        }
        "web_search" => {
            if trimmed.starts_with("(no results") {
                (true, "0 kết quả".to_string())
            } else {
                let n = out.lines().filter(|l| l.trim_start().starts_with(|c: char| c.is_ascii_digit())).count();
                (true, format!("{n} kết quả"))
            }
        }
        "web_fetch" => {
            let kb = out.len() as f64 / 1024.0;
            if kb >= 1.0 {
                (true, format!("tải {kb:.0} KB"))
            } else {
                (true, format!("{} ký tự", out.chars().count()))
            }
        }
        // Everything else already returns a good one-line header (`N reference(s)`,
        // `N match(es) in M file(s):`, `crawled N page(s) → M URL(s)`, `todo list updated: …`) —
        // reuse it verbatim (sans a trailing ':').
        _ => {
            let f = out.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            if f.is_empty() {
                (true, "xong".to_string())
            } else {
                (true, first_line_clip(f.trim_end_matches(':'), 60))
            }
        }
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

/// Flat per-message envelope estimate (role name + JSON framing), in tokens.
const MSG_OVERHEAD_TOK: usize = 4;
/// Flat estimate per attached vision image. The real cost is provider-side (encoder-dependent);
/// a modest constant beats counting zero for a multi-MB attachment.
const IMAGE_TOK: usize = 768;

/// Rough token estimate (chars/4, no tokenizer dep) over EVERYTHING a message actually puts on
/// the wire: content, tool-call payloads (name + arguments + ~24 chars of call envelope), plus
/// flat per-message / per-image overheads. The old content-only estimate systematically
/// under-counted tool-heavy turns (a `file_edit` turn carries the whole diff in `arguments` with
/// `content: null`), so the 60/80/90% context guards fired late. Shared with `main.rs`'s
/// `session_tokens` so the mid-loop guard and the HUD agree on size.
pub fn estimate_message_tokens(m: &Message) -> usize {
    let mut chars: usize = m.content.as_ref().map_or(0, |c| c.chars().count());
    for tc in &m.tool_calls {
        chars += tc.function.name.chars().count() + tc.function.arguments.chars().count() + 24;
    }
    chars / 4 + MSG_OVERHEAD_TOK + m.images.len() * IMAGE_TOK
}

/// Sum of [`estimate_message_tokens`]. Callers comparing against the context window must ADD the
/// per-request tool-schema overhead (`estimate_defs_tokens` / `schema_overhead_tokens`) — the
/// schemas ride on every request but live in no message.
fn estimate_tokens(messages: &[Message]) -> usize {
    messages.iter().map(estimate_message_tokens).sum()
}

/// The most recent tool-schema overhead estimate, published by the loop for `main.rs`'s HUD /
/// auto-compact so both sides agree on request size (0 before the first loop run).
static SCHEMA_OVERHEAD_TOK: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Per-request tool-schema cost: the serialized JSON length / 4. Computed once per loop run (the
/// defs don't change mid-run) and published to a process-global for the HUD.
pub fn estimate_defs_tokens(defs: &[ToolDef]) -> usize {
    let tok = serde_json::to_string(defs).map(|s| s.len() / 4).unwrap_or(0);
    SCHEMA_OVERHEAD_TOK.store(tok, std::sync::atomic::Ordering::Relaxed);
    tok
}

/// Read back the last published tool-schema overhead (see [`estimate_defs_tokens`]).
pub fn schema_overhead_tokens() -> usize {
    SCHEMA_OVERHEAD_TOK.load(std::sync::atomic::Ordering::Relaxed)
}

/// Anchor from the provider's REAL usage report: `tokens` = billed prompt tokens at the last
/// usage-reporting call, `est_at` = our estimate of that same request. Guards then track growth
/// with the estimate DELTA on top of the real base — far more accurate than chars/4 alone for
/// code-heavy or non-English content.
struct RealAnchor {
    tokens: usize,
    est_at: usize,
}

/// Effective context size for the guards: the real anchor plus estimated growth since it, or the
/// plain estimate when the provider never reported usage.
fn effective_tokens(est_now: usize, anchor: Option<&RealAnchor>) -> usize {
    match anchor {
        Some(a) => a.tokens + est_now.saturating_sub(a.est_at),
        None => est_now,
    }
}

/// Sanity clamp before trusting a provider-reported prompt size: some gateways report cumulative
/// or tool-exclusive numbers; anything outside `[est/4, est*4]` is discarded as garbage.
fn accept_anchor(real: usize, est: usize) -> bool {
    real >= est / 4 && real <= est.saturating_mul(4)
}

/// Compact token count for trace lines: `12.4K` / `300`.
fn fmt_tok(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}K", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// The placeholder a cleared tool result is collapsed to — tells the model it can re-fetch.
const CLEARED_TOOL_PLACEHOLDER: &str =
    "[earlier tool output cleared to conserve context — re-run the tool if you need it again]";

/// Suffix marking a trimmed FAILURE result. Doubles as the idempotency sentinel — a body ending
/// with it is never re-trimmed.
const FAILED_TOOL_TRIM_SUFFIX: &str =
    " …[failure detail trimmed to conserve context — the error still stands; re-run for the full output]";

/// What one clearing pass did (drives the trace line; fields also assert cleanly in tests).
#[derive(Debug, Default, PartialEq)]
pub struct ClearStats {
    pub chars_reclaimed: usize,
    /// Successes blanked to [`CLEARED_TOOL_PLACEHOLDER`].
    pub cleared: usize,
    /// Failures reduced to first line + [`FAILED_TOOL_TRIM_SUFFIX`].
    pub failures_trimmed: usize,
}

/// A tool result that represents a FAILURE the model must keep seeing: an `error:`-prefixed
/// feedback string (unknown tool / bad args / tool error / denied approval) or a `shell_run`
/// result whose exit code is nonzero. Blanking failures teaches the model to repeat them —
/// its own error trail is how it stops making the same mistake (the Manus lesson).
fn is_failure_result(content: &str) -> bool {
    if content.starts_with("error:") {
        return true;
    }
    // shell_run convention: results start "exit N\n…" (builtin.rs) — nonzero N is a failure.
    if let Some(rest) = content.strip_prefix("exit ") {
        let code: String = rest.chars().take_while(|c| c.is_ascii_digit() || *c == '-').collect();
        return code.parse::<i64>().map(|n| n != 0).unwrap_or(false);
    }
    false
}

/// Batch-evict stale tool-result bodies until the running estimate (messages + `schema_overhead`)
/// reaches `target_tokens` — oldest-first and ERROR-AWARE. One BIG infrequent mutation instead of
/// a per-turn trickle, because every mid-history rewrite invalidates the provider's prompt cache
/// from that byte onward.
///
/// Pass 1 blanks bulky SUCCESSES (> `min_chars`) to [`CLEARED_TOOL_PLACEHOLDER`]. Pass 2 — only
/// when successes alone can't reach the floor — TRIMS failures to their first line +
/// [`FAILED_TOOL_TRIM_SUFFIX`], so the failure signal survives while the bulk goes. The most
/// recent `keep_recent` tool results are never touched; `tool_call_id` always survives (no orphan
/// pairing → no gateway 400); both passes are idempotent (cleared bodies are short; trimmed
/// failures carry the sentinel suffix).
pub fn clear_tool_results_to_floor(
    messages: &mut [Message],
    keep_recent: usize,
    min_chars: usize,
    target_tokens: usize,
    schema_overhead: usize,
) -> ClearStats {
    let mut stats = ClearStats::default();
    let tool_idxs: Vec<usize> =
        messages.iter().enumerate().filter(|(_, m)| m.role == "tool").map(|(i, _)| i).collect();
    if tool_idxs.len() <= keep_recent {
        return stats; // nothing older than the recent window
    }
    let clear_upto = tool_idxs.len() - keep_recent; // only the oldest `clear_upto` are candidates
    let mut est = estimate_tokens(messages) + schema_overhead;

    // Pass 1: bulky successes, oldest first, until the floor is reached.
    for &i in &tool_idxs[..clear_upto] {
        if est <= target_tokens {
            break;
        }
        let len = match messages[i].content.as_deref() {
            Some(b) if b.chars().count() > min_chars && !is_failure_result(b) => b.chars().count(),
            _ => continue,
        };
        est = est.saturating_sub(estimate_message_tokens(&messages[i]));
        messages[i].content = Some(CLEARED_TOOL_PLACEHOLDER.to_string());
        est += estimate_message_tokens(&messages[i]);
        stats.chars_reclaimed += len.saturating_sub(CLEARED_TOOL_PLACEHOLDER.chars().count());
        stats.cleared += 1;
    }

    // Pass 2: failures — trimmed, never blanked, and only when still above the floor.
    for &i in &tool_idxs[..clear_upto] {
        if est <= target_tokens {
            break;
        }
        let (len, trimmed) = match messages[i].content.as_deref() {
            Some(b)
                if is_failure_result(b)
                    && b.chars().count() > min_chars
                    && !b.ends_with(FAILED_TOOL_TRIM_SUFFIX) =>
            {
                let first: String = b.lines().next().unwrap_or("").chars().take(160).collect();
                (b.chars().count(), format!("{first}{FAILED_TOOL_TRIM_SUFFIX}"))
            }
            _ => continue,
        };
        est = est.saturating_sub(estimate_message_tokens(&messages[i]));
        messages[i].content = Some(trimmed);
        est += estimate_message_tokens(&messages[i]);
        stats.chars_reclaimed += len.saturating_sub(messages[i].content.as_deref().map_or(0, |c| c.chars().count()));
        stats.failures_trimmed += 1;
    }
    stats
}

/// Is a clearing pass due? First crossing always fires; after that, only `step_pct` points of
/// growth past the last fire OR `cooldown_iters` iterations re-arm it — the cadence that keeps
/// mutations infrequent (cache-friendly) instead of a per-turn trickle.
fn clearing_due(pct: usize, iter: usize, last: Option<(usize, usize)>, step_pct: u8, cooldown_iters: usize) -> bool {
    match last {
        None => true,
        Some((p0, i0)) => pct >= p0 + step_pct as usize || iter >= i0 + cooldown_iters,
    }
}

// ── self-review (opt-in, one extra turn before Done) ────────────────────────────────────────────

/// Nudge-mode self-review text (no oracle configured): the model re-reads its own diff.
const SELF_REVIEW_NUDGE: &str =
    "[self-review] Before finishing: run `git diff`, re-read the ORIGINAL request, and verify \
     every requirement is met and nothing unrelated changed. Fix or flag anything off, then give \
     your final answer.";

/// Ask the oracle (a usually-stronger model) to review the working-tree diff against the original
/// request. `None` ⇒ nothing actionable (no git / empty diff / LGTM / call failed) — the loop
/// falls through to Done without burning a turn.
async fn oracle_review<O, OFut>(oracle: &O, messages: &[Message]) -> Option<String>
where
    O: Fn(Vec<Message>) -> OFut,
    OFut: Future<Output = Result<String>>,
{
    let diff = git_diff_capped()?;
    let task: String = messages
        .iter()
        .find(|m| m.role == "user")
        .and_then(|m| m.content.as_deref())
        .unwrap_or("")
        .chars()
        .take(2_000)
        .collect();
    let sys = Message::system(
        "You are a rigorous senior code reviewer. Review the DIFF against the ORIGINAL REQUEST. \
         List only REAL problems (bugs, missed requirements, unintended changes) with file:line \
         evidence, most severe first. If the diff is sound, reply with exactly: LGTM",
    );
    let usr = Message::user(format!("ORIGINAL REQUEST:\n{task}\n\nDIFF:\n{diff}"));
    match oracle(vec![sys, usr]).await {
        Ok(s) => {
            let t = s.trim().to_string();
            (!t.is_empty() && !t.eq_ignore_ascii_case("lgtm")).then_some(t)
        }
        Err(_) => None, // best-effort: a failing oracle never blocks Done
    }
}

/// The working-tree `git diff`, capped at 12k chars. `None` when not a repo / git missing / clean.
fn git_diff_capped() -> Option<String> {
    let out = std::process::Command::new("git").args(["diff"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout);
    let t = s.trim();
    if t.is_empty() {
        return None;
    }
    Some(t.chars().take(12_000).collect())
}

// ── mid-loop nudges (collapsed, never accreted) ─────────────────────────────────────────────────

/// Stable identifying prefixes for the loop's system nudges. [`push_nudge`] uses them to REPLACE a
/// stale earlier nudge of the same kind instead of accreting a new copy — across a long REPL
/// session (one loop invocation per user turn) the old append-only behavior grew without bound.
const NUDGE_CONTEXT: &str = "Context is nearly full";
const NUDGE_DIVERGENCE: &str = "You repeated the same tool call(s)";
const NUDGE_STEP_LIMIT: &str = "You are nearing the step limit";
const NUDGE_TODO: &str = "Current task list";

/// Append a system nudge, first removing any EARLIER system message of the same kind
/// (`kind_prefix` must prefix `text`). Scans indices 1.. only — the system prompt at `[0]` is
/// untouchable — and removes ONLY `role == "system"` messages, so assistant↔tool pairing cannot be
/// orphaned by construction. The new nudge is always the TAIL message, preserving the caller's
/// error-rollback contract (`messages.pop()` removes exactly the nudge).
fn push_nudge(messages: &mut Vec<Message>, kind_prefix: &str, text: &str) {
    debug_assert!(text.starts_with(kind_prefix), "kind prefix must identify its own nudge text");
    let mut i = messages.len();
    while i > 1 {
        i -= 1;
        if messages[i].role == "system"
            && messages[i].content.as_deref().is_some_and(|c| c.starts_with(kind_prefix))
        {
            messages.remove(i);
        }
    }
    messages.push(Message::system(text));
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
    if crate::ui::tui::active() {
        let prompt = format!(
            "{}  {}",
            tool_call_line(tool, args),
            style("— approve? [y]es · [n]o · [a]llow all this session").color256(crate::ui::theme::WARN)
        );
        return tokio::task::block_in_place(|| crate::ui::tui::ask_approval(&prompt));
    }
    if !std::io::stdin().is_terminal() {
        if crate::channels::telegram::daemon_is_active() && crate::channels::telegram::is_configured() {
            let prompt = format!("{tool} {}", compact_args(args));
            // Bridge to the async approval on the current (multi-thread) runtime; the serve poll
            // loop runs on another worker and delivers the callback.
            if let Some(v) = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(crate::channels::telegram::request_approval(&prompt))
            }) {
                return v;
            }
        }
        return false;
    }
    print!(
        "{}  {} ",
        tool_call_line(tool, args),
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
    use crate::core::types::FunctionCall;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    // ── display: the ⏺ call line + ⎿ result summary ─────────────────────────
    #[test]
    fn tool_call_line_uses_the_dot_and_parens() {
        let line = tool_call_line("file_read", &serde_json::json!({"path": "src/main.rs"}));
        let plain = console::strip_ansi_codes(&line).to_string();
        assert!(plain.starts_with("⏺ file_read("), "{plain:?}");
        assert!(plain.contains("src/main.rs)"), "salient arg parenthesised: {plain:?}");
    }

    #[test]
    fn summarize_result_reads_signal_from_each_tool() {
        assert_eq!(summarize_result("file_read", "l1\nl2\nl3"), (true, "đọc 3 dòng".to_string()));
        assert_eq!(summarize_result("shell_run", "exit 0\nok"), (true, "exit 0".to_string()));
        assert_eq!(summarize_result("shell_run", "exit 2\nboom"), (false, "exit 2".to_string()));
        assert_eq!(summarize_result("file_glob", "a.rs\nb.rs"), (true, "2 tệp".to_string()));
        assert_eq!(summarize_result("file_glob", "(no files match 'x')"), (true, "0 tệp".to_string()));
        // an edit result → target + counts derived from the embedded unified diff
        let edit = "edited src/x.rs (1 replacement(s))\n a\n-old\n+new\n b";
        let (ok, s) = summarize_result("file_edit", edit);
        assert!(ok && s.starts_with("sửa src/x.rs") && s.contains("+1"), "{s:?}");
        assert_eq!(summarize_result("file_edit", "created src/n.rs"), (true, "tạo src/n.rs".to_string()));
        // a tool with no special arm reuses its own header (sans a trailing ':')
        assert_eq!(
            summarize_result("search_files", "7 match(es) in 2 file(s):\nsrc/a.rs:3: hit"),
            (true, "7 match(es) in 2 file(s)".to_string())
        );
        // an error is coloured as a failure
        let (ok, s) = summarize_result("file_edit", "error: old_string not found");
        assert!(!ok && s.starts_with("lỗi:"), "{s:?}");
    }

    #[test]
    fn count_diff_counts_only_column0_markers() {
        let out = "edited x (1)\n a\n-gone\n+added\n+also\n…(3 dòng thêm nữa)\n b";
        assert_eq!(count_diff(out), (2, 1), "two '+' lines, one '-'; '…' and ' ' ignored");
    }

    #[test]
    fn edit_target_labels_create_vs_edit() {
        assert_eq!(edit_target("edited src/x.rs (1 replacement(s))"), "sửa src/x.rs");
        assert_eq!(edit_target("created src/n.rs"), "tạo src/n.rs");
    }

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
            keep_recent_tool_results: 8,
            clear_tool_result_min_chars: 1024,
            clear_at_pct: 60,
            clear_target_pct: 45,
            clear_step_pct: 10,
            clear_cooldown_iters: 6,
            todo_reminder_every: 0, // recitation OFF in unit tests (todo state is process-global)
            compact_at_pct: 80,
            max_verify_attempts: 2,
            enable_self_review: false,
            enable_lsp: false,
            lsp_request_timeout_secs: 20,
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

    /// The barrier path as one call (gate → body), mirroring `execute_calls`' serial arm — lets
    /// the gate tests stay synchronous and focused.
    fn execute_one_for_test(r: &ToolRegistry, tc: &ToolCall, cfg: &AgentConfig) -> String {
        let args = match parse_call_args(&tc.function.arguments) {
            Ok(a) => a,
            Err(e) => return e,
        };
        let Some(tool) = r.get_arc(&tc.function.name) else {
            return format!("error: unknown tool '{}'", tc.function.name);
        };
        if let Some(denied) = gate_and_approve(tool.as_ref(), &args, cfg) {
            return denied;
        }
        run_tool_body(tool, &args, cfg.quiet, cfg.max_tool_result_chars)
    }

    /// Drive the async executor the way the loop does: pre-filled placeholder sink, results out.
    async fn exec(r: &ToolRegistry, calls: &[ToolCall], c: &AgentConfig) -> Vec<(String, String)> {
        let mut sink: Vec<Message> = calls
            .iter()
            .map(|tc| Message::tool_result(tc.id.clone(), INTERRUPTED_TOOL_PLACEHOLDER.to_string()))
            .collect();
        let results = execute_calls(r, calls, c, &mut sink, Vec::new()).await;
        // The sink must mirror the returned results (the loop relies on it).
        for (k, (_, out)) in results.iter().enumerate() {
            assert_eq!(sink[k].content.as_deref(), Some(out.as_str()), "sink[{k}] mirrors the result");
        }
        results
    }

    #[test]
    fn hard_floor_blocks_even_under_yolo() {
        // THE security invariant: a catastrophic command is refused even with auto_approve (yolo) ON.
        // The floor runs BEFORE the approval short-circuit, so /yolo cannot bypass it.
        let mut r = ToolRegistry::new();
        r.register(Box::new(ShellStub));
        let mut c = cfg();
        c.auto_approve = true; // yolo
        let out = execute_one_for_test(&r, &call("1", "shell_run", r#"{"command":"rm -rf /"}"#), &c);
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
        let out = execute_one_for_test(&r, &call("1", "shell_run", r#"{"command":"ls -la"}"#), &c);
        assert_eq!(out, "RAN", "read-only shell should auto-run under smart");
    }

    #[test]
    fn smart_still_asks_for_writes() {
        // A write-shaped command under smart (non-TTY) → safe-deny, NOT auto-run.
        let mut r = ToolRegistry::new();
        r.register(Box::new(ShellStub));
        let mut c = cfg();
        c.smart_approve = true; // not yolo
        let out = execute_one_for_test(&r, &call("1", "shell_run", r#"{"command":"rm -rf node_modules"}"#), &c);
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
    fn estimator_counts_tool_call_args_and_overheads() {
        // A file_edit-style turn: content null, the whole payload rides in `arguments` — the old
        // content-only estimate scored this ZERO.
        let big_args = "x".repeat(4000);
        let mut m = Message::system("");
        m.role = "assistant".into();
        m.content = None;
        m.tool_calls = vec![call("a", "file_edit", &big_args)];
        let tok = estimate_message_tokens(&m);
        assert!(tok >= 1000, "4000-char arguments must dominate the estimate, got {tok}");
        // Content-only messages count content/4 plus the flat envelope.
        let plain = Message::user("abcd".repeat(100)); // 400 chars → 100 tok
        assert_eq!(estimate_message_tokens(&plain), 100 + MSG_OVERHEAD_TOK);
    }

    #[test]
    fn defs_overhead_is_deterministic_and_published() {
        let r = registry();
        let tok = estimate_defs_tokens(&r.defs());
        assert!(tok > 0, "two registered tools must have a nonzero schema cost");
        assert_eq!(estimate_defs_tokens(&r.defs()), tok, "same defs → same estimate");
        assert!(schema_overhead_tokens() > 0, "the loop-published global must be readable");
    }

    #[test]
    fn effective_tokens_prefers_anchor_and_tracks_growth() {
        assert_eq!(effective_tokens(500, None), 500, "no anchor → plain estimate");
        let a = RealAnchor { tokens: 900, est_at: 300 };
        assert_eq!(effective_tokens(300, Some(&a)), 900, "at the anchor point → the real number");
        assert_eq!(effective_tokens(350, Some(&a)), 950, "growth rides on the real base");
        assert_eq!(effective_tokens(250, Some(&a)), 900, "never below the real base (saturating)");
    }

    #[test]
    fn prompt_tier_heuristic_and_override() {
        // Small/local families and size suffixes → strict.
        assert_eq!(prompt_tier_for("qwen2.5-coder-7b", None), PromptTier::Strict);
        assert_eq!(prompt_tier_for("Llama-3.3-70B-Instruct", None), PromptTier::Strict, "llama family is strict");
        assert_eq!(prompt_tier_for("gpt-4o-mini", None), PromptTier::Strict, "mini tier is strict");
        assert_eq!(prompt_tier_for("mistral-small-latest", None), PromptTier::Strict);
        assert_eq!(prompt_tier_for("some-model-14b", None), PromptTier::Strict, "size suffix");
        // Frontier / unknown → full (the safe default).
        assert_eq!(prompt_tier_for("claude-sonnet-4-6", None), PromptTier::Full);
        assert_eq!(prompt_tier_for("gpt-4o", None), PromptTier::Full);
        assert_eq!(prompt_tier_for("totally-unknown-model", None), PromptTier::Full);
        // Whole-token matching: substrings never false-positive.
        assert_eq!(prompt_tier_for("geminiacs-pro", None), PromptTier::Full);
        assert_eq!(prompt_tier_for("nanotech-writer-xl", None), PromptTier::Full);
        // Config override beats the heuristic, both ways.
        assert_eq!(prompt_tier_for("gpt-4o", Some("strict")), PromptTier::Strict);
        assert_eq!(prompt_tier_for("qwen2.5-coder-7b", Some("full")), PromptTier::Full);
    }

    #[test]
    fn system_prompt_is_byte_stable_per_tier() {
        // build_system_prompt reads global HOME state (skills/persona/config) — serialize with the
        // other sandboxing tests or a concurrent skill-write makes the two builds differ.
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Same inputs → identical bytes (the prefix-cache invariant), on both tiers.
        let a = build_system_prompt("/w", "linux", "2026-07-05", "gpt-4o", None);
        let b = build_system_prompt("/w", "linux", "2026-07-05", "gpt-4o", None);
        assert_eq!(a, b, "full tier must be deterministic");
        let s1 = build_system_prompt("/w", "linux", "2026-07-05", "qwen2.5-coder-7b", None);
        let s2 = build_system_prompt("/w", "linux", "2026-07-05", "qwen2.5-coder-7b", None);
        assert_eq!(s1, s2, "strict tier must be deterministic");
        assert!(s1.starts_with(SYSTEM_BASE_STRICT.trim_end()), "strict base leads the strict prompt");
        assert!(s1.contains("OUTPUT CONTRACT"));
    }

    #[test]
    fn fmt_tok_compact() {
        assert_eq!(fmt_tok(999), "999");
        assert_eq!(fmt_tok(24_130), "24.1K");
    }

    #[test]
    fn anchor_clamp_rejects_garbage() {
        assert!(accept_anchor(100, 100));
        assert!(accept_anchor(25, 100), "est/4 boundary is accepted");
        assert!(accept_anchor(400, 100), "est*4 boundary is accepted");
        assert!(!accept_anchor(24, 100), "below est/4 → cumulative-gateway garbage");
        assert!(!accept_anchor(401, 100), "above est*4 → garbage");
    }

    #[tokio::test]
    async fn real_usage_anchor_triggers_wrapup_before_estimate_would() {
        let r = registry();
        let c = AgentConfig { max_iters: 5, auto_extend_to: 5, quiet: true, context_window: 1000, ..Default::default() };
        // ~1200 chars ≈ 300 tok estimated — far under 90% of the 1000-tok window on its own.
        let mut messages = vec![Message::system("sys"), Message::user("x".repeat(1200))];
        // …but the provider reports the request REALLY was 950 prompt tokens (code-heavy tokenization).
        let mut anchored = tool_turn("echo", r#"{"text":"a"}"#);
        anchored.usage =
            Some(crate::core::types::Usage { prompt_tokens: Some(950), ..Default::default() });
        let chat = scripted(vec![anchored, final_turn("done")]);
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert!(
            messages.iter().any(|m| m.content.as_deref().is_some_and(|c| c.contains("Context is nearly full"))),
            "the real-usage anchor (950/1000) must trigger the wrap-up nudge even though chars/4 (~300) would not"
        );
    }

    #[tokio::test]
    async fn compacting_loop_summarizes_older_turns_when_over_threshold() {
        let r = registry();
        // window 100 tok, compact at 80% — the bulky multi-turn history below blows past it.
        let c = AgentConfig { context_window: 100, compact_at_pct: 80, ..cfg() };
        let mut messages = vec![
            Message::system("sys"),
            Message::user(format!("u1 {}", "x".repeat(200))),
            Message::assistant("a1"),
            Message::user(format!("u2 {}", "y".repeat(200))),
            Message::assistant("a2"),
            Message::user(format!("u3 {}", "z".repeat(200))),
        ];
        let summarize = |_msgs: Vec<Message>| async { Ok("DENSE_SUMMARY_OK".to_string()) };
        let out = run_agent_loop_compacting(scripted(vec![final_turn("done")]), summarize, &c, &r, &mut messages)
            .await
            .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(messages[0].content.as_deref(), Some("sys"), "system prompt preserved at [0]");
        assert!(
            messages.iter().any(|m| m.content.as_deref().is_some_and(|x| x.contains("DENSE_SUMMARY_OK"))),
            "older turns were summarized into the injected compaction note"
        );
        // The bulky first turn was folded into the summary (no longer present verbatim).
        assert!(
            !messages.iter().any(|m| m.content.as_deref().is_some_and(|x| x.contains(&"x".repeat(200)))),
            "the oldest bulky turn was compacted away"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_calls_parallel_preserves_order() {
        // 3 read-only echo calls in one turn → concurrent run; results re-stitch in CALL order
        // regardless of completion order.
        let r = registry();
        let calls = vec![
            call("1", "echo", r#"{"text":"first"}"#),
            call("2", "echo", r#"{"text":"second"}"#),
            call("3", "echo", r#"{"text":"third"}"#),
        ];
        let results = exec(&r, &calls, &cfg()).await;
        assert_eq!(results, vec![
            ("1".to_string(), "first".to_string()),
            ("2".to_string(), "second".to_string()),
            ("3".to_string(), "third".to_string()),
        ]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_calls_parallel_is_fail_soft() {
        // echo + fail are both read-only/safe → concurrent; one tool's error must not drop its
        // sibling's result (fail-soft, no sibling abort).
        let r = registry();
        let calls = vec![call("1", "echo", r#"{"text":"ok"}"#), call("2", "fail", "{}")];
        let results = exec(&r, &calls, &cfg()).await;
        assert_eq!(results[0], ("1".to_string(), "ok".to_string()));
        assert_eq!(results[1].0, "2");
        assert!(results[1].1.contains("boom"), "tool error fed back, got {:?}", results[1].1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_calls_destructive_is_a_barrier_siblings_still_run() {
        // delete is destructive → a BARRIER (approval gating preserved), but the safe sibling
        // BEFORE it still executes concurrently-eligible; order kept.
        // non-TTY test env → delete safe-denied.
        let r = registry();
        let calls = vec![call("1", "echo", r#"{"text":"x"}"#), call("2", "delete", "{}")];
        let results = exec(&r, &calls, &cfg()).await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], ("1".to_string(), "x".to_string()));
        assert!(results[1].1.contains("declined"), "destructive denied non-TTY, got {:?}", results[1].1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_calls_single_call_works() {
        // a lone safe call runs (one-handle run) and yields the same result.
        let r = registry();
        let results = exec(&r, &[call("1", "echo", r#"{"text":"solo"}"#)], &cfg()).await;
        assert_eq!(results, vec![("1".to_string(), "solo".to_string())]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_calls_parallel_above_cap_keeps_order() {
        // > MAX_PARALLEL calls → windowed; order still preserved.
        let r = registry();
        let calls: Vec<ToolCall> = (0..(MAX_PARALLEL * 2 + 1))
            .map(|i| call(&format!("id{i}"), "echo", &format!(r#"{{"text":"v{i}"}}"#)))
            .collect();
        let results = exec(&r, &calls, &cfg()).await;
        assert_eq!(results.len(), calls.len());
        for (i, (id, val)) in results.iter().enumerate() {
            assert_eq!(id, &format!("id{i}"));
            assert_eq!(val, &format!("v{i}"));
        }
    }

    #[tokio::test]
    async fn self_review_fires_once_after_edits_then_allows_done() {
        // enable_self_review + an edit → the first "done" is intercepted by ONE review turn
        // (nudge mode — no oracle), the second "done" is accepted.
        let r = registry();
        let c = AgentConfig { enable_self_review: true, auto_approve: true, ..cfg() };
        let mut messages = vec![Message::system("sys"), Message::user("edit something")];
        let chat = scripted(vec![
            tool_turn("delete", "{}"), // a successful destructive op arms made_any_edits
            final_turn("first done"),  // intercepted by the self-review nudge
            final_turn("second done"), // accepted
        ]);
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(out.final_text.as_deref(), Some("second done"));
        let reviews = messages
            .iter()
            .filter(|m| m.role == "user" && m.content.as_deref().is_some_and(|c| c.starts_with("[self-review]")))
            .count();
        assert_eq!(reviews, 1, "exactly one review turn, never a loop");
        assert_valid_history(&messages);
    }

    #[tokio::test]
    async fn self_review_skipped_without_edits() {
        let r = registry();
        let c = AgentConfig { enable_self_review: true, ..cfg() };
        let mut messages = vec![Message::system("sys"), Message::user("just a question")];
        let chat = scripted(vec![tool_turn("echo", r#"{"text":"look"}"#), final_turn("answer")]);
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(out.final_text.as_deref(), Some("answer"));
        assert!(
            !messages.iter().any(|m| m.content.as_deref().is_some_and(|c| c.starts_with("[self-review]"))),
            "read-only runs never pay the review turn"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_calls_adopts_eager_handles_by_position() {
        let r = registry();
        let calls = vec![call("1", "echo", r#"{"text":"fresh"}"#), call("2", "echo", r#"{"text":"normal"}"#)];
        // Position 0 was eagerly started (with a distinguishable payload) — adoption must use it,
        // never re-run the tool.
        let h = tokio::task::spawn_blocking(|| "EAGER_RESULT".to_string());
        let mut sink: Vec<Message> = calls
            .iter()
            .map(|tc| Message::tool_result(tc.id.clone(), INTERRUPTED_TOOL_PLACEHOLDER.to_string()))
            .collect();
        let results = execute_calls(&r, &calls, &cfg(), &mut sink, vec![(0, h)]).await;
        assert_eq!(results[0].1, "EAGER_RESULT", "adopted, not re-executed");
        assert_eq!(results[1].1, "normal", "non-eager sibling runs normally");
        assert_eq!(sink[0].content.as_deref(), Some("EAGER_RESULT"), "sink mirrors the adopted result");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn eager_starter_enforces_the_prefix_rule() {
        let r = registry();
        let c = cfg();
        let starter = eager_starter(&r, &c);
        // Safe call → started early.
        let h = starter(0, &call("1", "echo", r#"{"text":"a"}"#));
        assert!(h.is_some(), "read-only call starts eagerly");
        assert_eq!(h.unwrap().await.unwrap(), "a", "the eager body really ran");
        // Destructive call → None AND trips the barrier…
        assert!(starter(1, &call("2", "delete", "{}")).is_none(), "writes never start early");
        // …so a safe call AFTER it must not start either (prefix rule = barrier semantics).
        assert!(starter(2, &call("3", "echo", r#"{"text":"b"}"#)).is_none(), "post-barrier calls wait");
    }

    /// Records event order across threads for the barrier-semantics + drop tests.
    struct RecordingTool {
        name: &'static str,
        destructive: bool,
        log: std::sync::Arc<Mutex<Vec<String>>>,
        delay_ms: u64,
    }
    impl Tool for RecordingTool {
        fn name(&self) -> &str {
            self.name
        }
        fn description(&self) -> &str {
            "recorder"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object"})
        }
        fn is_destructive(&self) -> bool {
            self.destructive
        }
        fn is_concurrency_safe(&self) -> bool {
            !self.destructive
        }
        fn execute(&self, _args: &serde_json::Value) -> Result<String> {
            self.log.lock().unwrap().push(format!("{}:start", self.name));
            std::thread::sleep(std::time::Duration::from_millis(self.delay_ms));
            self.log.lock().unwrap().push(format!("{}:end", self.name));
            Ok(format!("{} done", self.name))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn barrier_partition_preserves_sequential_observability() {
        // [read read WRITE read] ⇒ both reads END before the write STARTS; the write ENDS before
        // the trailing read STARTS. Sequential observability — a read after a write always sees
        // post-write state.
        let log = std::sync::Arc::new(Mutex::new(Vec::new()));
        let mut r = ToolRegistry::new();
        r.register(Box::new(RecordingTool { name: "read_a", destructive: false, log: log.clone(), delay_ms: 20 }));
        r.register(Box::new(RecordingTool { name: "read_b", destructive: false, log: log.clone(), delay_ms: 5 }));
        r.register(Box::new(RecordingTool { name: "write_w", destructive: true, log: log.clone(), delay_ms: 5 }));
        r.register(Box::new(RecordingTool { name: "read_c", destructive: false, log: log.clone(), delay_ms: 5 }));
        let mut c = cfg();
        c.auto_approve = true; // clear the write barrier without a prompt
        let calls = vec![
            call("1", "read_a", "{}"),
            call("2", "read_b", "{}"),
            call("3", "write_w", "{}"),
            call("4", "read_c", "{}"),
        ];
        let results = exec(&r, &calls, &c).await;
        assert!(results.iter().all(|(_, out)| out.contains("done")), "{results:?}");
        let events = log.lock().unwrap().clone();
        let pos = |e: &str| events.iter().position(|x| x == e).unwrap_or_else(|| panic!("missing {e} in {events:?}"));
        assert!(pos("read_a:end") < pos("write_w:start"), "{events:?}");
        assert!(pos("read_b:end") < pos("write_w:start"), "{events:?}");
        assert!(pos("write_w:end") < pos("read_c:start"), "{events:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_turn_leaves_valid_history_with_placeholders() {
        // The pre-fill invariant: Esc dropping the turn future mid-batch must leave a VALID
        // history (assistant tool_calls each paired with a tool result) — placeholders where the
        // work never completed. Strict gateways 400 on anything less.
        let log = std::sync::Arc::new(Mutex::new(Vec::new()));
        let mut r = ToolRegistry::new();
        r.register(Box::new(RecordingTool { name: "slow_read", destructive: false, log, delay_ms: 400 }));
        let c = cfg();
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        {
            let fut = run_agent_loop(
                scripted(vec![tool_turn("slow_read", "{}"), final_turn("done")]),
                &c,
                &r,
                &mut messages,
            );
            tokio::pin!(fut);
            // 60ms ≪ the 400ms tool sleep — the future is dropped mid-batch at scope end.
            let _ = tokio::time::timeout(std::time::Duration::from_millis(60), &mut fut).await;
        }
        assert_valid_history(&messages);
        let last = messages.last().unwrap();
        assert_eq!(last.role, "tool", "history ends in the placeholder tool result");
        assert_eq!(last.content.as_deref(), Some(INTERRUPTED_TOOL_PLACEHOLDER));
    }

    // NOTE deliberately NO test sets `tui::request_cancel()`: the flag is process-global and the
    // suite runs threaded — setting it (even for a µs) makes any concurrently-executing
    // `execute_calls`/loop test report "cancelled" (observed in practice: the alphabetically
    // adjacent barrier test flaked). The cancel-fill arm is 4 straight lines; the pre-fill DROP
    // test above pins the invariant that actually matters (valid history under interruption).

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
            usage: None,
            eager: Vec::new(),
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_calls_parallel_tolerates_duplicate_and_empty_ids() {
        // Some gateways reuse ids / emit empty ids; position-based stitching must return BOTH
        // results in original order, never overwrite or drop one (the HIGH bug the review caught).
        let r = registry();
        let calls = vec![
            call("dup", "echo", r#"{"text":"first"}"#),
            call("dup", "echo", r#"{"text":"second"}"#),
            call("", "echo", r#"{"text":"third"}"#),
        ];
        let results = exec(&r, &calls, &cfg()).await;
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

    /// Every tool result must pair with a preceding assistant tool_call — the invariant strict
    /// gateways 400 on. Run after every history-mutating operation under test.
    fn assert_valid_history(msgs: &[Message]) {
        let mut declared: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for m in msgs {
            for tc in &m.tool_calls {
                declared.insert(tc.id.as_str());
            }
            if m.role == "tool" {
                let id = m.tool_call_id.as_deref().expect("tool message carries tool_call_id");
                assert!(declared.contains(id), "tool result '{id}' has no preceding assistant tool_call");
            }
        }
    }

    #[test]
    fn clear_to_floor_evicts_old_large_keeps_recent() {
        let big = "z".repeat(2000);
        let mut msgs = vec![
            Message::system("sys"),
            Message::user("task"),
            Message::assistant_tool_calls(vec![call("1", "echo", "{}")]),
            Message::tool_result("1", big.clone()), // OLD + large → cleared
            Message::assistant_tool_calls(vec![call("2", "echo", "{}")]),
            Message::tool_result("2", big.clone()), // RECENT (within keep=1) → kept
        ];
        // target 0 → clear everything clearable.
        let stats = clear_tool_results_to_floor(&mut msgs, 1, 1024, 0, 0);
        assert!(stats.chars_reclaimed > 1900, "reclaimed most of the 2000-char body: {stats:?}");
        assert_eq!(stats.cleared, 1);
        assert_eq!(stats.failures_trimmed, 0);
        assert_eq!(msgs[3].content.as_deref(), Some(CLEARED_TOOL_PLACEHOLDER), "old result cleared");
        assert_eq!(msgs[3].tool_call_id.as_deref(), Some("1"), "tool_call_id preserved (no orphan)");
        assert_eq!(msgs[5].content.as_deref(), Some(big.as_str()), "recent result kept verbatim");
        // Non-tool messages are never touched.
        assert_eq!(msgs[0].content.as_deref(), Some("sys"));
        assert_eq!(msgs[1].content.as_deref(), Some("task"));
        assert_valid_history(&msgs);
    }

    #[test]
    fn clear_to_floor_skips_small_and_is_idempotent() {
        let mut msgs = vec![
            Message::system("sys"),
            Message::tool_result("1", "tiny"), // below min_chars → never cleared
            Message::tool_result("2", "x".repeat(2000)),
            Message::tool_result("3", "x".repeat(2000)),
        ];
        let first = clear_tool_results_to_floor(&mut msgs, 1, 1024, 0, 0);
        assert!(first.chars_reclaimed > 0);
        assert_eq!(msgs[1].content.as_deref(), Some("tiny"), "small result untouched");
        // Running again reclaims nothing (cleared bodies are shorter than min_chars).
        let second = clear_tool_results_to_floor(&mut msgs, 1, 1024, 0, 0);
        assert_eq!(second, ClearStats::default(), "idempotent — no re-clearing");
    }

    #[test]
    fn clear_to_floor_noop_when_within_keep_window() {
        let mut msgs = vec![Message::system("sys"), Message::tool_result("1", "x".repeat(2000))];
        let stats = clear_tool_results_to_floor(&mut msgs, 8, 1024, 0, 0);
        assert_eq!(stats, ClearStats::default(), "fewer tools than keep_recent");
        assert_eq!(msgs[1].content.as_deref().map(|c| c.len()), Some(2000));
    }

    #[test]
    fn clear_to_floor_stops_at_target() {
        // Three 4000-char successes ≈ 1000 tok each. Target generous enough that TWO evictions
        // suffice — the pass must stop there, not clear everything.
        let mut msgs = vec![
            Message::system("sys"),
            Message::assistant_tool_calls(vec![
                call("1", "echo", "{}"),
                call("2", "echo", "{}"),
                call("3", "echo", "{}"),
                call("4", "echo", "{}"),
            ]),
            Message::tool_result("1", "a".repeat(4000)),
            Message::tool_result("2", "b".repeat(4000)),
            Message::tool_result("3", "c".repeat(4000)),
            Message::tool_result("4", "recent".repeat(10)),
        ];
        let start = msgs.iter().map(estimate_message_tokens).sum::<usize>();
        let target = start - 1200; // one ~1000-tok eviction is not enough, two overshoot past it
        let stats = clear_tool_results_to_floor(&mut msgs, 1, 1024, target, 0);
        assert_eq!(stats.cleared, 2, "oldest-first until ≤ target, then stop: {stats:?}");
        assert_eq!(msgs[4].content.as_deref().map(|c| c.len()), Some(4000), "third success untouched");
        assert_valid_history(&msgs);
    }

    #[test]
    fn clear_to_floor_preserves_failures_then_trims_last() {
        let fail_body = format!("error: build failed\n{}", "log line\n".repeat(300));
        let exit_fail = format!("exit 1\n{}", "stderr spam\n".repeat(300));
        let mut msgs = vec![
            Message::system("sys"),
            Message::assistant_tool_calls(vec![
                call("1", "shell_run", "{}"),
                call("2", "echo", "{}"),
                call("3", "shell_run", "{}"),
                call("4", "echo", "{}"),
            ]),
            Message::tool_result("1", fail_body.clone()),
            Message::tool_result("2", "ok ".repeat(1000)), // bulky success
            Message::tool_result("3", exit_fail.clone()),
            Message::tool_result("4", "recent"),
        ];
        // Generous target: the success eviction alone reaches it → failures fully intact.
        let start = msgs.iter().map(estimate_message_tokens).sum::<usize>();
        let stats = clear_tool_results_to_floor(&mut msgs, 1, 1024, start - 500, 0);
        assert_eq!(stats.cleared, 1, "{stats:?}");
        assert_eq!(stats.failures_trimmed, 0, "failures survive pass 1 untouched");
        assert_eq!(msgs[2].content.as_deref(), Some(fail_body.as_str()));
        assert_eq!(msgs[4].content.as_deref(), Some(exit_fail.as_str()));
        // Target 0: now failures must be TRIMMED (first line + sentinel), never blanked.
        let stats2 = clear_tool_results_to_floor(&mut msgs, 1, 1024, 0, 0);
        assert_eq!(stats2.failures_trimmed, 2, "{stats2:?}");
        let t1 = msgs[2].content.as_deref().unwrap();
        assert!(t1.starts_with("error: build failed"), "first line survives: {t1}");
        assert!(t1.ends_with(FAILED_TOOL_TRIM_SUFFIX));
        let t3 = msgs[4].content.as_deref().unwrap();
        assert!(t3.starts_with("exit 1"), "exit code survives: {t3}");
        // Idempotent: a third pass finds nothing.
        let stats3 = clear_tool_results_to_floor(&mut msgs, 1, 1024, 0, 0);
        assert_eq!(stats3, ClearStats::default());
        assert_valid_history(&msgs);
    }

    #[test]
    fn failure_result_convention() {
        assert!(is_failure_result("error: no such tool"));
        assert!(is_failure_result("exit 1\nstderr"));
        assert!(is_failure_result("exit 101\n"));
        assert!(!is_failure_result("exit 0\nok"));
        assert!(!is_failure_result("plain file contents"));
        assert!(!is_failure_result("exit code unknown")); // unparsable code ≠ failure
    }

    #[test]
    fn push_nudge_collapses_same_kind_keeps_others() {
        let mut msgs = vec![Message::system("sys prompt"), Message::user("task")];
        push_nudge(&mut msgs, NUDGE_DIVERGENCE, "You repeated the same tool call(s) — v1");
        msgs.push(Message::user("more work"));
        push_nudge(&mut msgs, NUDGE_DIVERGENCE, "You repeated the same tool call(s) — v2");
        let divergence = msgs
            .iter()
            .filter(|m| m.role == "system" && m.content.as_deref().is_some_and(|c| c.starts_with(NUDGE_DIVERGENCE)))
            .count();
        assert_eq!(divergence, 1, "same-kind nudges collapse to the newest");
        assert!(msgs.last().unwrap().content.as_deref().unwrap().ends_with("v2"), "newest wins, at the tail");
        assert_eq!(msgs[0].content.as_deref(), Some("sys prompt"), "system prompt never touched");
        // A DIFFERENT kind is additive, and doesn't disturb the existing one.
        push_nudge(&mut msgs, NUDGE_CONTEXT, "Context is nearly full — wrap up");
        assert!(msgs.iter().any(|m| m.content.as_deref().is_some_and(|c| c.starts_with(NUDGE_DIVERGENCE))));
        assert!(msgs.iter().any(|m| m.content.as_deref().is_some_and(|c| c.starts_with(NUDGE_CONTEXT))));
    }

    #[test]
    fn push_nudge_never_touches_index_zero_even_if_matching() {
        // Pathological: a system PROMPT that happens to start with a nudge prefix must survive.
        let mut msgs = vec![Message::system("Context is nearly full — just kidding, SYSTEM PROMPT")];
        push_nudge(&mut msgs, NUDGE_CONTEXT, "Context is nearly full (~90%) — wrap up");
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].content.as_deref().unwrap().contains("SYSTEM PROMPT"));
    }

    #[tokio::test]
    async fn divergence_nudges_collapse_across_invocations() {
        let r = registry();
        let c = cfg();
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        for round in 0..2 {
            let chat = scripted(vec![
                tool_turn("echo", r#"{"text":"same"}"#),
                tool_turn("echo", r#"{"text":"same"}"#), // repeat → nudge + recovery turn
                tool_turn("echo", r#"{"text":"same"}"#), // repeat again → Divergence stop
            ]);
            let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
            assert_eq!(out.stop, StopReason::Divergence, "round {round}");
            messages.push(Message::user("try again"));
        }
        let nudges = messages
            .iter()
            .filter(|m| m.role == "system" && m.content.as_deref().is_some_and(|c| c.starts_with(NUDGE_DIVERGENCE)))
            .count();
        assert_eq!(nudges, 1, "two invocations, ONE divergence nudge (collapsed, not accreted)");
        assert_valid_history(&messages);
    }

    #[tokio::test]
    async fn todo_recitation_fires_replaces_and_respects_cadence() {
        let _g = todo::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        todo::set(vec![
            todo::Todo { content: "map the module".into(), status: todo::Status::Done },
            todo::Todo { content: "fix the parser".into(), status: todo::Status::InProgress },
            todo::Todo { content: "run the tests".into(), status: todo::Status::Pending },
        ]);
        let r = registry();
        let c = AgentConfig { todo_reminder_every: 2, ..cfg() };
        let mut messages = vec![Message::system("sys"), Message::user("long task")];
        // 5 distinct tool turns → reminders due after iters 2 and 4 — but COLLAPSED to one message.
        let chat = scripted(vec![
            tool_turn("echo", r#"{"text":"1"}"#),
            tool_turn("echo", r#"{"text":"2"}"#),
            tool_turn("echo", r#"{"text":"3"}"#),
            tool_turn("echo", r#"{"text":"4"}"#),
            final_turn("done"),
        ]);
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(out.stop, StopReason::Done);
        let reminders: Vec<&Message> = messages
            .iter()
            .filter(|m| m.role == "system" && m.content.as_deref().is_some_and(|c| c.starts_with(NUDGE_TODO)))
            .collect();
        assert_eq!(reminders.len(), 1, "recitations are replaced, not accreted");
        let body = reminders[0].content.as_deref().unwrap();
        assert!(body.contains("[x] map the module"), "{body}");
        assert!(body.contains("[>] fix the parser"), "{body}");
        assert!(body.contains("[ ] run the tests"), "{body}");
        assert_valid_history(&messages);

        // Empty list → no reminder at all.
        todo::clear();
        let mut messages2 = vec![Message::system("sys"), Message::user("task")];
        let chat2 = scripted(vec![
            tool_turn("echo", r#"{"text":"1"}"#),
            tool_turn("echo", r#"{"text":"2"}"#),
            tool_turn("echo", r#"{"text":"3"}"#),
            final_turn("done"),
        ]);
        run_agent_loop(chat2, &c, &r, &mut messages2).await.unwrap();
        assert!(
            !messages2.iter().any(|m| m.content.as_deref().is_some_and(|c| c.starts_with(NUDGE_TODO))),
            "no todos → no recitation"
        );
    }

    #[test]
    fn clearing_cadence_steps_not_trickles() {
        // First crossing fires.
        assert!(clearing_due(60, 3, None, 10, 6));
        // +2 points later, 1 iter later: NOT due (the trickle this cadence exists to prevent).
        assert!(!clearing_due(62, 4, Some((60, 3)), 10, 6));
        // +10 points growth: due.
        assert!(clearing_due(70, 5, Some((60, 3)), 10, 6));
        // Cooldown expiry fires even below the growth step.
        assert!(clearing_due(61, 9, Some((60, 3)), 10, 6));
        assert!(!clearing_due(61, 8, Some((60, 3)), 10, 6));
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

    /// Pin all four agent source dirs into an isolated sandbox so `<agents>` discovery is deterministic.
    fn with_agent_sandbox<T>(tag: &str, f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("ng-tlp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("USERPROFILE", &root);
        std::env::set_var("HOME", &root);
        std::env::set_var("AIZEN_HOME", root.join(".aizen"));
        std::env::set_var("NEXTGEN_HOME", root.join(".aizen"));
        std::env::set_var("NG_PROJECT_ROOT", root.join("proj"));
        let out = f(&root);
        for v in ["USERPROFILE", "HOME", "AIZEN_HOME", "NEXTGEN_HOME", "NG_PROJECT_ROOT"] {
            std::env::remove_var(v);
        }
        let _ = std::fs::remove_dir_all(&root);
        out
    }

    #[test]
    fn top_level_prompt_equals_base_when_no_agents() {
        with_agent_sandbox("none", |_root| {
            let base = build_system_prompt("/w", "linux", "2026-06-20", "m", None);
            let top = build_top_level_system_prompt("/w", "linux", "2026-06-20", "m", None);
            // Upgrade safety: a user with no agents gets a byte-identical prompt (no prefix-cache bust).
            assert_eq!(base, top);
            assert!(!top.contains("<agents>"));
        });
    }

    #[test]
    fn top_level_prompt_adds_agents_block_and_keeps_base_prefix() {
        with_agent_sandbox("some", |root| {
            let dir = root.join(".aizen/agents");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("code-reviewer.md"), "---\nname: Code Reviewer\ndescription: reviews diffs\n---\nbody").unwrap();
            let top = build_top_level_system_prompt("/w", "linux", "2026-06-20", "m", None);
            assert!(top.contains("<agents>"), "installed agent ⇒ index present");
            assert!(top.contains("task(agent="), "tells the model how to dispatch");
            assert!(top.starts_with(SYSTEM_BASE.trim_end()), "static base prefix is preserved");
            // The block is a pure SUFFIX: stripping it yields exactly the base prompt.
            let base = build_system_prompt("/w", "linux", "2026-06-20", "m", None);
            assert!(top.starts_with(&base), "agents block is appended after the unchanged base");
        });
    }
}
