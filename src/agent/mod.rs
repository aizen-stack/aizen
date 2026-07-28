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
pub mod codebase;
pub mod compact;
pub mod goal;
pub mod lsp;
pub mod mcp;
pub mod mcp_oauth;
pub mod orchestration;
pub mod process;
pub mod project_context;
pub mod reach;
pub mod repo_map;
pub mod search;
pub mod task_tool;
pub mod todo;
pub mod tools;
pub mod toolsets;
pub mod verify_gate;
pub mod web_tools;
pub mod workflow;
pub mod workflow_tool;

use crate::core::types::{Message, ToolCall, ToolDef};
use crate::llm::client::ChatTurn;
use anyhow::Result;
use console::style;
use std::future::Future;
use tools::ToolRegistry;

/// XOR-obfuscated system prompts (see `build.rs`): the plaintext is not present in the binary, so
/// `strings aizen(.exe)` can't lift it. `decode` reverses the build-time XOR; the public accessors
/// cache the result so the work happens once per process.
mod obf {
    include!(concat!(env!("OUT_DIR"), "/prompts_obf.rs"));

    pub fn decode(cipher: &[u8]) -> String {
        let key = PROMPT_KEY;
        let bytes: Vec<u8> = cipher
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ key[i % key.len()])
            .collect();
        String::from_utf8(bytes).expect("obfuscated prompt is valid UTF-8")
    }
}

/// The static (cached-prefix) base system prompt — see `system_prompt.md`. Stored obfuscated; decoded
/// once into a cached `String` (was a plaintext `include_str!` const).
pub fn system_base() -> &'static str {
    static CELL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CELL.get_or_init(|| obf::decode(obf::SYSTEM_BASE_OBF))
        .as_str()
}

/// The STRICT-tier base prompt for small/local models (numbered imperative rules, explicit output
/// contract, tool cheat sheet) — weak models follow commands, not essays. Selected by
/// [`prompt_tier_for`]; ~half the tokens of the full prompt. Obfuscated like [`system_base`].
pub fn system_base_strict() -> &'static str {
    static CELL: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CELL.get_or_init(|| obf::decode(obf::SYSTEM_BASE_STRICT_OBF))
        .as_str()
}

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
    match override_tier
        .map(|s| s.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("strict") => return PromptTier::Strict,
        Some("full") => return PromptTier::Full,
        _ => {}
    }
    let m = model.to_ascii_lowercase();
    // Small/local families + the "mini"/"nano" tier markers, matched as whole tokens so e.g.
    // "geminia" or "granite-cloud-ultra" never false-positives on a substring.
    const STRICT_FAMILIES: &[&str] = &[
        "qwen", "llama", "gemma", "phi", "granite", "smollm", "mini", "nano",
    ];
    if STRICT_FAMILIES
        .iter()
        .any(|k| crate::llm::client::contains_word(&m, k))
    {
        return PromptTier::Strict;
    }
    if m.contains("mistral-small") {
        return PromptTier::Strict;
    }
    // Explicit parameter-count suffixes (whole tokens): anything ≤32B gets the strict tier.
    const SMALL_SIZES: &[&str] = &[
        "1b", "2b", "3b", "4b", "7b", "8b", "9b", "12b", "13b", "14b", "24b", "27b", "30b", "32b",
    ];
    if SMALL_SIZES
        .iter()
        .any(|k| crate::llm::client::contains_word(&m, k))
    {
        return PromptTier::Strict;
    }
    PromptTier::Full
}

/// Static/dynamic system-prompt lanes.
///
/// The stable lane is intended to stay byte-identical for the life of a session so provider prefix
/// caches remain warm. The dynamic lane is rewritten only at fresh user-turn boundaries (persona,
/// memory, skills, visual contract, ultimate mode, recovery notes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptBundle {
    pub stable: String,
    pub dynamic: String,
}

impl PromptBundle {
    pub fn flatten(&self) -> String {
        if self.dynamic.trim().is_empty() {
            self.stable.clone()
        } else {
            format!("{}\n{}", self.stable.trim_end(), self.dynamic.trim_start())
        }
    }

    pub fn is_empty(&self) -> bool {
        self.stable.trim().is_empty() && self.dynamic.trim().is_empty()
    }
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
    build_system_prompt_bundle(cwd, os, date, model, frozen_core).flatten()
}

/// Same content as [`build_system_prompt`], split into cache-stable vs per-turn dynamic lanes.
pub fn build_system_prompt_bundle(
    cwd: &str,
    os: &str,
    date: &str,
    model: &str,
    frozen_core: Option<&str>,
) -> PromptBundle {
    // Tier is a pure function of (model, config): fixed within a session, so the prefix stays
    // byte-stable; every model switch already rebuilds the system prompt.
    let base = match prompt_tier_for(
        model,
        crate::core::cli_config::load().prompt_tier.as_deref(),
    ) {
        PromptTier::Strict => system_base_strict(),
        PromptTier::Full => system_base(),
    };
    let mut stable = String::from(base.trim_end());
    stable.push_str("\n\n<environment>\n");
    stable.push_str(&format!(
        "cwd: {cwd}\nos: {os}\ndate: {date}\nmodel: {model}\n"
    ));
    stable.push_str("</environment>\n");

    let mut dynamic = String::new();
    // Durable AGENT operating-identity (who the agent IS across every persona/project) — ABOVE the
    // persona costume and the user model. HOME-only + sanitized + fail-closed (see `crate::persona::soul`).
    if let Some(soul) = crate::persona::soul::prompt_block() {
        dynamic.push_str("\n<agent_identity>\n");
        dynamic.push_str(soul.trim());
        dynamic.push_str("\n</agent_identity>\n");
    }
    // Active character card (who the agent IS) — before user_memory (who the user is).
    if let Some(p) = crate::persona::prompt_block() {
        dynamic.push_str("\n<persona>\n");
        dynamic.push_str(p.trim());
        dynamic.push_str("\n</persona>\n");
        // The character's accumulated experience (who it has BECOME) — only meaningful with a
        // persona active, so nested under it.
        if let Some(sb) = crate::persona::self_block() {
            dynamic.push_str("\n<self>\n");
            dynamic.push_str(sb.trim());
            dynamic.push_str("\n</self>\n");
        }
    }
    if let Some(fc) = frozen_core {
        let fc = fc.trim();
        if !fc.is_empty() {
            dynamic.push_str("\n<user_memory>\n");
            dynamic.push_str(fc);
            dynamic.push_str("\n</user_memory>\n");
        }
    }
    // Compact index of saved skills (procedures); full bodies are pulled on demand via skill_load.
    if let Some(idx) = crate::skills::prompt_index() {
        dynamic.push_str("\n<skills>\n");
        dynamic.push_str(&idx);
        dynamic.push_str("\n</skills>\n");
    }
    PromptBundle { stable, dynamic }
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
    let base = match prompt_tier_for(
        model,
        crate::core::cli_config::load().prompt_tier.as_deref(),
    ) {
        PromptTier::Strict => system_base_strict(),
        PromptTier::Full => system_base(),
    };
    let mut s = String::from(base.trim_end());
    s.push_str("\n\n<environment>\n");
    s.push_str(&format!(
        "cwd: {cwd}\nos: {os}\ndate: {date}\nmodel: {model}\n"
    ));
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

/// A top-level-only output contract for terminal-native tables and diagrams. Kept as a suffix so the
/// large static base prompt remains cache-stable; sub-agents intentionally never receive it.
pub fn response_visuals_prompt_block(
    mode: crate::core::cli_config::ResponseVisuals,
) -> Option<String> {
    use crate::core::cli_config::ResponseVisuals;

    if mode == ResponseVisuals::Off {
        return None;
    }
    let requirement = match mode {
        ResponseVisuals::Auto => {
            "Use a visual only when it makes the answer materially easier to scan. Skip it for yes/no, \
             identity, clarification, short errors, exact JSON/code, raw command output, and other tiny answers."
        }
        ResponseVisuals::Always => {
            "For every substantial final answer, include at least ONE meaningful compact visual (a table OR \
             a text diagram). The same exceptions apply to yes/no, identity, clarification, short errors, \
             exact JSON/code, raw command output, and other tiny answers."
        }
        ResponseVisuals::Off => unreachable!(),
    };
    Some(format!(
        "<response_visuals mode=\"{mode}\">\n\
         Make final answers easier to scan without repeating the prose. {requirement}\n\
         - Use a Markdown table for comparisons, status/files/results/metrics, or several parallel options.\n\
         - Use a fenced `diagram` block for flows, architecture, dependencies, sequences, or state transitions.\n\
         - A table OR a diagram is enough; use both only when they communicate different information.\n\
         - Diagrams must be compact monospace text with clear labels and must not rely on color. Never emit \
         Mermaid: this terminal does not execute Mermaid.\n\
         - Lead with the result; the visual supports it and never replaces it.\n\
         </response_visuals>\n"
    ))
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
    build_top_level_system_prompt_bundle(cwd, os, date, model, frozen_core).flatten()
}

/// Top-level prompt lanes. Stable lane is environment + project conventions; dynamic lane holds
/// identity/memory/skills/agents/visuals/ultimate and is safe to rewrite at user-turn boundaries.
pub fn build_top_level_system_prompt_bundle(
    cwd: &str,
    os: &str,
    date: &str,
    model: &str,
    frozen_core: Option<&str>,
) -> PromptBundle {
    let mut bundle = build_system_prompt_bundle(cwd, os, date, model, frozen_core);
    // Project conventions (AGENTS.md / CLAUDE.md), top-level only: coder turns inherit the repo's
    // build/test commands and house rules. Kept in the stable lane so they don't thrash the cache
    // every persona/memory refresh.
    if let Some(ctx) = project_context::load_project_context(std::path::Path::new(cwd)) {
        bundle.stable.push_str("\n<project_context>\n");
        bundle.stable.push_str(&ctx);
        bundle.stable.push_str("\n</project_context>\n");
    }
    if let Some(idx) = crate::agents::prompt_index() {
        bundle.dynamic.push_str("\n<agents>\n");
        bundle.dynamic.push_str(&idx);
        bundle.dynamic.push_str("\n</agents>\n");
    }
    if let Some(block) =
        response_visuals_prompt_block(crate::core::cli_config::load().response_visuals())
    {
        bundle.dynamic.push('\n');
        bundle.dynamic.push_str(&block);
    }
    // Ultimate mode (aizen's `ultracode`): a pure SUFFIX telling the model to reason at max depth and
    // orchestrate by default. Absent when off, so the prefix stays byte-identical (zero cache bust).
    // Top-level only — sub-agents call `build_system_prompt` directly and never see this (they can't
    // fan out further anyway; the workflow tool is depth-capped at 1).
    if crate::core::cli_config::ultimate_enabled() {
        bundle.dynamic.push_str(
            "\n<ultimate_mode>\n\
             You are in ULTIMATE mode. Reason at maximum depth.\n\
             - When work decomposes into independent angles (multi-file investigation, multi-angle \
             review, broad search, parallel research), PREFER the `workflow` tool (mode=fanout, ≤5 \
             tasks) over serial `task` loops or doing it yourself. Keep writes singular (at most \
             ONE coder/writer child); fan out the reads.\n\
             - When you have claims/findings to trust, use `workflow` mode=verify to adversarially \
             refute each one before committing to conclusions.\n\
             - Use `task` for a single focused sub-problem; never nest orchestration.\n\
             - Be thorough: prefer evidence with file:line over narrative summary.\n\
             </ultimate_mode>\n",
        );
    }
    bundle
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
    /// Larger budget for the READ/FETCH tools whose output is a document scanned for specifics
    /// (`file_read`/`web_fetch`/`web_crawl`/`search_files`). The reach layer already caps a fetched
    /// page at `FETCH_CAP` (20k); cutting that again to `max_tool_result_chars` (4k) here would drop
    /// the very region relevance-truncation is meant to keep (W22). Used as a FLOOR
    /// (`max_fetch_chars.max(max_tool_result_chars)`) so these tools are never trimmed *tighter* than
    /// a plain tool — the kept window stays meaningful. Other tools stay at `max_tool_result_chars`.
    pub max_fetch_result_chars: usize,
    /// Unified approval level. `ask` prompts before destructive tools; `smart` auto-runs only shell
    /// commands classified read-only by `cmd_guard`; `yolo` pre-authorizes destructive tools. The hard
    /// blocklist applies at every level.
    pub approval_mode: crate::core::approval::ApprovalMode,
    /// Turn-scoped cooperative cancellation. Top-level turns create a fresh token; delegated
    /// task/workflow children inherit it so Esc fans out without affecting unrelated turns.
    pub cancel: crate::core::cancel::TurnCancel,
    /// Turn-scoped execution context (conversation identity, and — over time — the other per-turn
    /// facts a tool body needs). Seeded into a thread-local INSIDE the `spawn_blocking` closure
    /// alongside `cancel`, so tool bodies read this turn's identity instead of whatever the driver
    /// thread last set process-globally. Delegated children inherit the parent's context.
    pub exec_ctx: crate::core::exec_ctx::ExecutionContext,
    /// Suppress the stderr progress trace (tests set this).
    pub quiet: bool,
    /// Run a fast typecheck/build (cargo check / tsc) once after an editing run, before
    /// reporting Done; on failure inject the errors and grant one fix turn (F2 verify gate).
    pub enable_verify_gate: bool,
    /// Wall-clock cap (seconds) for the verify-gate subprocess.
    pub verify_gate_timeout_secs: u64,
    /// Stamp a one-shot time-machine checkpoint before the FIRST destructive tool call of a run, so
    /// the whole session's edits are rewindable (W15). Best-effort (no-op outside a git repo).
    /// Default `true`; tests set it `false` (their cwd is a real repo — no checkpoint pollution).
    pub auto_checkpoint: bool,
    /// Stamp a time-machine checkpoint AFTER every turn that successfully edited files (Cline-style
    /// per-step snapshots), so each editing step is an independent restore point — not just the one
    /// pre-run snapshot from `auto_checkpoint`. Best-effort + dedup'd (a zero-diff tree reuses the
    /// last snapshot), so quiet turns cost nothing. Requires `auto_checkpoint`; default `true`,
    /// forced `false` in tests (their cwd is a real repo).
    pub checkpoint_each_edit: bool,
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
    /// The one-shot "wrap up now" context guard fires when running history crosses this percent of
    /// `context_window` (P-ctx4 — was a hardcoded 90). Kept above `compact_at_pct` so a summarizer-
    /// equipped caller compacts first; the guard is the last-ditch nudge for callers WITHOUT one.
    /// Requires `context_window > 0`; `0` disables the guard.
    pub context_guard_pct: u8,
    /// Max gate-triggered fix rounds in the verify/repair loop: after an editing run, a failing
    /// typecheck injects the errors and loops back for a fix, up to this many times (then the model
    /// is allowed to finish). `1` = the old one-shot behavior; `0` disables looping entirely (the
    /// gate still needs `enable_verify_gate`). Only re-fires after the model makes NEW edits.
    pub max_verify_attempts: usize,
    /// One extra SELF-REVIEW turn before Done on runs that edited files (opt-in): re-check the
    /// diff against the request — via the `roles.oracle` model when configured, else a nudge to
    /// this model. Costs one turn per editing task; default OFF until measured worth it.
    pub enable_self_review: bool,
    /// Enable the LSP subsystem (type-aware symbol navigation + symbolic edit via a per-language
    /// server). Default ON: tools register and servers spawn lazily on first symbol query (no
    /// process until needed). Set false / `/lsp off` to reclaim RAM and hide the tools. Sub-agents
    /// and workflows keep a separate slim registry (no LSP tools).
    pub enable_lsp: bool,
    /// Per-request wall-clock cap (seconds) for an LSP query, so a hung server can never block the
    /// agent turn. Mirrors Helix's 20s default.
    pub lsp_request_timeout_secs: u64,
    /// P0.1: when the model returns text-only while session todos still have pending/in_progress
    /// items, inject a poke and continue (anti early-exit). Sub-agents leave this OFF — their plan
    /// lives in ScopedTodo, not the process-global list. Default ON for the top-level loop.
    pub enable_todo_poke: bool,
    /// Max incomplete-todo pokes per run before Done is allowed anyway. `0` disables poking even
    /// when `enable_todo_poke` is true.
    pub max_todo_poke_attempts: usize,
    /// P0.2: arm a one-shot re-check when a todo is marked done with a large confidence jump.
    pub enable_confidence_gate: bool,
    /// Confidence ≥ this (and a spike of `conf_spike_delta`) when marking done arms the gate.
    pub conf_high: u8,
    /// Minimum upward jump in confidence (at Done) that counts as a spike.
    pub conf_spike_delta: u8,
    /// P0.3: reframe + cadence nudges for quantifiable goals (optimize/perf/benchmark…).
    pub enable_hill_climb: bool,
    /// Self-reported `hill_climbable` below this on an open todo triggers a one-shot reframe.
    pub hill_climb_gate: u8,
    /// Re-nudge to re-measure every N iters while hill-climb mode is on. `0` = reframe only.
    pub hill_climb_reminder_every: usize,
    /// How many times an ORDINARY (non-goal-mode) turn retries a TRANSIENT model-call failure
    /// (429/5xx/transport/timeout) with backoff before giving up. Permanent 4xx is never retried here.
    ///
    /// `0` at the top level, deliberately: the REPL surfaces the error to a user who is right there
    /// and can re-ask, and a silent retry would just look like a hang. DELEGATED loops set it > 0 —
    /// nobody is watching a sub-agent, and one transient blip used to discard every step it had
    /// completed and come back as a bare "sub-agent (coder) failed".
    pub max_transient_retries: usize,
    /// GOAL MODE (`/goal <text>`). `Some(goal)` makes the loop run until the goal is genuinely
    /// finished — the iteration cap is bypassed (stop only on Esc or a verified completion), and
    /// transient API failures (429/5xx/timeouts/empty-200) auto-retry with backoff instead of
    /// killing the run. Completion is a two-key handshake: the model must call `goal_complete` AND
    /// the verify gate must pass. `None` (default) = ordinary behavior (cap applies, API errors are
    /// fatal after the HTTP client's own retries). The stored string is the goal text, re-injected
    /// each time the model tries to stop without having genuinely finished.
    pub goal: Option<String>,
    /// MID-TURN STEERING: drain [`crate::core::steer`] at each iteration boundary and fold anything
    /// the user typed during the run into this conversation as a `user` message. Lets "wait, also do
    /// X" land without Esc + restart (which would discard every tool result gathered so far, and the
    /// provider prompt cache with it). Only the top-level interactive loop sets this — sub-agents and
    /// workflow children leave it `false` so a steer aimed at the main task can't be swallowed by a
    /// delegated child. Default `false` (the mailbox is process-global; opting in is explicit).
    pub enable_steering: bool,
    /// MID-TURN PERSISTENCE hook, called at each iteration boundary with the conversation so far.
    ///
    /// The loop borrows `messages` mutably for the whole turn, so nothing outside it can observe
    /// progress — which meant a terminal closed mid-turn persisted the user's question and lost every
    /// assistant reply and tool result the turn had already produced. Called at the same boundary
    /// steering drains, the only point where history is guaranteed coherent (no assistant `tool_calls`
    /// awaiting their results), so the observer never sees a shape a strict gateway would reject.
    ///
    /// A plain `fn` pointer, not a closure: `AgentConfig` derives `Clone + Debug` and is threaded
    /// through every sub-agent spawn, so a boxed `dyn Fn` would cost both derives. `None` ⇒ no
    /// observer (sub-agents and workflow children: their transcripts aren't the user's session).
    pub on_progress: Option<fn(&[Message])>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            max_iters: 25,
            auto_extend_to: 50,
            max_tool_result_chars: 4096,
            max_fetch_result_chars: 12_000,
            approval_mode: crate::core::approval::ApprovalMode::Ask,
            cancel: crate::core::cancel::TurnCancel::new(),
            exec_ctx: crate::core::exec_ctx::ExecutionContext::default(),
            quiet: false,
            enable_verify_gate: true,
            verify_gate_timeout_secs: 90,
            auto_checkpoint: true,
            checkpoint_each_edit: true,
            context_window: 0,
            keep_recent_tool_results: 8,
            clear_tool_result_min_chars: 1024,
            clear_at_pct: 60,
            clear_target_pct: 45,
            clear_step_pct: 10,
            clear_cooldown_iters: 6,
            todo_reminder_every: 8,
            compact_at_pct: 80,
            context_guard_pct: 90,
            max_verify_attempts: 2,
            enable_self_review: false,
            enable_lsp: true,
            lsp_request_timeout_secs: 20,
            enable_todo_poke: true,
            max_todo_poke_attempts: 2,
            enable_confidence_gate: true,
            conf_high: 90,
            conf_spike_delta: 40,
            enable_hill_climb: true,
            hill_climb_gate: 90,
            hill_climb_reminder_every: 6,
            max_transient_retries: 0, // top level: the user sees the error and can re-ask
            goal: None,
            enable_steering: false,
            on_progress: None,
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
    /// The verification gate ran and exhausted its repair budget without a passing result.
    VerificationFailed,
    /// The model invoked `clarify` — the turn is PAUSED pending the user's answer (the carried
    /// string is the user-facing question + options). The caller surfaces it and the next user
    /// message re-enters the loop as the answer. History is left valid (the assistant tool-call
    /// turn and its tool result are already appended).
    AwaitingInput(String),
    /// The user cancelled mid-loop (Esc / cancel flag). Cooperative: no further model/tool work.
    /// Nested sub-agents (`task` / `workflow` children) observe the same process-global flag and
    /// stop at their next loop boundary instead of running to max_iters.
    Cancelled,
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
    run_agent_loop_inner(
        chat,
        no_summarizer,
        None::<NoSummarizer>,
        cfg,
        registry,
        messages,
    )
    .await
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
    run_agent_loop_inner(
        chat,
        Some(summarize),
        None::<NoSummarizer>,
        cfg,
        registry,
        messages,
    )
    .await
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
    // Capture the real request before verify/todo/goal gates can append synthetic user turns. Prefix
    // stripping keeps automatic memory/codebase retrieval out of the review contract.
    let mut review_request = capture_review_request(messages);
    // Run-scoped Time Machine anchors (pre_edit / last_good / rewind budget). Must reset every loop
    // entry so a prior task's safety net cannot be restored into this task by accident.
    crate::features::timemachine::begin_agent_run();
    let defs = registry.defs();
    // Tool schemas ride on EVERY request but live in no message — count them once here so the
    // context guards below compare the real request size against the window.
    let schema_overhead = estimate_defs_tokens(&defs);
    let mut cap = cfg.max_iters;
    let mut extended = false;
    // W1/W6: ring of recent EXECUTED-turn signatures for A,A + A,B,A,B detection, plus per-
    // signature nudge memory. nudged_sigs is cleared by any productive turn, so a legitimately
    // repeated call AFTER real progress earns a fresh nudge instead of an instant hard-stop (the
    // old run-global one-shot never reset). recent_sigs/nudged_sigs are plain trackers, never
    // `messages`, so they can never orphan an assistant/tool pair.
    let mut recent_sigs: std::collections::VecDeque<String> =
        std::collections::VecDeque::with_capacity(SIG_RING);
    let mut nudged_sigs: std::collections::HashSet<String> = std::collections::HashSet::new();
    // W3/W4: hashes of every NON-failure tool-result body seen this run. Novel content = progress —
    // the shared signal that resets the thrash streak, resets the divergence latch, and gates
    // auto-extend. A stale re-read (already-seen bytes) is not novel, so it can't rescue a flail.
    let mut seen_results: std::collections::HashSet<u64> = std::collections::HashSet::new();
    // THRASH GUARD: consecutive UNPRODUCTIVE turns (no successful edit and no novel tool output).
    // Catches a model flailing with distinct-but-fruitless calls that dodge the divergence check
    // (which only fires on repeated/oscillating signatures). Nudges once, then stops, so a stuck
    // run can't burn the whole step budget.
    let mut unproductive_streak = 0usize;
    let mut stuck_nudged = false;
    let mut verify_attempts = 0usize;
    // The verify gate PASSED for the current tree state (W8). Set when a check comes back clean;
    // CLEARED by a fresh successful edit (new work must be re-verified). While false and edits
    // exist, the gate re-fires on every "done" claim until it passes or attempts exhaust — so a
    // model can't finish by re-asserting "done" without fixing the breakage.
    let mut verify_passed = false;
    // CUMULATIVE edit flag — set once any successful edit lands; arms the one-shot self-review AND
    // gates the verify gate (a run that never edited has nothing to verify).
    let mut made_any_edits = false;
    // Operation-scoped checkpoint latch: set only after a pre-edit checkpoint succeeds. Approval is
    // evaluated first; a declined call must never run Git hooks/filters or mutate recovery metadata.
    let mut auto_checkpointed = false;
    let mut writer_lease: Option<crate::core::workspace_txn::WorkspaceWriterLease> = None;
    crate::core::recovery::set_phase(crate::core::recovery::RecoveryPhase::WaitingModel);
    let mut self_review_done = false;
    let mut context_warned = false;
    // P-ctx1: the last budget band we surfaced to the model (see `budget_band`). Injected only on a
    // band change so the running budget `system` nudge stays cache-stable within a band. Reset when
    // history shrinks (clear/compact) so the signal re-arms honestly against the new, smaller size.
    let mut budget_band_shown: Option<u8> = None;
    // P-ctx2: one-shot latch — the FIRST time history reaches the clearing threshold we warn the
    // model to persist anything durable and SKIP that pass, so the eviction happens a turn later
    // with the important content already saved. Never fires again (subsequent clears are silent).
    let mut save_before_clear_warned = false;
    // Provider-reported prompt size at the last usage-carrying call (see `RealAnchor`) —
    // invalidated whenever history is mutated (clearing/compaction shrink what we'd send next).
    let mut real_anchor: Option<RealAnchor> = None;
    // Clearing cadence: (pct-of-window after the last clear, iter at the last clear).
    let mut last_clear: Option<(usize, usize)> = None;
    // Compaction cadence: (pct-of-window after the last compaction attempt, iter at that attempt).
    // Mirrors `last_clear`. WITHOUT this, the compaction trigger below re-fires on every consecutive
    // iteration once history is long enough — each pass re-splices mid-history and busts the prompt
    // cache from the splice point. The guard makes compaction fire in big infrequent jumps (like
    // clearing), not a per-turn cache-shredding trickle: it only re-arms when usage grows by
    // `clear_step_pct` OR `clear_cooldown_iters` iterations have elapsed since the last attempt.
    let mut last_compact: Option<(usize, usize)> = None;
    // Iter of the last todo-recitation reminder (0 = none yet).
    let mut last_todo_reminder = 0usize;
    // P0.1: incomplete-todo pokes this run (cap = max_todo_poke_attempts).
    let mut todo_poke_attempts = 0usize;
    // P0.2: last confidence per todo content key; spike at Done arms a one-shot gate.
    let mut conf_last: std::collections::HashMap<String, u8> = std::collections::HashMap::new();
    let mut confidence_gate_armed = false;
    let mut confidence_gate_cleared = false;
    // P0.3: hill-climb mode + one-shot reframe + cadence.
    let mut hill_climb_on = cfg.enable_hill_climb && task_looks_hill_climbable(messages);
    let mut hill_climb_reframed = false;
    let mut last_hill_climb_reminder = 0usize;
    let mut iter = 0usize;
    // GOAL MODE bypasses the iteration cap entirely: `/goal` promises to run until the goal is
    // genuinely finished, so the only exits are Esc (→ Cancelled, via `cancel::race`) or a verified
    // completion (→ Done, via the goal gate + verify gate). Ordinary turns keep `iter < cap`.
    let goal_mode = cfg.goal.is_some();

    loop {
        // COOPERATIVE CANCEL wins before any cap bookkeeping. Goal mode bypasses the cap exactly as
        // before; ordinary runs get one convergent extension at this single boundary, regardless of
        // whether the previous iteration reached it through a tool path or a Done-gate `continue`.
        if cfg.cancel.is_cancelled() {
            return Ok(AgentOutcome {
                final_text: None,
                iters: iter,
                stop: StopReason::Cancelled,
            });
        }
        if !goal_mode && iter >= cap {
            if should_auto_extend(
                cfg.max_iters,
                cap,
                cfg.auto_extend_to,
                extended,
                unproductive_streak,
                !nudged_sigs.is_empty(),
            ) {
                extended = true;
                cap = cfg.auto_extend_to;
                push_nudge(
                    messages,
                    NUDGE_STEP_LIMIT,
                    "You are nearing the step limit. Finish the task now, or stop and state what is blocking you.",
                );
            } else {
                break;
            }
        }

        // STEERING: mid-turn course correction. The input thread can hand a message to the RUNNING
        // turn instead of the post-turn queue (Alt+Enter, Ctrl-S, or a `>` prefix), so "also do X" lands
        // without an Esc + restart. Drained HERE — the top of an iteration is the only point where
        // history is guaranteed consistent (every assistant `tool_calls` already paired with its
        // results), so appending a `user` message can't strand a dangling call. Only the top-level
        // interactive loop opts in (`cfg.enable_steering`); sub-agents and workflows ignore the
        // channel so a steer meant for the main turn can't leak into a delegated child.
        if cfg.enable_steering {
            let steers = crate::core::steer::drain();
            if !steers.is_empty() {
                if !cfg.quiet {
                    for s in &steers {
                        emit_trace(
                            &crate::ui::theme::accent(format!(
                                "⤳ steering: {}",
                                first_line_clip(s, 72)
                            ))
                            .to_string(),
                        );
                    }
                }
                messages.push(Message::user(crate::core::steer::format_injection(&steers)));
            }
        }

        // PUBLISH the in-flight transcript for crash/close safety. Same boundary as the steering
        // drain, for the same reason: this is the only point where history is guaranteed coherent
        // (every assistant `tool_calls` already paired with its results), so a snapshot taken here is
        // always a transcript that can be reloaded. The REPL owns `messages` mutably for the whole
        // turn, so without this hook a terminal closed mid-turn persisted the user's question and
        // discarded every reply and tool result the turn had already produced.
        if let Some(publish) = cfg.on_progress {
            publish(messages);
        }

        // Effective request size for ALL guards this iteration: estimate (messages + tool schemas)
        // corrected by the provider's last real usage report when we have one. Recomputed after any
        // guard mutates history.
        let mut est_now = effective_tokens(
            estimate_tokens(messages) + schema_overhead,
            real_anchor.as_ref(),
        );

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
            if clearing_due(
                pct,
                iter,
                last_clear,
                cfg.clear_step_pct,
                cfg.clear_cooldown_iters,
            ) {
                // SAVE-BEFORE-CLEAR (P-ctx2): the first eviction of a run is the moment stale
                // tool-result bodies leave context for good — the single biggest source of "it
                // forgot the workaround we found" complaints. So the FIRST time we're due to clear,
                // don't: warn the model to persist anything durable (memory files, todo_write) while
                // the results are still here, then let THIS turn run with the warning + full context.
                // The eviction happens next turn (latch set, cadence NOT armed → clearing_due stays
                // true), by which point the important content is saved. One-shot; later clears are
                // silent (the model has been told the rule once).
                if !save_before_clear_warned {
                    save_before_clear_warned = true;
                    push_nudge(
                        messages,
                        NUDGE_SAVE_BEFORE_CLEAR,
                        "Context is filling up, so older tool results will start being dropped from \
                         history to make room. BEFORE that happens: if any earlier command output, \
                         file content, fix, or workaround still matters for this task, save it now — \
                         write it to a memory file or record it with todo_write. Details you don't \
                         persist will be gone from context after this.",
                    );
                    // Skip the eviction this pass; do NOT arm the cadence, so the next iteration is
                    // still "due" and actually clears — now that the model has had a turn to save.
                } else {
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
                        // Cleared result BODIES are gone from context, but their content hashes would
                        // linger in seen_results and mark a legitimate RE-READ of that now-evicted content
                        // as "not novel" → falsely unproductive → a spurious thrash stop. Drop the novelty
                        // memory too, so re-reading evicted content counts as progress again.
                        seen_results.clear();
                        est_now = estimate_tokens(messages) + schema_overhead;
                        budget_band_shown = None; // history shrank — re-arm the running budget signal (P-ctx1)
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
                let pct = est_now * 100 / cfg.context_window;
                // CADENCE GUARD (mirror of the clearing path): don't re-compact every iteration once
                // history sits above `compact_at_pct`. compact_history keeps the last KEEP_TURNS
                // verbatim, so a single big turn can leave the result still above threshold — without
                // this the condition stays true and re-splices (cache-busting) every turn. Re-arm only
                // on `clear_step_pct` growth or after `clear_cooldown_iters` iters (same knobs as
                // clearing — one cadence policy for both history-shrinking guards).
                if clearing_due(
                    pct,
                    iter,
                    last_compact,
                    cfg.clear_step_pct,
                    cfg.clear_cooldown_iters,
                ) {
                    if let Ok((before, after)) =
                        compact::compact_history(messages, summarize, compact::KEEP_TURNS).await
                    {
                        context_warned = false; // history shrank — let the wrap-up nudge re-arm if it refills
                        budget_band_shown = None; // …and the running budget signal (P-ctx1)
                        real_anchor = None; // spliced history invalidates the anchor
                        seen_results.clear(); // summarized-away results must not mark a re-read as stale
                        est_now = estimate_tokens(messages) + schema_overhead;
                        if !cfg.quiet {
                            let line =
                                format!("→ context: auto-compacted ~{before} → ~{after} tok");
                            if crate::ui::tui::active() {
                                crate::ui::tui::emit_line(&line);
                            } else {
                                eprintln!("{line}");
                            }
                        }
                    }
                    // Arm the cadence even when compaction was a no-op (history too short to cut) or
                    // barely dented size — re-attempting the same summarize every iteration buys
                    // nothing and each attempt is a model round-trip. Recompute pct against the
                    // (possibly shrunk) history so the latch reflects the post-compaction size.
                    last_compact = Some((est_now * 100 / cfg.context_window, iter));
                }
            }
        }

        // RUNNING CONTEXT BUDGET (P-ctx1): give the model an explicit token budget it can plan
        // against — the way context-aware Claude models get a server-side `<budget>` tag plus a
        // running `<system_warning>` after each tool call. We can't touch the system prompt
        // server-side, so we inject one collapsible `system` nudge instead — but only when usage
        // crosses a NEW band (≥50%, per decile). Refreshing every turn would rewrite a mid-history
        // message each iteration and bust the prompt cache from that byte onward — trading the very
        // context we're conserving for churn. Band-gated, it stays cache-stable within a band while
        // still escalating as pressure climbs. Off when `context_window == 0` (sub-agents).
        if cfg.context_window > 0 {
            if let Some(band) = budget_band(est_now, cfg.context_window) {
                if budget_band_shown != Some(band) {
                    budget_band_shown = Some(band);
                    push_nudge(
                        messages,
                        NUDGE_BUDGET,
                        &budget_nudge_text(est_now, cfg.context_window),
                    );
                }
            }
        }

        // MID-LOOP CONTEXT GUARD: a single run-away loop (e.g. reading many large files) can blow
        // past the window BEFORE control returns to the REPL's auto-compact (which only runs
        // between turns). When the running history crosses ~90% of the window, inject a ONE-TIME
        // "wrap up" nudge so the model acts on what it has rather than overflowing. Pure arithmetic
        // (chars/4 + the real-usage anchor) — NOT a mid-loop summarization model call, and no
        // tokenizer dep. Disabled when `context_window == 0` (sub-agents / unconfigured). This runs
        // AFTER the budget nudge so the wrap-up is the tail message — the error-rollback `pop()`
        // below removes exactly it, leaving the persistent budget signal intact.
        let nudge_pushed = if cfg.context_window > 0
            && cfg.context_guard_pct > 0
            && !context_warned
            && est_now * 100 >= cfg.context_window * cfg.context_guard_pct as usize
        {
            context_warned = true;
            push_nudge(
                messages,
                NUDGE_CONTEXT,
                &format!(
                    "Context is nearly full (~{}% of the window). Wrap up now: stop gathering more, act \
                     on what you already have, and give your final answer — or state what is blocking you.",
                    cfg.context_guard_pct
                ),
            );
            true
        } else {
            false
        };

        // Roll back the just-appended nudge if the model call fails, so a network/gateway error
        // doesn't strand an unanswered system message at the tail of history (the REPL's error path
        // only pops a trailing `user` message, so it wouldn't clean this up).
        //
        // GOAL MODE (cfg.goal.is_some()) wraps the call in a smart retry loop instead of failing
        // fatally: the whole point of `/goal` is to survive a flaky API and keep working until the
        // goal is genuinely done. Transient failures (429/5xx/transport/timeout) AND empty-200s
        // (HTTP 200 but no content and no tool_calls — this provider does that a lot) retry
        // indefinitely with growing backoff; permanent client errors (400/401/403/404) retry only a
        // few times (the provider may be briefly misbehaving) then surface the error. Esc still exits
        // cleanly every attempt via `cancel::race`. Ordinary turns keep the old fatal-on-error path.
        let mut turn = if cfg.goal.is_some() {
            const GOAL_PERMANENT_RETRIES: u32 = 3;
            let mut attempt: u32 = 0;
            let mut permanent_tries: u32 = 0;
            loop {
                match crate::core::cancel::race(&cfg.cancel, chat(messages.clone(), defs.clone()))
                    .await
                {
                    None => {
                        if nudge_pushed {
                            messages.pop();
                        }
                        return Ok(AgentOutcome {
                            final_text: None,
                            iters: iter,
                            stop: StopReason::Cancelled,
                        });
                    }
                    Some(Ok(t)) => {
                        // EMPTY-200: a 200 with neither content nor a tool call, and no completion
                        // claimed this turn, is a silent provider failure — not a real "done". Retry
                        // it with backoff rather than feeding an empty turn into the done cascade.
                        let empty_200 = t.tool_calls.is_empty()
                            && t.content
                                .as_deref()
                                .map(|s| s.trim().is_empty())
                                .unwrap_or(true)
                            && !goal::is_pending();
                        if empty_200 {
                            let delay = crate::llm::client::goal_backoff_ms(attempt);
                            attempt += 1;
                            goal_retry_line(&format!("empty response; retry #{attempt}"), delay);
                            if goal_sleep_or_cancel(&cfg.cancel, delay).await {
                                if nudge_pushed {
                                    messages.pop();
                                }
                                return Ok(AgentOutcome {
                                    final_text: None,
                                    iters: iter,
                                    stop: StopReason::Cancelled,
                                });
                            }
                            continue;
                        }
                        break t;
                    }
                    Some(Err(e)) => match crate::llm::client::classify_api_error(&e) {
                        crate::llm::client::ApiErrorKind::Permanent => {
                            permanent_tries += 1;
                            if permanent_tries > GOAL_PERMANENT_RETRIES {
                                if nudge_pushed {
                                    messages.pop();
                                }
                                return Err(e);
                            }
                            let delay = crate::llm::client::goal_backoff_ms(attempt);
                            attempt += 1;
                            goal_retry_line(
                                    &format!("client error; retry {permanent_tries}/{GOAL_PERMANENT_RETRIES}"),
                                    delay,
                                );
                            if goal_sleep_or_cancel(&cfg.cancel, delay).await {
                                if nudge_pushed {
                                    messages.pop();
                                }
                                return Ok(AgentOutcome {
                                    final_text: None,
                                    iters: iter,
                                    stop: StopReason::Cancelled,
                                });
                            }
                        }
                        crate::llm::client::ApiErrorKind::Transient => {
                            let delay = crate::llm::client::goal_backoff_ms(attempt);
                            attempt += 1;
                            goal_retry_line(&format!("API error; retry #{attempt}"), delay);
                            if goal_sleep_or_cancel(&cfg.cancel, delay).await {
                                if nudge_pushed {
                                    messages.pop();
                                }
                                return Ok(AgentOutcome {
                                    final_text: None,
                                    iters: iter,
                                    stop: StopReason::Cancelled,
                                });
                            }
                        }
                    },
                }
            }
        } else {
            // ORDINARY TURNS: a bounded TRANSIENT retry, then fatal. `max_transient_retries` is 0 at
            // the top level (unchanged behavior: the REPL shows the error and the user is right there
            // to re-ask), but non-zero for a delegated sub-agent — nobody is watching that loop, and
            // one 429/5xx mid-run used to throw away every step of work it had already done and
            // surface as a bare "sub-agent failed". Permanent 4xx never retries here: it can't fix
            // itself and burning backoff on it only delays the report.
            let mut attempt: u32 = 0;
            loop {
                match crate::core::cancel::race(&cfg.cancel, chat(messages.clone(), defs.clone()))
                    .await
                {
                    None => {
                        if nudge_pushed {
                            messages.pop();
                        }
                        return Ok(AgentOutcome {
                            final_text: None,
                            iters: iter,
                            stop: StopReason::Cancelled,
                        });
                    }
                    Some(Ok(t)) => break t,
                    Some(Err(e)) => {
                        let transient = matches!(
                            crate::llm::client::classify_api_error(&e),
                            crate::llm::client::ApiErrorKind::Transient
                        );
                        if !transient || attempt as usize >= cfg.max_transient_retries {
                            if nudge_pushed {
                                messages.pop();
                            }
                            return Err(e);
                        }
                        let delay = crate::llm::client::goal_backoff_ms(attempt);
                        attempt += 1;
                        if !cfg.quiet {
                            goal_retry_line(
                                &format!(
                                    "API error; retry {attempt}/{}",
                                    cfg.max_transient_retries
                                ),
                                delay,
                            );
                        }
                        if goal_sleep_or_cancel(&cfg.cancel, delay).await {
                            if nudge_pushed {
                                messages.pop();
                            }
                            return Ok(AgentOutcome {
                                final_text: None,
                                iters: iter,
                                stop: StopReason::Cancelled,
                            });
                        }
                    }
                }
            }
        };

        // REAL-USAGE ANCHOR: when the provider reports how many prompt tokens THIS request really
        // was, trust that over chars/4 — the guards then track growth as (estimate delta) on top of
        // the real base. `est_now` was the estimate of the exact request just sent, so the pair is
        // coherent. Insane reports (cumulative gateways, tool-exclusive counts) fail the clamp.
        if let Some(p) = turn.usage.as_ref().and_then(|u| u.prompt_tokens) {
            let real = p as usize;
            if accept_anchor(real, est_now) {
                real_anchor = Some(RealAnchor {
                    tokens: real,
                    est_at: est_now,
                });
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
            // VERIFY/REPAIR GATE (F2 + W8): after an editing run, run a fast typecheck before Done.
            // On failure, record the premature "done", inject the errors, and loop back for a fix.
            // The gate RE-FIRES on EVERY "done" claim until it PASSES or the attempt budget
            // (`max_verify_attempts`, a monotonic total-run cap that bounds the repair loop) is
            // spent — it is NOT consumed by a single edit, so a model can no longer skip
            // verification by simply re-asserting "done" without editing (W8). `verify_passed`
            // latches a clean check; a FRESH successful edit clears it (see the edit block) so new
            // work is always re-verified. Best-effort: an unknown project / missing toolchain → the
            // gate returns None and this turn falls through to Done (never loops on absence).
            if cfg.enable_verify_gate
                && made_any_edits
                && !verify_passed
                && verify_attempts < cfg.max_verify_attempts
            {
                // Canonicalize to match the tool-registry root (`builtin::resolve_root`), so the
                // gate typechecks the same tree the file tools were confined to.
                let cwd = std::env::current_dir()
                    .and_then(|p| p.canonicalize())
                    .unwrap_or_else(|_| std::path::PathBuf::from("."));
                if let Some(result) =
                    verify_gate::run_verify_gate(&cwd, cfg.verify_gate_timeout_secs).await
                {
                    if !cfg.quiet {
                        if result.passed {
                            // The mockup's green success line — `✓ <cmd> — verify gate passed`.
                            crate::ui::tui::verify_line(&result.command, "verify gate passed");
                        } else {
                            let line = format!(
                                "→ verify: {} FAILED (attempt {}/{})",
                                result.command,
                                verify_attempts + 1,
                                cfg.max_verify_attempts,
                            );
                            if crate::ui::tui::active() {
                                crate::ui::tui::emit_line(&line);
                            } else {
                                eprintln!("{line}");
                            }
                        }
                    }
                    if !result.passed {
                        verify_attempts += 1;
                        // GOAL MODE: a failed verify invalidates any completion claim the model made
                        // this turn — clear it so the stale claim can't leak through the goal gate to
                        // Done after the model fixes the errors. The `goal_complete` ack already tells
                        // the model to call the tool AGAIN once verification passes, so re-declaration
                        // is the contract; without this clear, a model that fixes-then-just-stops
                        // (no re-declare) would slip past on the old claim.
                        if cfg.goal.is_some() {
                            goal::clear();
                        }
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
                    // PASSED: latch it so a subsequent no-edit "done" doesn't needlessly re-run the
                    // gate (a fresh successful edit clears the latch → new work is re-verified).
                    verify_passed = true;
                }
            }
            if cfg.enable_verify_gate
                && made_any_edits
                && !verify_passed
                && verify_attempts >= cfg.max_verify_attempts
            {
                if let Some(t) = &turn.content {
                    if !t.trim().is_empty() {
                        messages.push(Message::assistant(t.clone()));
                    }
                }
                return Ok(AgentOutcome {
                    final_text: turn.content,
                    iters: iter + 1,
                    stop: StopReason::VerificationFailed,
                });
            }
            // SELF-REVIEW (opt-in, once per run): after the verify gate is satisfied and before
            // Done, spend ONE extra turn checking the work against the original request. Oracle
            // mode (roles.oracle configured → closure supplied) has a stronger model review the
            // `git diff` — its findings come back as a fix-or-rebut turn; an LGTM costs nothing
            // extra. Nudge mode makes THIS model re-read its own diff.
            if cfg.enable_self_review && made_any_edits && !self_review_done {
                self_review_done = true;
                match &oracle {
                    // ORACLE MODE: the verify step gates Done. A review with a [BLOCKING] finding
                    // costs one fix-or-rebut turn; an all-[ADVISORY] review is surfaced ONCE as a
                    // note (so the user sees the cleanup suggestions) but does NOT hold Done — the
                    // model isn't forced to churn on style. LGTM / no-diff falls straight through.
                    Some(o) => {
                        if let Some(review) = oracle_review(o, &review_request).await {
                            if review.blocking {
                                // Record the premature "done" so the review turn reads coherently,
                                // then hand back a fix-or-rebut turn (the verify step gates Done).
                                if let Some(t) = &turn.content {
                                    if !t.trim().is_empty() {
                                        messages.push(Message::assistant(t.clone()));
                                    }
                                }
                                messages.push(Message::user(format!(
                                    "[self-review]\n{}\n\nFix each [BLOCKING] item above, or state briefly why it does not apply — then give your final answer. [ADVISORY] items are optional.",
                                    review.findings
                                )));
                                iter += 1;
                                continue;
                            }
                            // Advisory-only: surface the notes ONCE as a trace (they don't gate
                            // Done — the model isn't forced to churn on style), then finish this
                            // turn. Falls through to the shared Done return below, which records the
                            // final assistant text exactly once (no duplicate).
                            if !cfg.quiet {
                                emit_trace(&format!(
                                    "  └ self-review: advisory only (no blocking issue)\n{}",
                                    review.findings
                                ));
                            }
                        }
                        // else: LGTM / no diff → fall through to Done, no extra turn.
                    }
                    // NUDGE MODE (no oracle): the model re-reads its own diff. One turn, always.
                    None => {
                        if let Some(t) = &turn.content {
                            if !t.trim().is_empty() {
                                messages.push(Message::assistant(t.clone()));
                            }
                        }
                        messages.push(Message::user(SELF_REVIEW_NUDGE.to_string()));
                        iter += 1;
                        continue;
                    }
                }
            }

            // P0.1 INCOMPLETE-TODO GATE: text-only while session todos still open → poke, don't Done.
            // Empty list is a no-op (trivial tasks / model never used todos). Exhausted budget → Done.
            // Sub-agents leave enable_todo_poke false (process-global list ≠ ScopedTodo).
            if cfg.enable_todo_poke
                && cfg.max_todo_poke_attempts > 0
                && todo_poke_attempts < cfg.max_todo_poke_attempts
            {
                if let Some(summary) = todo::incomplete_summary(600) {
                    todo_poke_attempts += 1;
                    if !cfg.quiet {
                        let line = format!(
                            "→ todo-poke: incomplete (attempt {}/{})",
                            todo_poke_attempts, cfg.max_todo_poke_attempts
                        );
                        if crate::ui::tui::active() {
                            crate::ui::tui::emit_line(&line);
                        } else {
                            eprintln!("{line}");
                        }
                    }
                    messages.push(Message {
                        role: "assistant".to_string(),
                        content: Some(turn.content.clone().unwrap_or_default()),
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                        images: Vec::new(),
                        cache_control: None,
                    });
                    messages.push(Message::user(format!(
                        "{TODO_POKE_PREFIX} Session todos are still incomplete — you may not finish yet.\n\n\
                         Incomplete:\n{summary}\n\n\
                         Either (a) finish the remaining items and verify, or (b) mark items done only \
                         if genuinely complete, or (c) clear the todo list to abandon the plan — then stop.\n\
                         Attempt {todo_poke_attempts}/{}.",
                        cfg.max_todo_poke_attempts
                    )));
                    iter += 1;
                    continue;
                }
            }

            // P0.2 CONFIDENCE GATE: Done + large confidence jump → one-shot re-check turn.
            if cfg.enable_confidence_gate && confidence_gate_armed && !confidence_gate_cleared {
                confidence_gate_cleared = true;
                if !cfg.quiet {
                    let line = "→ confidence-gate: large jump at done — re-check once";
                    if crate::ui::tui::active() {
                        crate::ui::tui::emit_line(line);
                    } else {
                        eprintln!("{line}");
                    }
                }
                messages.push(Message {
                    role: "assistant".to_string(),
                    content: Some(turn.content.clone().unwrap_or_default()),
                    tool_calls: Vec::new(),
                    tool_call_id: None,
                    images: Vec::new(),
                    cache_control: None,
                });
                messages.push(Message::user(format!(
                    "{CONFIDENCE_GATE_PREFIX} You marked todo(s) done with a large confidence jump \
                     without stepwise evidence.\n\n\
                     Before finishing: re-run the relevant check (tests / verify / metric). \
                     If checks pass, keep Done. If not, reopen the todo and fix.\n\
                     This gate fires once per run."
                )));
                iter += 1;
                continue;
            }

            // GOAL GATE (`/goal <text>`): the second key of goal mode's completion handshake. A turn
            // that stops WITHOUT the model having declared completion via `goal_complete` is NOT done
            // — re-inject the goal text and keep working (mirrors the todo-poke gate's shape). Only a
            // turn that DID declare completion (its `PENDING` claim, drained here by `take_pending`)
            // is allowed to fall through to Done. The verify gate above is the FIRST key: it has
            // already run and PASSED by the time control reaches here, because a failing verify
            // `continue`s earlier in this same block. So Done in goal mode ⟺ declared + verified.
            // There is no iteration cap in goal mode, so this poke can re-fire indefinitely — the run
            // ends only on genuine completion (here) or Esc (`cancel::race` above).
            if let Some(goal_text) = &cfg.goal {
                if goal::take_pending().is_none() {
                    if !cfg.quiet {
                        let line = "→ goal: not complete yet — keep working";
                        if crate::ui::tui::active() {
                            crate::ui::tui::emit_line(line);
                        } else {
                            eprintln!("{line}");
                        }
                    }
                    // Record the premature stop (content or "") so history stays coherent, then poke.
                    messages.push(Message {
                        role: "assistant".to_string(),
                        content: Some(turn.content.clone().unwrap_or_default()),
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                        images: Vec::new(),
                        cache_control: None,
                    });
                    messages.push(Message::user(format!(
                        "{GOAL_POKE_PREFIX} The goal is NOT complete yet:\n\n{goal_text}\n\n\
                         Keep working until every part of it is genuinely done. When — and only when \
                         — you have nothing left to do, call the `goal_complete` tool with a short \
                         summary of what you accomplished. Do not stop before then."
                    )));
                    iter += 1;
                    continue;
                }
                // Declared complete AND verified (or nothing to verify) → fall through to Done.
            }

            // STEERING GATE: a steer typed while the model was composing its FINAL answer arrived
            // after the top-of-loop drain, so without this check it would sit in the mailbox until
            // `disarm` re-queued it as a fresh turn — the user's correction landing one turn late,
            // after the work it meant to redirect was already reported done. Draining here keeps the
            // run alive for one more iteration instead. Mirrors the todo-poke/goal gates' shape:
            // record the (premature) assistant text so history reads coherently, inject, continue.
            if cfg.enable_steering {
                let steers = crate::core::steer::drain();
                if !steers.is_empty() {
                    let injected = crate::core::steer::format_injection(&steers);
                    review_request.push_str("\n\nSteering requirements:\n");
                    review_request.push_str(&injected);
                    if !cfg.quiet {
                        for s in &steers {
                            emit_trace(
                                &crate::ui::theme::accent(format!(
                                    "⤳ steering: {}",
                                    first_line_clip(s, 72)
                                ))
                                .to_string(),
                            );
                        }
                    }
                    messages.push(Message {
                        role: "assistant".to_string(),
                        content: Some(turn.content.clone().unwrap_or_default()),
                        tool_calls: Vec::new(),
                        tool_call_id: None,
                        images: Vec::new(),
                        cache_control: None,
                    });
                    messages.push(Message::user(crate::core::steer::format_injection(&steers)));
                    iter += 1;
                    continue;
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

        // DIVERGENCE (W1): a turn whose canonical signature exactly repeats the previous turn
        // (A,A) or completes a 2-cycle (A,B,A,B) is a SUSPECTED loop. On the FIRST flagged
        // occurrence we nudge but still EXECUTE the call, so its result novelty is judged by the
        // progress block below: a repeat that yields NEW content is productive and clears the latch
        // (a legit poll/consume loop runs free — the false-positive fix), while a redundant repeat
        // yields nothing new, keeps the latch, and the NEXT recurrence hard-stops WITHOUT executing.
        // nudged_sigs is also cleared by any productive turn (W6), so a repeat after real progress
        // earns a fresh nudge rather than an instant stop.
        let sig = turn_signature(&turn.tool_calls);
        let repeated = recent_sigs.back() == Some(&sig) || is_two_cycle(&recent_sigs, &sig);
        recent_sigs.push_back(sig.clone());
        if recent_sigs.len() > SIG_RING {
            recent_sigs.pop_front();
        }
        if repeated {
            if !nudged_sigs.insert(sig) {
                // Already flagged this episode AND recurred with no productive turn clearing the
                // latch → a genuine loop. Stop; the repeat is NOT executed/appended (return before
                // pre-fill, so no dangling tool_calls can strand history).
                return Ok(AgentOutcome {
                    final_text: turn.content,
                    iters: iter + 1,
                    stop: StopReason::Divergence,
                });
            }
            // First flag for this signature: nudge, then fall through to execute so the progress
            // block can judge whether the repeat actually produced new information.
            push_nudge(
                messages,
                NUDGE_DIVERGENCE,
                "You repeated the same tool call(s). If this is not producing NEW information, take a DIFFERENT approach or stop and explain what is blocking you.",
            );
        }

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
            messages.push(Message::tool_result(
                tc.id.clone(),
                INTERRUPTED_TOOL_PLACEHOLDER.to_string(),
            ));
        }

        // EXECUTE the call(s): barrier-partitioned — consecutive read-only calls run concurrently
        // (spawn_blocking, raced against Esc); each write/shell call is a barrier executed alone
        // with approval on THIS future. Eager starts from the streaming path are adopted by
        // position. Results land in ORIGINAL call order. (A DISCARDED turn — divergence/error —
        // simply drops its eager handles: detached, read-only, harmless.)
        let eager = std::mem::take(&mut turn.eager);
        crate::core::recovery::set_phase(crate::core::recovery::RecoveryPhase::ExecutingTools);
        let results = execute_calls(
            registry,
            &calls,
            cfg,
            &mut messages[base..],
            eager,
            &mut auto_checkpointed,
            &mut writer_lease,
        )
        .await;
        crate::core::recovery::set_phase(crate::core::recovery::RecoveryPhase::WaitingModel);

        // Arm the verify gate only if a destructive tool actually SUCCEEDED this turn — a
        // denied/errored edit changed nothing, so it must not make the gate blame the tree.
        let edited_this_turn = turn_made_edits(registry, &calls, &results);
        if edited_this_turn {
            made_any_edits = true;
            // Fresh work invalidates any prior clean check — the gate must re-verify (W8).
            verify_passed = false;
            // PER-STEP CHECKPOINT (Cline-style): after each turn whose edits SUCCEEDED, stamp a
            // restore point so every editing step is independently rewindable — not just the whole
            // run from the single pre-edit snapshot. Best-effort; `save` dedups a zero-diff tree, so
            // a turn that only ran (say) a shell build with no file change costs nothing. Runs AFTER
            // the pre-fill/execute so the snapshot captures the POST-edit tree.
            if cfg.checkpoint_each_edit {
                match crate::features::timemachine::save("after agent edit", true) {
                    Ok(snap) => {
                        crate::features::timemachine::note_last_good(snap.id);
                        if !cfg.quiet {
                            emit_trace(&format!(
                                "  └ checkpoint #{} (agent: `checkpoint_rewind` target=last_good; human: `aizen time restore {}`)",
                                snap.id, snap.id
                            ));
                        }
                    }
                    // No work tree at all is not a failure — it mirrors the pre-edit path, which
                    // reports "checkpoint unavailable: not a git repository" and moves on. Only a
                    // REAL failure (dubious ownership, a corrupt store, a locked ref) is worth a
                    // cry-wolf warning, and it must carry git's own cause (`{e:#}` = full chain),
                    // not the swallowed top-level context.
                    Err(e) if e.to_string().contains("not a git repository") => {
                        if !cfg.quiet {
                            emit_trace("  └ checkpoint unavailable: not a git repository");
                        }
                    }
                    Err(e) if crate::core::gitx::is_git_missing(&e) => {
                        if !cfg.quiet {
                            emit_trace("  └ checkpoint unavailable: git executable not found (edits proceed without checkpoints)");
                        }
                    }
                    Err(e) => {
                        emit_trace(&format!(
                            "  └ warning: post-edit checkpoint failed; the latest change may not be independently rewindable: {e:#}"
                        ));
                    }
                }
            }
        }

        // PROGRESS / THRASH GUARD (W3/W4): a turn is PRODUCTIVE iff a successful edit landed OR some
        // NON-failure result carried content not seen before this run. Failures are never progress (a
        // fresh error string can't rescue a flail — W3); a throwaway re-read of already-seen bytes is
        // not novel (padding can't reset the streak — W3), and a same-content re-read loop climbs to a
        // stop (W4). A re-read that surfaces NEW bytes, or is followed by a successful edit, IS
        // productive and resets the streak — the system-prompt-sanctioned re-read→retry recovery is
        // never punished. Productive turns also clear nudged_sigs, ending any open divergence episode.
        let mut new_content = false;
        for (tc, (_, r)) in calls.iter().zip(&results) {
            // Registry-aware: a tool that self-declares failure (W12) is never progress, even if it
            // returned Ok(...) without the `error:`/`exit N` shape the heuristic keys on.
            if result_is_failure(registry, &tc.function.name, r) {
                continue; // failures are never progress
            }
            if seen_results.insert(hash_str(r)) {
                new_content = true;
            }
        }
        let productive = edited_this_turn || new_content;
        if productive {
            unproductive_streak = 0;
            stuck_nudged = false;
        } else if !calls.is_empty() {
            unproductive_streak += 1;
        }
        // Only GENUINELY NOVEL output ends a divergence episode. A successful destructive call sets
        // edited_this_turn=true on EVERY turn (turn_made_edits is true for any non-error destructive
        // result), so clearing the latch on `productive` would let a repeated IDENTICAL destructive
        // call (same signature, same body — e.g. `git commit --allow-empty`, `>> log`, a no-op
        // file_write) re-flag as "first seen" forever and never hard-stop. Gating the clear on
        // new_content keeps a legit poll/consume loop free (each call is novel) while a redundant
        // repeated edit keeps its latch → the next recurrence hits the insert()==false stop.
        if new_content {
            nudged_sigs.clear();
        }
        if unproductive_streak >= STUCK_STOP_STREAK {
            return Ok(AgentOutcome {
                final_text: turn.content,
                iters: iter + 1,
                stop: StopReason::Divergence,
            });
        }
        if unproductive_streak >= STUCK_NUDGE_STREAK && !stuck_nudged {
            stuck_nudged = true;
            push_nudge(
                messages,
                NUDGE_STUCK,
                "Several turns in a row made no progress (failing, or repeating calls that return \
                 nothing new). STOP retrying variations. Re-read the file to copy exact text, use \
                 file_write to create or fully overwrite a file (never blank a file with shell), \
                 verify the real state, or explain what is blocking you.",
            );
        }

        // P0.2 / P0.3: after tools run, sample process-global todos for confidence spikes and
        // low hill_climbable self-scores. Only the top-level list is visible here (ScopedTodo is
        // private to sub-agents — their poke/gates stay off via config).
        if calls.iter().any(|c| c.function.name == "todo_write") {
            update_confidence_tracking(
                &todo::snapshot(),
                &mut conf_last,
                &mut confidence_gate_armed,
                cfg.conf_high,
                cfg.conf_spike_delta,
            );
            if cfg.enable_hill_climb {
                if todo::snapshot().iter().any(|t| {
                    t.status != todo::Status::Done
                        && t.hill_climbable.is_some_and(|h| h < cfg.hill_climb_gate)
                }) {
                    hill_climb_on = true;
                }
            }
        }

        // P0.3 hill-climb: one-shot reframe + optional cadence remeasure (system nudges).
        if hill_climb_on {
            if !hill_climb_reframed {
                hill_climb_reframed = true;
                last_hill_climb_reminder = iter;
                push_nudge(
                    messages,
                    NUDGE_HILL_CLIMB,
                    "[hill-climb] This goal looks quantifiable. Before more edits, state:\n\
                     1) metric (e.g. ns/op, pass count, binary KB),\n\
                     2) baseline measurement command,\n\
                     3) target direction (higher/lower).\n\
                     Then iterate: measure → change → measure. Stop when plateau or budget.",
                );
            } else if cfg.hill_climb_reminder_every > 0
                && iter.saturating_sub(last_hill_climb_reminder) >= cfg.hill_climb_reminder_every
                && todo::has_incomplete()
            {
                last_hill_climb_reminder = iter;
                push_nudge(
                    messages,
                    NUDGE_HILL_CLIMB,
                    "[hill-climb] Re-measure the metric before claiming progress. No metric delta → \
                     try a different approach or stop.",
                );
            }
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

        // The loop-boundary check above owns extension decisions. Keeping it out of the tool path
        // ensures Done-gate continues cannot bypass the policy or fall through to synthesis early.
    }

    // MAXITERS (W9): don't abandon the task with no answer. Spend ONE tool-free call for a best-
    // effort final answer, built on a THROWAWAY clone so the real `messages` is never mutated before
    // the call (no rollback needed, TAIL invariant intact); empty tool defs push a prose answer.
    // Degrades to None on any chat error — never worse than the old behavior.
    let final_text = {
        let mut synth = messages.clone();
        synth.push(Message::user(
            "You have reached the step limit and cannot call any more tools. Summarize what you \
             accomplished and give your best final answer now from what you already have; \
             explicitly flag anything you could not verify or finish.",
        ));
        match chat(synth, Vec::new()).await {
            Ok(t) => t.content.filter(|s| !s.trim().is_empty()),
            Err(_) => None,
        }
    };
    if let Some(ref s) = final_text {
        messages.push(Message::assistant(s.clone()));
    }
    Ok(AgentOutcome {
        final_text,
        iters: iter,
        stop: StopReason::MaxIters,
    })
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
    auto_checkpointed: &mut bool,
    writer_lease: &mut Option<crate::core::workspace_txn::WorkspaceWriterLease>,
) -> Vec<(String, String)> {
    debug_assert_eq!(
        sink.len(),
        calls.len(),
        "one pre-filled placeholder per call"
    );
    // Eager starts from the streaming path, keyed by position — adopted instead of re-spawned.
    // Their bodies ran quiet; the executor emits the trace at adoption and the result marker at
    // landing so the UX is indistinguishable from a normal run.
    let mut adopted: std::collections::HashSet<usize> = std::collections::HashSet::new();
    // The tool-call line `seq` opened at adoption, so the result can update THAT line in place.
    let mut adopted_seq: std::collections::HashMap<usize, u64> = std::collections::HashMap::new();
    let mut eager: std::collections::HashMap<usize, tokio::task::JoinHandle<String>> =
        eager.into_iter().collect();
    // Parse every call's arguments ONCE — used for the safety partition, the gate, and the body.
    let parsed: Vec<Result<serde_json::Value, String>> = calls
        .iter()
        .map(|tc| parse_call_args(&tc.function.arguments))
        .collect();
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
        if cancelled || cfg.cancel.is_cancelled() {
            cancelled = true;
            land(
                i,
                "error: cancelled by user".to_string(),
                &mut results,
                sink,
            );
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
                                if let (Some(tool), Ok(args)) =
                                    (registry.get(&calls[k].function.name), &parsed[k])
                                {
                                    adopted_seq.insert(k, emit_tool_call(tool.name(), args));
                                }
                            }
                            return (k, h);
                        }
                        let tool = registry
                            .get_arc(&calls[k].function.name)
                            .expect("safe ⇒ known");
                        let args = parsed[k].clone().expect("safe ⇒ parsed");
                        let quiet = cfg.quiet;
                        let max = cfg.max_tool_result_chars;
                        let max_fetch = cfg.max_fetch_result_chars;
                        let cancel = cfg.cancel.clone();
                        let exec_ctx = cfg.exec_ctx.clone();
                        (
                            k,
                            tokio::task::spawn_blocking(move || {
                                crate::core::cancel::with_current(cancel, || {
                                    crate::core::exec_ctx::with_current(exec_ctx, || {
                                        run_tool_body(tool, &args, quiet, max, max_fetch)
                                    })
                                })
                            }),
                        )
                    })
                    .collect();
                for (k, h) in handles {
                    let out = tokio::select! {
                        r = h => r.unwrap_or_else(|_| "error: tool thread panicked".to_string()),
                        _ = cfg.cancel.cancelled() => {
                            // The blocking body keeps running detached; safe calls are read-only,
                            // so discarding the result is harmless.
                            "error: cancelled by user".to_string()
                        }
                    };
                    if adopted.contains(&k) && !cfg.quiet {
                        // Eager body ran quiet; close the line opened at adoption (matched by seq).
                        let seq = adopted_seq.get(&k).copied().unwrap_or(0);
                        if let Ok(args) = &parsed[k] {
                            // Eager-adopted parallel body: no per-call wall-clock to attribute here.
                            emit_tool_result(seq, &calls[k].function.name, args, &out, None);
                        }
                    }
                    land(k, out, &mut results, sink);
                }
                if cfg.cancel.is_cancelled() {
                    cancelled = true;
                    break 'windows;
                }
            }
            // Fill any slots of the run skipped by a mid-run cancel.
            for k in i..j {
                if results[k].is_none() {
                    land(
                        k,
                        "error: cancelled by user".to_string(),
                        &mut results,
                        sink,
                    );
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
                            let effect = tool.workspace_effect(args);
                            let lease_error = if writer_lease.is_none()
                                && (matches!(
                                    effect,
                                    crate::agent::tools::WorkspaceEffect::Paths
                                        | crate::agent::tools::WorkspaceEffect::OpaqueWorkspace
                                ) || tool.name() == "checkpoint_rewind")
                            {
                                let cwd = std::env::current_dir()
                                    .and_then(|p| p.canonicalize())
                                    .unwrap_or_else(|_| std::path::PathBuf::from("."));
                                // 15s, not 5: transient contention (another aizen instance, an
                                // autosave, the parallel test suite) must WAIT, not fail the edit
                                // with a lease error the user can't act on. Esc still interrupts —
                                // the cancel token is threaded into the wait loop.
                                match crate::core::workspace_txn::WorkspaceWriterLease::acquire(
                                    &cwd,
                                    std::time::Duration::from_secs(15),
                                    Some(&cfg.cancel),
                                    tool.name(),
                                ) {
                                    Ok(lease) => {
                                        *writer_lease = Some(lease);
                                        None
                                    }
                                    Err(e) => Some(format!(
                                        "error: workspace writer lease was not acquired: {e}"
                                    )),
                                }
                            } else {
                                None
                            };
                            // `checkpoint_rewind` IS the recovery path — never nest a pre-edit
                            // snapshot of the broken tree before undoing it.
                            let skip_pre_checkpoint = tool.name() == "checkpoint_rewind";
                            let checkpoint_error = if let Some(error) = lease_error {
                                Some(error)
                            } else if skip_pre_checkpoint {
                                None
                            } else if matches!(
                                effect,
                                crate::agent::tools::WorkspaceEffect::External
                            ) {
                                Some(
                                    "error: protected change targets a path outside the current repository; Time Machine cannot guarantee rewind coverage. Narrow the path or run it manually with an external backup."
                                        .to_string(),
                                )
                            } else if cfg.auto_checkpoint
                                && !*auto_checkpointed
                                && effect.needs_checkpoint()
                            {
                                match crate::features::timemachine::save_protected_change("before agent edits") {
                                    Ok(None) => {
                                        // Two benign shapes, two honest messages: "not a repo" and
                                        // "no git executable" behave the same (checkpoints off, the
                                        // edit proceeds) but must never be conflated in what the
                                        // user reads — the latter is fixable by installing git.
                                        if !cfg.quiet {
                                            if crate::core::gitx::git_exe().is_none() {
                                                emit_trace("→ checkpoint off: git executable not found — edits proceed without checkpoints");
                                            } else {
                                                emit_trace("→ checkpoint unavailable: not a git repository");
                                            }
                                        }
                                        None
                                    }
                                    Ok(Some(snap)) => {
                                        *auto_checkpointed = true;
                                        crate::features::timemachine::note_pre_edit(snap.id);
                                        crate::core::recovery::set_checkpoint(Some(snap.id));
                                        if !cfg.quiet {
                                            emit_trace(&format!(
                                                "→ checkpoint #{} saved (agent: `checkpoint_rewind` target=pre_edit; human: `aizen time restore {}`)",
                                                snap.id, snap.id
                                            ));
                                        }
                                        None
                                    }
                                    Err(e) => Some(format!(
                                        "error: protected workspace change was not run because the pre-edit checkpoint failed: {e:#}"
                                    )),
                                }
                            } else {
                                None
                            };
                            if let Some(error) = checkpoint_error {
                                error
                            } else {
                                let args = args.clone();
                                let quiet = cfg.quiet;
                                let max = cfg.max_tool_result_chars;
                                let max_fetch = cfg.max_fetch_result_chars;
                                let cancel = cfg.cancel.clone();
                                let exec_ctx = cfg.exec_ctx.clone();
                                tokio::task::spawn_blocking(move || {
                                    crate::core::cancel::with_current(cancel, || {
                                        crate::core::exec_ctx::with_current(exec_ctx, || {
                                            run_tool_body(tool, &args, quiet, max, max_fetch)
                                        })
                                    })
                                })
                                .await
                                .unwrap_or_else(|_| "error: tool thread panicked".to_string())
                            }
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
        .map(|(tc, r)| {
            (
                tc.id.clone(),
                r.unwrap_or_else(|| INTERRUPTED_TOOL_PLACEHOLDER.to_string()),
            )
        })
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
        let Some(t) = registry.get(&tc.function.name) else {
            return false;
        };
        let args =
            parse_call_args(&tc.function.arguments).unwrap_or_else(|_| serde_json::json!({}));
        if !t.workspace_effect(&args).needs_checkpoint() || result.starts_with("error:") {
            return false; // no workspace mutation effect, or a denied/errored op.
        }
        // A write tool that no-op'd (target already held identical content) wrote nothing to disk,
        // so it must not arm the verify gate (an unchanged tree can't have broken) — W16.
        if result.starts_with(crate::agent::builtin::NOOP_WRITE_PREFIX) {
            return false;
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
    let max_fetch_chars = cfg.max_fetch_result_chars;
    let cancel = cfg.cancel.clone();
    move |_slot, tc| {
        use std::sync::atomic::Ordering::Relaxed;
        if barrier_hit.load(Relaxed) {
            return None;
        }
        let ok = parse_call_args(&tc.function.arguments)
            .ok()
            .and_then(|args| {
                let tool = registry.get_arc(&tc.function.name)?;
                (!tool.is_destructive() && tool.is_concurrency_safe_for(&args))
                    .then_some((tool, args))
            });
        let Some((tool, args)) = ok else {
            // First unsafe/unknown/unparseable call = the barrier: nothing after it starts early.
            barrier_hit.store(true, Relaxed);
            return None;
        };
        if started.fetch_add(1, Relaxed) >= MAX_PARALLEL {
            return None; // over the cap: run normally at execution time
        }
        let turn_cancel = cancel.clone();
        Some(tokio::task::spawn_blocking(move || {
            crate::core::cancel::with_current(turn_cancel, || {
                run_tool_body(tool, &args, true, max_chars, max_fetch_chars)
            })
        }))
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
fn gate_and_approve(
    tool: &dyn tools::Tool,
    args: &serde_json::Value,
    cfg: &AgentConfig,
) -> Option<String> {
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
                    style(format!(
                        "{reason} — refused (hard safety floor, not overridable)"
                    ))
                    .dim()
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
            cmd_guard::Verdict::Allow => smart_allow = cfg.approval_mode.approves_readonly_shell(),
            cmd_guard::Verdict::Caution(reason) => {
                // A risky-but-legit git op (force-push, reset --hard, push to main, …). Surface the
                // specific reason, then ALWAYS fall through to the approval prompt — `smart` must not
                // auto-clear it (leave smart_allow false), so this can never be silently auto-run.
                let line = format!(
                    "{} {}",
                    style("⚠ caution").color256(crate::ui::theme::WARN).bold(),
                    style(&reason).dim()
                );
                if crate::ui::tui::active() {
                    crate::ui::tui::emit_line(&line);
                } else if !cfg.quiet {
                    eprintln!("{line}");
                }
            }
            cmd_guard::Verdict::Ask => {}
        }
    }

    if tool.is_destructive()
        && !cfg.approval_mode.approves_all()
        && !smart_allow
        && !approve(tool.name(), args)
    {
        return Some("error: the user declined this action".to_string());
    }
    None
}

/// The body of one tool call: trace → execute → result marker → truncate. Never panics upward
/// (failures become feedback strings). This is the `spawn_blocking` payload — the existing tool
/// bridges (`block_in_place` + `Handle::block_on`) work unchanged on blocking threads (pinned by
/// `tools::tests::bridge_works_inside_spawn_blocking`).
fn run_tool_body(
    tool: std::sync::Arc<dyn tools::Tool>,
    args: &serde_json::Value,
    quiet: bool,
    max_chars: usize,
    max_fetch_chars: usize,
) -> String {
    if tool.recovery_effect(args) {
        crate::core::recovery::mark_side_effects_possible();
    }
    // Open the tool-call line (mockup shape: `⚙ <name>   <target>`, digest filled in on completion).
    // The `seq` ties the result back to this same line so retained updates it in place.
    let seq = if !quiet {
        emit_tool_call(tool.name(), args)
    } else {
        0
    };
    let started = std::time::Instant::now();
    let out = match tool.execute(args) {
        Ok(out) => out,
        Err(e) => format!("error: {e}"),
    };
    let elapsed_ms = started.elapsed().as_millis() as u64;
    if !quiet {
        emit_tool_result(seq, tool.name(), args, &out, Some(elapsed_ms));
    }
    // Relevance-aware truncation for the READ/FETCH tools whose output is a large document the model
    // is scanning for specifics (W11/W22): keep the region matching the call's query keywords rather
    // than a blind head+tail, and give it the LARGER `max_fetch_chars` budget so the reach layer's
    // 20k fetch isn't halved to 4k before the relevant window is even chosen (the W22 double-cut).
    // Non-failure only (an error string must survive verbatim — the model's error trail is how it
    // recovers) and only for these tools (an edit diff / shell log is positional, not keyword-scored).
    // Everything else keeps the exact old head+tail behavior at the standard budget.
    if !is_failure_result(&out) && is_relevance_truncatable(tool.name()) {
        let keywords = relevance_keywords(&relevance_query_from_args(args));
        return truncate_relevant(&out, max_fetch_chars.max(max_chars), &keywords);
    }
    truncate_result(&out, max_chars)
}

/// Tools whose large output is a document scanned for specifics — relevance-trimming keeps the
/// matching region instead of a blind head+tail. Edit/shell/memory tools are excluded (their
/// output is positional or already digested).
fn is_relevance_truncatable(name: &str) -> bool {
    matches!(
        name,
        "file_read" | "web_fetch" | "web_crawl" | "search_files"
    )
}

/// Pull the relevance-signal string from a call's args: the query/pattern/topic fields these tools
/// take. `web_fetch`/`web_crawl` carry a URL (its path segments are decent keywords); `search_files`
/// a `query`/`pattern`; `file_read` a `path`. Joined so [`relevance_keywords`] can tokenize once.
fn relevance_query_from_args(args: &serde_json::Value) -> String {
    const KEYS: &[&str] = &["query", "pattern", "q", "search", "topic", "url", "path"];
    let mut parts = Vec::new();
    for k in KEYS {
        if let Some(v) = args.get(*k).and_then(|v| v.as_str()) {
            parts.push(v.to_string());
        }
    }
    parts.join(" ")
}

/// The event-anchor line for a tool call. When the tool maps to a human action ([`tool_action`]),
/// it reads `◆ <verb + target> (tool_name)` — the verb+target in moonlight, the raw tool name
/// parenthesised + dimmed, so the user sees *what* is happening at a glance and the exact tool only
/// as a quiet footnote. Tools with no mapping fall back to the older `◆ name(salient-arg)` shape.
/// The `◆` anchor is moonlight-silver here (the call is starting); the result corner `└` on the
/// A one-shot styled string form of the call line (mockup shape `⚙ <name>   <target>`) — the raw
/// tool name in moonlight, its salient target in dim silver. Used for the APPROVAL PROMPT (a single
/// inline line), where there's no in-place digest to fill later. The live transcript uses the
/// structured [`emit_tool_call`]/[`emit_tool_result`] pair instead, which right-aligns the digest.
fn tool_call_line(name: &str, args: &serde_json::Value) -> String {
    let icon = tool_icon();
    let target = tool_target(name, args);
    if target.is_empty() {
        format!(
            "{} {}",
            crate::ui::theme::accent(icon),
            crate::ui::theme::accent(name)
        )
    } else {
        format!(
            "{} {}   {}",
            crate::ui::theme::accent(icon),
            crate::ui::theme::accent(name),
            crate::ui::theme::accent_dim(target)
        )
    }
}

/// Re-print a restored conversation into the scrolling transcript. `/sessions` restore only
/// rehydrates `history` (so the model regains context) — the SCREEN stayed blank, which read as
/// "nothing loaded". This replays each turn with the same surfaces a live turn uses: `❯ user`
/// echoes, markdown-rendered assistant text, and `◆ tool` call lines + `└ result` digests. The
/// system prompt at `[0]` is skipped (it's plumbing, not conversation). Tool-result messages carry
/// only a `tool_call_id`, so we first index `id → tool name` from the assistant tool-calls to render
/// each result under its originating tool.
pub fn replay_transcript(msgs: &[crate::core::types::Message]) {
    use std::collections::HashMap;
    let decorate = crate::ui::tui::active()
        || crate::ui::tui::retained_running()
        || std::io::IsTerminal::is_terminal(&std::io::stdout());
    let cols = crate::ui::tui::width();
    let mut call_names: HashMap<String, String> = HashMap::new();
    for m in msgs {
        for c in &m.tool_calls {
            call_names.insert(c.id.clone(), c.function.name.clone());
        }
    }
    // A restored call line and its result must share a `seq` so the digest lands on the same line
    // under retained (matched by seq). Keyed by tool_call_id, populated as each call line replays.
    let mut call_seq: HashMap<String, u64> = HashMap::new();
    for m in msgs {
        match m.role.as_str() {
            "user" => {
                let body = m.content.as_deref().unwrap_or("").trim();
                if body.is_empty() && m.images.is_empty() {
                    continue;
                }
                let echo = if body.is_empty() { "(image)" } else { body };
                // Tint the whole echo line (not just the `❯`) so a restored user turn reads as
                // clearly the user's voice — matching the live turn's accent-bold echo.
                emit_trace(&format!(
                    "{} {}",
                    crate::ui::theme::accent("❯"),
                    crate::ui::theme::accent(echo)
                ));
            }
            "assistant" => {
                let body = m.content.as_deref().unwrap_or("").trim();
                if !body.is_empty() {
                    let mut md = crate::ui::markdown::MarkdownStream::new(decorate, cols);
                    let mut rendered = md.push(&format!("{body}\n"));
                    rendered.push_str(&md.finish());
                    crate::ui::tui::emit(&rendered);
                }
                for c in &m.tool_calls {
                    let args: serde_json::Value = serde_json::from_str(&c.function.arguments)
                        .unwrap_or(serde_json::json!({}));
                    call_seq.insert(c.id.clone(), emit_tool_call(&c.function.name, &args));
                }
            }
            "tool" => {
                let id = m.tool_call_id.as_deref().unwrap_or("");
                let name = call_names.get(id).map(String::as_str).unwrap_or("tool");
                // Re-parse the originating call's args for the target label; fall back to empty.
                let args = msgs
                    .iter()
                    .flat_map(|mm| &mm.tool_calls)
                    .find(|c| c.id == id)
                    .and_then(|c| serde_json::from_str(&c.function.arguments).ok())
                    .unwrap_or_else(|| serde_json::json!({}));
                let seq = call_seq.get(id).copied().unwrap_or(0);
                emit_tool_result(seq, name, &args, m.content.as_deref().unwrap_or(""), None);
            }
            _ => {} // system prompt + any other roles: not part of the visible conversation
        }
    }
}

/// Emit a trace line into the scroll region (sticky TUI) or stderr (plain / one-shot path).
fn emit_trace(line: &str) {
    // `retained_running()` (not just `active()`) so replay during a SUSPENDED dialoguer menu — e.g.
    // restoring via `/sessions` — still routes into the render thread's buffer, which `resume`
    // redraws from. Otherwise the trace would `eprintln!` onto the menu screen and be wiped.
    if crate::ui::tui::active() || crate::ui::tui::retained_running() {
        crate::ui::tui::emit_line(line);
    } else {
        eprintln!("{line}");
    }
}

/// [`emit_trace`] for tool bodies outside this module.
///
/// A long-running tool is the one place a *tool* legitimately needs the trace lane: `shell_run`'s
/// slow-command note has to reach the same surface as the loop's own progress lines, or it would
/// `println!` into a raw-mode TUI and be wiped by the next repaint.
pub(crate) fn emit_trace_public(line: &str) {
    emit_trace(line);
}

/// The tool-call anchor icon — the moonlight cog `⚙` (matching the mockup), or empty when icons are
/// off. Kept plain (no SGR) so the retained/classic renderer tints it as part of the line.
fn tool_icon() -> &'static str {
    if crate::ui::icons::on() {
        "⚙"
    } else {
        "◆"
    }
}

/// The compact TARGET shown after the raw tool name — a basename / host / clipped query, reusing the
/// same salient-field extraction as [`tool_trace`] but WITHOUT a verb (the mockup shows the raw tool
/// name + its target, e.g. `file_read   src/auth.rs`). Empty when there's nothing salient.
fn tool_target(name: &str, args: &serde_json::Value) -> String {
    let field = |k: &str| args.get(k).and_then(|v| v.as_str());
    let base = |p: &str| basename(p).to_string();
    match name {
        "shell_run" | "bash" | "powershell" | "shell" => {
            shell_target(field("command").or_else(|| field("cmd")).unwrap_or(""))
        }
        "file_write" | "write_file" | "file_edit" | "edit_file" | "apply_patch" | "multi_edit"
        | "symbol_replace" | "symbol_insert" => base(
            field("path")
                .or_else(|| field("file"))
                .or_else(|| field("symbol"))
                .unwrap_or(""),
        ),
        "file_read" | "read_file" => base(field("path").or_else(|| field("file")).unwrap_or("")),
        "file_move" | "move_file" | "rename_file" | "file_rename" => {
            base(field("from").unwrap_or(""))
        }
        "file_glob" => {
            first_line_clip(field("pattern").or_else(|| field("glob")).unwrap_or(""), 40)
        }
        "search_files" => first_line_clip(
            field("query").or_else(|| field("pattern")).unwrap_or(""),
            40,
        ),
        "web_fetch" | "web_crawl" => url_host(field("url").unwrap_or("")),
        "find_symbols" | "lsp_query" => {
            first_line_clip(field("query").or_else(|| field("name")).unwrap_or(""), 40)
        }
        "memory_search" => first_line_clip(field("query").or_else(|| field("q")).unwrap_or(""), 40),
        "skill_load" => field("name").unwrap_or("").to_string(),
        "todo_write" => String::new(),
        "clarify" | "memory_ask" | "telegram_ask" => {
            first_line_clip(field("question").unwrap_or(""), 40)
        }
        n if n.ends_with("_search") || n == "search" => {
            first_line_clip(field("query").or_else(|| field("q")).unwrap_or(""), 40)
        }
        _ => {
            // Unknown tool: fall back to the salient-arg trace so nothing renders bare.
            let t = tool_trace(name, args);
            if t == compact_args(args) && args.as_object().map(|o| o.is_empty()).unwrap_or(true) {
                String::new()
            } else {
                first_line_clip(&t, 40)
            }
        }
    }
}

/// Open a tool-call line (mockup shape `⚙ <name>   <target>`), returning the `seq` so the result can
/// update the same line in place under retained. Shared by the serial + eager-adoption paths.
fn emit_tool_call(name: &str, args: &serde_json::Value) -> u64 {
    crate::ui::tui::tool_call_begin(tool_icon(), name, &tool_target(name, args))
}

/// Close a tool-call line: compute the result digest (via [`summarize_result`]) and update the line
/// opened by [`emit_tool_call`] in place (retained) or render it once (classic). Edits then push a
/// boxed diff; a passing verify-context command gets no special box here (the verify gate emits its
/// own green line). `seq` matches the [`emit_tool_call`] that opened this line.
fn emit_tool_result(
    seq: u64,
    name: &str,
    args: &serde_json::Value,
    out: &str,
    elapsed_ms: Option<u64>,
) {
    let (ok, summary) = summarize_result(name, out);
    // Point the idle screensaver's context card at the feature this tool illustrates (a sub-agent
    // spawn → "Delegate", a web_search → "Researches the web", …). Only on success — a failed call
    // didn't really exercise the feature. A no-op for tools with no card.
    if ok {
        crate::ui::cards::note_tool_activity(name);
    }
    crate::ui::tui::tool_call_end(
        seq,
        tool_icon(),
        name,
        &tool_target(name, args),
        &summary,
        Some(ok),
        elapsed_ms,
    );
    if ok && !out.trim_start().starts_with("error:") && is_edit_tool(name) {
        emit_edit_diff(&tool_target(name, args), out);
    }
}

fn is_edit_tool(name: &str) -> bool {
    matches!(
        name,
        "file_edit"
            | "multi_edit"
            | "edit_file"
            | "apply_patch"
            | "file_write"
            | "write_file"
            | "symbol_replace"
            | "symbol_insert"
    )
}

/// Count added / removed lines in a unified-diff-bearing result: lines beginning `+` / `-` at
/// column 0. The `…(N more lines …)` cap notes begin with `…` and context lines with a space, so neither
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

/// `edited src/foo.rs (2 replacement(s))` → `edited src/foo.rs`; `created src/foo.rs` → `created src/foo.rs`.
fn edit_target(head: &str) -> String {
    let mut it = head.split_whitespace();
    match (it.next(), it.next()) {
        (Some("created"), Some(path)) => format!("created {path}"),
        (Some(_), Some(path)) => format!("edited {path}"),
        _ => "edited".to_string(),
    }
}

/// Emit a boxed diff preview for an edit result: header `diff · <path>  +A −D`, then up to a few of
/// the `^[-+]`-prefixed lines the unified `diff_preview` emitted (added = green `+`, removed = salmon
/// `−`); context / cap-note lines (space- / `…`-prefixed) are skipped. `path` is the edited target
/// (already a basename-ish target string). The full diff still reaches the model; only this preview
/// hits the screen. Under retained this is a framed box; classic re-renders the same box inline.
fn emit_edit_diff(path: &str, out: &str) {
    const MAX_SHOWN: usize = 8;
    let (adds, dels) = count_diff(out);
    let mut lines: Vec<(bool, String)> = Vec::new();
    for l in out.lines() {
        let (is_add, content) = match l.as_bytes().first() {
            Some(b'+') => (true, &l[1..]),
            Some(b'-') => (false, &l[1..]),
            _ => continue,
        };
        if lines.len() == MAX_SHOWN {
            break;
        }
        lines.push((is_add, content.trim_end().to_string()));
    }
    if lines.is_empty() {
        return;
    }
    crate::ui::tui::diff_box(path, adds, dels, lines);
}

/// Build the `⎿` summary for a tool result, returning `(ok, text)` (`ok=false` → coloured as a
/// failure). Parses the tool's OWN returned string — no `Tool` trait change — minting a concise
/// label for the high-traffic tools and reusing the tool's own one-line header where it
/// already reads well (LSP `N reference(s)`, `search_files` `N match(es) in M file(s)`, `web_crawl`,
/// `todo_write`).
fn summarize_result(name: &str, out: &str) -> (bool, String) {
    let trimmed = out.trim_start();
    if let Some(reason) = trimmed.strip_prefix("error:") {
        return (
            false,
            format!("error: {}", first_line_clip(reason.trim(), 60)),
        );
    }
    let first = out.lines().next().unwrap_or("");
    match name {
        "shell_run" | "bash" | "powershell" | "shell" => {
            let code = trimmed.strip_prefix("exit ").and_then(|rest| {
                let tok: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_digit() || *c == '-')
                    .collect();
                tok.parse::<i32>().ok()
            });
            match code {
                Some(0) => (true, "exit 0".to_string()),
                Some(n) => (false, format!("exit {n}")),
                None => (true, "done".to_string()),
            }
        }
        "file_read" | "read_file" => (true, format!("read {} lines", out.lines().count())),
        "file_glob" => {
            if trimmed.starts_with("(no files") {
                (true, "0 files".to_string())
            } else {
                (true, format!("{} files", out.lines().count()))
            }
        }
        "file_edit" | "edit_file" | "apply_patch" | "symbol_replace" | "symbol_insert" => {
            if first.starts_with("created")
                || first.starts_with("inserted")
                || first.starts_with("replaced")
            {
                (true, first.chars().take(80).collect())
            } else {
                let (a, d) = count_diff(out);
                (true, format!("{} · +{a} −{d}", edit_target(first)))
            }
        }
        "multi_edit" => {
            let (a, d) = count_diff(out);
            (true, format!("{} · +{a} −{d}", edit_target(first)))
        }
        "file_write" | "write_file" => {
            // out first line = "created <path> (N line(s))" | "overwrote <path> (N line(s))"
            let verb = if first.starts_with("overwrote") {
                "overwrote"
            } else {
                "created"
            };
            let path = first.split_whitespace().nth(1).unwrap_or("");
            (true, format!("{verb} {path}"))
        }
        "file_move" | "move_file" | "rename_file" | "file_rename" => {
            // out first line = "moved <kind> <from> → <to>"; surface it verbatim (it's already a
            // tidy one-liner), sans any trailing detail.
            (true, first.trim_start_matches("moved ").to_string())
        }
        "memory_search" => {
            if trimmed.starts_with("(no memory") {
                (true, "0 memories".to_string())
            } else {
                (
                    true,
                    format!(
                        "{} memories",
                        out.lines().filter(|l| l.starts_with('[')).count()
                    ),
                )
            }
        }
        "web_search" => {
            if trimmed.starts_with("(no results") {
                (true, "0 results".to_string())
            } else {
                let n = out
                    .lines()
                    .filter(|l| l.trim_start().starts_with(|c: char| c.is_ascii_digit()))
                    .count();
                (true, format!("{n} results"))
            }
        }
        "web_fetch" => {
            let kb = out.len() as f64 / 1024.0;
            if kb >= 1.0 {
                (true, format!("fetched {kb:.0} KB"))
            } else {
                (true, format!("{} chars", out.chars().count()))
            }
        }
        // Everything else already returns a good one-line header (`N reference(s)`,
        // `N match(es) in M file(s):`, `crawled N page(s) → M URL(s)`, `todo list updated: …`) —
        // reuse it verbatim (sans a trailing ':').
        _ => {
            let f = out.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            if f.is_empty() {
                (true, "done".to_string())
            } else {
                (true, first_line_clip(f.trim_end_matches(':'), 60))
            }
        }
    }
}

/// Order-insensitive signature of a turn's tool calls, for divergence detection. Arguments are
/// canonicalized (object keys sorted recursively, insignificant whitespace removed) so a
/// reformatted-JSON or key-reordered repeat of the same call collapses to one signature (W2).
/// Pagination fields (e.g. file_read's start/end) are DELIBERATELY kept: a different read window is
/// different work; a re-read returning identical bytes is caught by the content-novelty thrash guard
/// instead (stripping them would flag legit sequential paging as divergence).
fn turn_signature(calls: &[ToolCall]) -> String {
    let mut sigs: Vec<String> = calls
        .iter()
        .map(|c| {
            format!(
                "{}({})",
                c.function.name,
                canonical_args(&c.function.arguments)
            )
        })
        .collect();
    sigs.sort();
    sigs.join("|")
}

/// Whitespace/key-order-independent rendering of a JSON value (keys sorted recursively — does NOT
/// depend on serde_json's `preserve_order` feature).
fn canonical_json(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(m) => {
            let mut e: Vec<(&String, &serde_json::Value)> = m.iter().collect();
            e.sort_by(|a, b| a.0.cmp(b.0));
            let body: Vec<String> = e
                .iter()
                .map(|(k, val)| format!("{k}:{}", canonical_json(val)))
                .collect();
            format!("{{{}}}", body.join(","))
        }
        serde_json::Value::Array(a) => {
            format!(
                "[{}]",
                a.iter().map(canonical_json).collect::<Vec<_>>().join(",")
            )
        }
        other => other.to_string(),
    }
}

/// Canonical, stable rendering of one call's raw argument string. Unparseable args fall back to the
/// old trimmed form so nothing regresses for tools with odd argument encodings.
fn canonical_args(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(v) => canonical_json(&v),
        Err(_) => raw.trim().to_string(),
    }
}

/// Does `s0` (the current turn signature) complete an A,B,A,B 2-cycle against the recent ring
/// (newest at the back)? The two alternating signatures must differ, keeping this disjoint from the
/// immediate-repeat (A,A) case.
fn is_two_cycle(recent: &std::collections::VecDeque<String>, s0: &str) -> bool {
    let n = recent.len();
    if n < 3 {
        return false;
    }
    let (s1, s2, s3) = (&recent[n - 1], &recent[n - 2], &recent[n - 3]);
    s0 == s2.as_str() && s1 == s3 && s0 != s1.as_str()
}

/// Stable in-process hash of a string (std only). Values live only within one run, so
/// DefaultHasher's lack of cross-version stability is irrelevant; storing u64 bounds memory.
fn hash_str(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
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
    let tok = serde_json::to_string(defs)
        .map(|s| s.len() / 4)
        .unwrap_or(0);
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

/// Registry-aware failure check (W12): a tool may DEFINITIVELY classify its own result via
/// [`tools::Tool::result_is_error`] (an MCP/custom tool that returns `Ok(...)` on a logical
/// failure, so the model sees the detail). When the tool declines (`None`) — the common case —
/// fall back to the generic [`is_failure_result`] heuristic. Used by the progress/thrash guard so
/// a self-declared-failed result never counts as "new content" progress.
fn result_is_failure(registry: &ToolRegistry, tool_name: &str, content: &str) -> bool {
    if let Some(t) = registry.get(tool_name) {
        if let Some(verdict) = t.result_is_error(content) {
            return verdict;
        }
    }
    is_failure_result(content)
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
        let code: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '-')
            .collect();
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
    let tool_idxs: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "tool")
        .map(|(i, _)| i)
        .collect();
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
                (
                    b.chars().count(),
                    format!("{first}{FAILED_TOOL_TRIM_SUFFIX}"),
                )
            }
            _ => continue,
        };
        est = est.saturating_sub(estimate_message_tokens(&messages[i]));
        messages[i].content = Some(trimmed);
        est += estimate_message_tokens(&messages[i]);
        stats.chars_reclaimed += len.saturating_sub(
            messages[i]
                .content
                .as_deref()
                .map_or(0, |c| c.chars().count()),
        );
        stats.failures_trimmed += 1;
    }
    stats
}

/// Is a clearing pass due? First crossing always fires; after that, only `step_pct` points of
/// growth past the last fire OR `cooldown_iters` iterations re-arm it — the cadence that keeps
/// mutations infrequent (cache-friendly) instead of a per-turn trickle.
fn clearing_due(
    pct: usize,
    iter: usize,
    last: Option<(usize, usize)>,
    step_pct: u8,
    cooldown_iters: usize,
) -> bool {
    match last {
        None => true,
        Some((p0, i0)) => pct >= p0 + step_pct as usize || iter >= i0 + cooldown_iters,
    }
}

/// P0.2: track per-todo confidence; arm the gate on a Done status with a large upward jump.
fn update_confidence_tracking(
    items: &[todo::Todo],
    conf_last: &mut std::collections::HashMap<String, u8>,
    confidence_gate_armed: &mut bool,
    conf_high: u8,
    conf_spike_delta: u8,
) {
    for t in items {
        let Some(c) = t.confidence else {
            continue;
        };
        let key = t.content.clone();
        if t.status == todo::Status::Done {
            if let Some(&prev) = conf_last.get(&key) {
                if c >= conf_high && c.saturating_sub(prev) >= conf_spike_delta {
                    *confidence_gate_armed = true;
                }
            }
        }
        conf_last.insert(key, c);
    }
}

/// P0.3: user/task text looks like a quantifiable optimization goal.
fn task_looks_hill_climbable(messages: &[Message]) -> bool {
    // Scan recent user messages (skip system). Keywords are word-ish substrings, lowercase.
    const KEYS: &[&str] = &[
        "optimize",
        "optimise",
        "benchmark",
        "perf",
        "latency",
        "throughput",
        "minimize",
        "minimise",
        "maximize",
        "maximise",
        "hill-climb",
        "hill climb",
        "faster",
        "speed up",
        "fewer allocations",
        "reduce memory",
        "smaller binary",
    ];
    for m in messages.iter().rev().take(6) {
        if m.role != "user" {
            continue;
        }
        let Some(text) = m.content.as_deref() else {
            continue;
        };
        let lower = text.to_ascii_lowercase();
        if KEYS.iter().any(|k| lower.contains(k)) {
            return true;
        }
    }
    false
}

// ── self-review (opt-in, one extra turn before Done) ────────────────────────────────────────────

/// Nudge-mode self-review text (no oracle configured): the model re-reads its own diff.
const SELF_REVIEW_NUDGE: &str =
    "[self-review] Before finishing: run `git diff`, re-read the ORIGINAL request, and verify \
     every requirement is met and nothing unrelated changed. Fix or flag anything off, then give \
     your final answer.";

/// The outcome of one oracle self-review pass.
struct ReviewOutcome {
    /// The findings text to inject (already formatted, most-severe first).
    findings: String,
    /// `true` if at least one finding is a VERIFIED correctness problem (a bug or missed
    /// requirement) — those gate Done (the model MUST fix or rebut). `false` when every finding is
    /// ADVISORY (cleanup/style): surfaced once, but Done is NOT held hostage to it.
    blocking: bool,
}

/// Does a review verdict carry at least one VERIFIED correctness problem? Keyed on the `[BLOCKING]`
/// tag (case-insensitive) the oracle is instructed to prefix such lines with. Pure so it's unit-
/// testable without a live model or a git tree.
fn review_is_blocking(review: &str) -> bool {
    review.to_ascii_uppercase().contains("[BLOCKING]")
}

fn clean_review_user_text(content: &str) -> &str {
    let mut current = content;
    loop {
        let next = codebase::strip_retrieval_prefix(crate::memory::strip_recall_prefix(current));
        if next == current {
            return current;
        }
        current = next;
    }
}

/// Capture the newest real user request. A terse clarification answer is joined to the preceding user
/// request when it immediately follows a `clarify` tool result, so "option 2" remains reviewable.
fn capture_review_request(messages: &[Message]) -> String {
    let Some((idx, current)) = messages
        .iter()
        .enumerate()
        .rev()
        .find(|(_, m)| m.role == "user")
        .and_then(|(i, m)| m.content.as_deref().map(|c| (i, c)))
    else {
        return String::new();
    };
    let current = clean_review_user_text(current).trim();
    let follows_clarify =
        idx > 0 && messages[idx - 1].role == "tool" && messages[idx - 1].tool_call_id.is_some();
    if follows_clarify {
        if let Some(previous) = messages[..idx.saturating_sub(1)]
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .and_then(|m| m.content.as_deref())
        {
            return format!(
                "{}\n\nClarification answer:\n{}",
                clean_review_user_text(previous).trim(),
                current
            );
        }
    }
    current.to_string()
}

/// Ask the oracle (a usually-stronger model) to review the working-tree diff against the original
/// request. `None` ⇒ nothing actionable (no git / empty diff / LGTM / call failed) — the loop
/// falls through to Done without burning a turn.
///
/// The oracle is asked to VERIFY each candidate finding against how the code actually behaves and
/// tag it `[BLOCKING]` (a real bug / missed requirement, evidenced) or `[ADVISORY]` (cleanup, style,
/// nice-to-have). This mirrors the SOTA pattern (Claude Code's `/code-review`): the verify step —
/// not the raw candidate list — is what gates Done, so a pile of style nits can't force a fix turn
/// while a genuine bug always does.
async fn oracle_review<O, OFut>(oracle: &O, review_request: &str) -> Option<ReviewOutcome>
where
    O: Fn(Vec<Message>) -> OFut,
    OFut: Future<Output = Result<String>>,
{
    let diff = git_diff_capped()?;
    let task: String = review_request.chars().take(2_000).collect();
    let sys = Message::system(
        "You are a rigorous senior code reviewer. Review the DIFF against the ORIGINAL REQUEST in \
         two steps. STEP 1 — find candidate problems. STEP 2 — VERIFY each against how the code \
         actually behaves and KEEP only the ones you can stand behind. For each surviving finding \
         emit ONE line, most severe first, prefixed with a tag:\n\
         [BLOCKING] a real bug, a broken build, or a missed/incorrect requirement — with file:line evidence.\n\
         [ADVISORY] cleanup, style, or a nice-to-have that does NOT affect correctness.\n\
         If the diff is correct and complete, reply with exactly: LGTM",
    );
    let usr = Message::user(format!("ORIGINAL REQUEST:\n{task}\n\nDIFF:\n{diff}"));
    match oracle(vec![sys, usr]).await {
        Ok(s) => {
            let t = s.trim();
            if t.is_empty() || t.eq_ignore_ascii_case("lgtm") {
                return None;
            }
            // A finding is BLOCKING iff any line is tagged [BLOCKING] (case-insensitive). An
            // all-advisory review is surfaced but must not gate Done.
            Some(ReviewOutcome {
                findings: t.to_string(),
                blocking: review_is_blocking(t),
            })
        }
        Err(_) => None, // best-effort: a failing oracle never blocks Done
    }
}

/// Complete, bounded, redacted working-tree review bundle. Includes staged, unstaged, deleted,
/// renamed, and safe untracked files without mutating the index.
fn git_diff_capped() -> Option<String> {
    const CAP: usize = 12_000;
    let cwd = std::env::current_dir().ok()?;
    let root_text = run_git_bounded(&cwd, &["rev-parse", "--show-toplevel"])?;
    let root = std::path::PathBuf::from(root_text.trim())
        .canonicalize()
        .ok()?;
    let has_head = run_git_bounded(&root, &["rev-parse", "--verify", "HEAD"]).is_some();

    let mut tracked = std::collections::BTreeSet::new();
    if has_head {
        add_nul_paths(
            &mut tracked,
            &run_git_bounded(&root, &["diff", "--name-only", "-z", "HEAD", "--"])?,
        );
    } else {
        if let Some(s) = run_git_bounded(&root, &["diff", "--cached", "--name-only", "-z", "--"]) {
            add_nul_paths(&mut tracked, &s);
        }
        if let Some(s) = run_git_bounded(&root, &["diff", "--name-only", "-z", "--"]) {
            add_nul_paths(&mut tracked, &s);
        }
    }
    let mut untracked = std::collections::BTreeSet::new();
    add_nul_paths(
        &mut untracked,
        &run_git_bounded(&root, &["ls-files", "--others", "--exclude-standard", "-z"])
            .unwrap_or_default(),
    );

    let mut bundle = String::new();
    for rel in tracked {
        if bundle.chars().count() >= CAP {
            break;
        }
        let rel_path = std::path::Path::new(&rel);
        if let Some(kind) = codebase::review_sensitivity(rel_path) {
            append_review_section(
                &mut bundle,
                CAP,
                &format!("\n--- {rel} [{kind}: content omitted] ---\n"),
            );
            continue;
        }
        let patch = if has_head {
            run_git_bounded_args(
                &root,
                &["diff", "--no-ext-diff", "HEAD", "--"],
                Some(rel_path),
            )
            .unwrap_or_default()
        } else {
            let cached = run_git_bounded_args(
                &root,
                &["diff", "--cached", "--no-ext-diff", "--"],
                Some(rel_path),
            )
            .unwrap_or_default();
            let work =
                run_git_bounded_args(&root, &["diff", "--no-ext-diff", "--"], Some(rel_path))
                    .unwrap_or_default();
            format!("{cached}{work}")
        };
        if !patch.trim().is_empty() {
            append_review_section(&mut bundle, CAP, &codebase::redact_for_review(&patch));
        }
    }

    for rel in untracked {
        if bundle.chars().count() >= CAP {
            break;
        }
        let rel_path = std::path::Path::new(&rel);
        if let Some(kind) = codebase::review_sensitivity(rel_path) {
            append_review_section(
                &mut bundle,
                CAP,
                &format!("\n--- {rel} [untracked {kind}: content omitted] ---\n"),
            );
            continue;
        }
        let candidate = root.join(rel_path);
        let Ok(canon) = candidate.canonicalize() else {
            continue;
        };
        if !canon.starts_with(&root) || !canon.is_file() {
            continue;
        }
        let Ok(meta) = std::fs::metadata(&canon) else {
            continue;
        };
        if meta.len() > codebase::review_file_limit() {
            append_review_section(
                &mut bundle,
                CAP,
                &format!("\n--- {rel} [untracked oversized: content omitted] ---\n"),
            );
            continue;
        }
        let Ok(bytes) = std::fs::read(&canon) else {
            continue;
        };
        if bytes.contains(&0) {
            append_review_section(
                &mut bundle,
                CAP,
                &format!("\n--- {rel} [untracked binary: content omitted] ---\n"),
            );
            continue;
        }
        let Ok(text) = std::str::from_utf8(&bytes) else {
            append_review_section(
                &mut bundle,
                CAP,
                &format!("\n--- {rel} [untracked binary: content omitted] ---\n"),
            );
            continue;
        };
        let section = format!(
            "\n--- {rel} [untracked file] ---\n{}\n",
            codebase::redact_for_review(text)
        );
        append_review_section(&mut bundle, CAP, &section);
    }

    let trimmed = bundle.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn add_nul_paths(out: &mut std::collections::BTreeSet<String>, raw: &str) {
    out.extend(
        raw.split('\0')
            .filter(|s| !s.is_empty())
            .map(str::to_string),
    );
}

fn append_review_section(out: &mut String, cap: usize, section: &str) {
    let remaining = cap.saturating_sub(out.chars().count());
    out.extend(section.chars().take(remaining));
}

fn run_git_bounded(root: &std::path::Path, args: &[&str]) -> Option<String> {
    run_git_bounded_args(root, args, None)
}

fn run_git_bounded_args(
    root: &std::path::Path,
    args: &[&str],
    path: Option<&std::path::Path>,
) -> Option<String> {
    let mut cmd = crate::core::gitx::command().ok()?;
    cmd.current_dir(root).args(args);
    if let Some(path) = path {
        cmd.arg(path);
    }
    let out = crate::core::proctree::output_bounded(
        &mut cmd,
        std::time::Duration::from_secs(15),
        std::time::Duration::from_secs(2),
    )
    .ok()?;
    (out.code == Some(0) && !out.timed_out).then_some(out.stdout)
}

// ── mid-loop nudges (collapsed, never accreted) ─────────────────────────────────────────────────

/// Stable identifying prefixes for the loop's system nudges. [`push_nudge`] uses them to REPLACE a
/// stale earlier nudge of the same kind instead of accreting a new copy — across a long REPL
/// session (one loop invocation per user turn) the old append-only behavior grew without bound.
const NUDGE_CONTEXT: &str = "Context is nearly full";
const NUDGE_DIVERGENCE: &str = "You repeated the same tool call(s)";
const NUDGE_STEP_LIMIT: &str = "You are nearing the step limit";
const NUDGE_TODO: &str = "Current task list";
const NUDGE_STUCK: &str = "Several turns in a row made no progress";
/// P0.1 incomplete-todo gate inject (user role — hard block path, not a soft system nudge).
const TODO_POKE_PREFIX: &str = "[todo-poke]";
/// P0.2 confidence-spike gate inject.
const CONFIDENCE_GATE_PREFIX: &str = "[confidence-gate]";
/// P0.3 hill-climb reframe / remeasure (system nudge via push_nudge).
const NUDGE_HILL_CLIMB: &str = "[hill-climb]";
/// GOAL MODE poke (user role — hard block path): the model tried to stop without having declared
/// completion via `goal_complete`, so we re-inject the goal text and keep it working. Mirrors the
/// todo-poke gate's shape (a `user` message the model can't ignore), not a soft system nudge.
const GOAL_POKE_PREFIX: &str = "[goal]";
/// Save-before-clear warning (P-ctx2). Mirrors Claude's server-side "preserve important information"
/// warning: fired ONCE, the turn BEFORE the first tool-result eviction, so the model can persist
/// anything durable (memory files, todo_write) while the old results are still in context.
const NUDGE_SAVE_BEFORE_CLEAR: &str = "Context is filling up";
/// Running context-budget signal (P-ctx1). Like Claude's server-side `<budget>`/`<system_warning>`
/// pair, but client-side and CACHE-AWARE: refreshed only when usage crosses a new band (see
/// `budget_band`), never every turn — every mid-history system-message rewrite busts the provider
/// prompt cache from that byte onward, so a per-turn refresh would trade the very context it reports
/// on for churn. One `system` nudge, collapsed in place by `push_nudge`.
const NUDGE_BUDGET: &str = "Context budget:";

/// Print a short GOAL-MODE retry status line. Deliberately terse and secret-free — it names the
/// failure shape and the wait, never the URL, key, or body (which could leak a token). Routed
/// through the TUI when active so it lands in the retained buffer, else stderr.
fn goal_retry_line(reason: &str, delay_ms: u64) {
    let line = format!("⟳ goal: {reason} — retrying in {delay_ms}ms");
    if crate::ui::tui::active() {
        crate::ui::tui::emit_line(&line);
    } else {
        eprintln!("{line}");
    }
}

/// Sleep `delay_ms`, but wake early (and return `true`) if the turn is cancelled — so Esc during a
/// long goal-mode backoff exits promptly instead of waiting out the full delay. Returns `false` when
/// the sleep completed normally (caller should retry). Mirrors `cancel::race`'s select shape.
async fn goal_sleep_or_cancel(token: &crate::core::cancel::TurnCancel, delay_ms: u64) -> bool {
    tokio::select! {
        _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => false,
        _ = token.cancelled() => true,
    }
}

/// The coarse band a usage fraction falls in, for the running budget signal. Returns `None` below
/// the floor (no point nagging about budget when the window is nearly empty), else a decile
/// `5..=10` (50%, 60%, …, 100%). Refreshing only on a band CHANGE keeps the injected `system`
/// message stable across turns within a band, so the prompt cache survives; the model still gets an
/// escalating signal as pressure climbs. Disabled-window (`window == 0`) is handled by the caller.
fn budget_band(est: usize, window: usize) -> Option<u8> {
    if window == 0 {
        return None;
    }
    let pct = (est.saturating_mul(100) / window).min(100);
    if pct < 50 {
        None
    } else {
        Some((pct / 10) as u8) // 50→5, 63→6, …, 100→10
    }
}

/// The running budget nudge text: `Context budget: ~123.4K/200K tokens used (~76.6K remaining, 38%
/// left). Spend it deliberately — do not re-read files you already have; wrap up before it runs
/// out.` Kept terse; the leading `NUDGE_BUDGET` prefix lets `push_nudge` collapse the prior one.
fn budget_nudge_text(est: usize, window: usize) -> String {
    let remaining = window.saturating_sub(est);
    let pct_left = if window > 0 {
        remaining * 100 / window
    } else {
        0
    };
    format!(
        "{NUDGE_BUDGET} ~{}/{} tokens used (~{} remaining, {}% left). Spend it deliberately — do \
         not re-read files you already have, and wrap up before it runs out.",
        fmt_tok(est as u64),
        fmt_tok(window as u64),
        fmt_tok(remaining as u64),
        pct_left,
    )
}

/// Decide whether a bounded ordinary run earns its one convergence extension.
/// `divergence_open` is passed explicitly so callers cannot accidentally invert the latch.
fn should_auto_extend(
    initial_cap: usize,
    cap: usize,
    auto_extend_to: usize,
    extended: bool,
    unproductive_streak: usize,
    divergence_open: bool,
) -> bool {
    initial_cap > 0
        && cap >= initial_cap
        && !extended
        && auto_extend_to > cap
        && unproductive_streak < STUCK_NUDGE_STREAK
        && !divergence_open
}

/// Consecutive unproductive turns before the thrash guard nudges, then stops (see the loop).
const STUCK_NUDGE_STREAK: usize = 3;
const STUCK_STOP_STREAK: usize = 5;

/// Recent executed-turn signatures retained for divergence detection. The detectors inspect only
/// the last 1 (immediate repeat, A,A) and last 3 (2-cycle, A,B,A,B); a few extra slots are cheap
/// headroom, nothing more. An INTERSPERSED cycle (A,B,X,A,B) is intentionally NOT treated as a loop
/// here — if it makes no progress the thrash guard catches it. O(1) memory, cheap String compares.
const SIG_RING: usize = 6;

/// Append a system nudge, first removing any EARLIER system message of the same kind
/// (`kind_prefix` must prefix `text`). Scans indices 1.. only — the system prompt at `[0]` is
/// untouchable — and removes ONLY `role == "system"` messages, so assistant↔tool pairing cannot be
/// orphaned by construction. The new nudge is always the TAIL message, preserving the caller's
/// error-rollback contract (`messages.pop()` removes exactly the nudge).
fn push_nudge(messages: &mut Vec<Message>, kind_prefix: &str, text: &str) {
    debug_assert!(
        text.starts_with(kind_prefix),
        "kind prefix must identify its own nudge text"
    );
    let mut i = messages.len();
    while i > 1 {
        i -= 1;
        if messages[i].role == "system"
            && messages[i]
                .content
                .as_deref()
                .is_some_and(|c| c.starts_with(kind_prefix))
        {
            messages.remove(i);
        }
    }
    messages.push(Message::system(text));
}

/// Generic tokens a URL/protocol source contributes that carry no topical signal (scheme, common
/// TLDs, markup extensions) — filtered so a `web_fetch(url=...)` query doesn't dilute scoring with
/// words that will spuriously "match" boilerplate (every page has "www"/"html" somewhere).
const KEYWORD_STOPWORDS: &[&str] = &["http", "https", "www", "com", "org", "net", "html", "htm"];

/// Split a query string into lowercased alphanumeric keyword tokens ≥3 chars, deduped, capped —
/// the relevance signal for [`truncate_relevant`]. Short/stop-ish tokens add noise, not signal.
fn relevance_keywords(query: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for tok in query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.chars().count() >= 3)
    {
        let low = tok.to_ascii_lowercase();
        if KEYWORD_STOPWORDS.contains(&low.as_str()) {
            continue;
        }
        if seen.insert(low.clone()) {
            out.push(low);
        }
        if out.len() >= 24 {
            break; // a huge query can't dominate the scan cost
        }
    }
    out
}

/// RELEVANCE-AWARE truncation (W11/W22): keep the `max`-char window of `s` most relevant to
/// `keywords` instead of a blind head+tail. Splits into line-blocks, scores each by keyword hits
/// (BM25-lite: a keyword's contribution saturates so one repeated term can't dominate), then keeps
/// the highest-scoring CONTIGUOUS run that fits `max`, always anchored to include the head (a
/// file's imports / a page's title carry orientation signal). Falls back to [`truncate_result`]
/// when there is no keyword signal or nothing scores — so a keyword-free result degrades exactly
/// to the old behavior. Pure; no allocation beyond the kept window.
pub fn truncate_relevant(s: &str, max: usize, keywords: &[String]) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    if keywords.is_empty() || max < 64 {
        return truncate_result(s, max); // no signal (or too small a budget) → old head+tail
    }
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() < 4 {
        return truncate_result(s, max); // too few lines to window meaningfully
    }
    // Score each line: sum over keywords of a saturating hit count (min(hits,3)) — cheap, and it
    // rewards a line matching MANY distinct keywords over one matching a single term many times.
    let score_line = |line: &str| -> u32 {
        let low = line.to_ascii_lowercase();
        keywords
            .iter()
            .map(|k| (low.matches(k.as_str()).count().min(3)) as u32)
            .sum()
    };
    let scores: Vec<u32> = lines.iter().map(|l| score_line(l)).collect();
    if scores.iter().all(|&x| x == 0) {
        return truncate_result(s, max); // nothing matched → don't distort; keep head+tail
    }
    // Reserve ~1/4 of the budget for an always-included HEAD (orientation), the rest for the best
    // window around the peak-scoring region.
    let head_budget = (max / 4).min(n);
    let head: String = s.chars().take(head_budget).collect();
    let body_budget = max.saturating_sub(head.chars().count() + 48); // 48 ≈ two elision markers

    // Find the contiguous line-run maximizing total score under body_budget chars (greedy window
    // grown around the single best line — O(lines), good enough and stable).
    let peak = scores
        .iter()
        .enumerate()
        .max_by_key(|(_, &sc)| sc)
        .map(|(i, _)| i)
        .unwrap_or(0);
    let (mut lo, mut hi) = (peak, peak);
    let mut win_chars = lines[peak].chars().count();
    // Expand outward toward whichever neighbor has the higher score, staying under budget.
    loop {
        let up = lo.checked_sub(1);
        let down = if hi + 1 < lines.len() {
            Some(hi + 1)
        } else {
            None
        };
        let cand = match (up, down) {
            (Some(u), Some(d)) => {
                if scores[u] >= scores[d] {
                    Some((u, true))
                } else {
                    Some((d, false))
                }
            }
            (Some(u), None) => Some((u, true)),
            (None, Some(d)) => Some((d, false)),
            (None, None) => None,
        };
        let Some((idx, is_up)) = cand else { break };
        let add = lines[idx].chars().count() + 1;
        if win_chars + add > body_budget {
            break;
        }
        win_chars += add;
        if is_up {
            lo = idx
        } else {
            hi = idx
        }
    }
    let window: String = lines[lo..=hi].join("\n");
    let omitted = n.saturating_sub(head.chars().count() + window.chars().count());
    format!(
        "{head}\n…[{omitted} chars elided — kept the region most relevant to the query]…\n{window}"
    )
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
    crate::core::recovery::set_phase(crate::core::recovery::RecoveryPhase::AwaitingApproval);
    struct RestorePhase;
    impl Drop for RestorePhase {
        fn drop(&mut self) {
            crate::core::recovery::set_phase(crate::core::recovery::RecoveryPhase::ExecutingTools);
        }
    }
    let _restore_phase = RestorePhase;
    // Under the sticky TUI the background input thread owns stdin, so we can't run a blocking y/N
    // read inline. Instead, route a per-action prompt THROUGH that thread: `ask_approval` blocks
    // until it presses [y]es / [n]o / [a]llow-all-session. (Destructive tools force the serial path,
    // so we're on a tokio worker where block_in_place is valid — same invariant as the telegram bridge.)
    if crate::ui::tui::active() {
        let prompt = format!(
            "{}  {}",
            tool_call_line(tool, args),
            style("— approve? [y]es · [n]o · [a]llow all this session")
                .color256(crate::ui::theme::WARN)
        );
        return tokio::task::block_in_place(|| crate::ui::tui::ask_approval(&prompt));
    }
    if !std::io::stdin().is_terminal() {
        if crate::hostbot::platforms::telegram::daemon_is_active()
            && crate::hostbot::platforms::telegram::is_configured()
        {
            let prompt = format!("{tool} {}", compact_args(args));
            // Bridge to the async approval on the current (multi-thread) runtime; the serve poll
            // loop runs on another worker and delivers the callback.
            if let Some(v) = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(
                    crate::hostbot::platforms::telegram::request_approval(&prompt),
                )
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

/// Map a tool call to a short, human-readable **action** phrase — an English verb + the salient
/// target — so the event line reads like a narration of intent (`Write foo.rs`, `Run build.ps1`,
/// `Search the web`) instead of a bare tool id. Returns `None` for tools with no natural verb, so
/// [`tool_call_line`] falls back to the raw `name(arg)` shape. Global product → English, always.
fn tool_action(name: &str, args: &serde_json::Value) -> Option<String> {
    let field = |k: &str| args.get(k).and_then(|v| v.as_str());
    let base = |p: &str| basename(p).to_string();
    Some(match name {
        "shell_run" | "bash" | "powershell" | "shell" => {
            let cmd = field("command").or_else(|| field("cmd")).unwrap_or("");
            format!("Run {}", shell_target(cmd))
        }
        "file_write" | "write_file" => format!(
            "Write {}",
            base(field("path").or_else(|| field("file")).unwrap_or(""))
        ),
        "file_edit" | "edit_file" | "apply_patch" | "multi_edit" | "symbol_replace"
        | "symbol_insert" => {
            format!(
                "Edit {}",
                base(
                    field("path")
                        .or_else(|| field("file"))
                        .or_else(|| field("symbol"))
                        .unwrap_or("")
                )
            )
        }
        "file_read" | "read_file" => format!(
            "Read {}",
            base(field("path").or_else(|| field("file")).unwrap_or(""))
        ),
        "file_move" | "move_file" | "rename_file" | "file_rename" => {
            format!("Move {}", base(field("from").unwrap_or("")))
        }
        "file_glob" => format!(
            "Find files {}",
            first_line_clip(field("pattern").or_else(|| field("glob")).unwrap_or(""), 48)
        ),
        "search_files" => format!(
            "Search {}",
            first_line_clip(
                field("query").or_else(|| field("pattern")).unwrap_or(""),
                48
            )
        ),
        "web_fetch" => format!("Fetch {}", url_host(field("url").unwrap_or(""))),
        "web_crawl" => format!("Crawl {}", url_host(field("url").unwrap_or(""))),
        "find_symbols" | "lsp_query" => format!(
            "Look up {}",
            first_line_clip(field("query").or_else(|| field("name")).unwrap_or(""), 48)
        ),
        "memory_search" => format!(
            "Recall {}",
            first_line_clip(field("query").or_else(|| field("q")).unwrap_or(""), 48)
        ),
        "skill_load" => format!("Load skill {}", field("name").unwrap_or("")),
        "todo_write" => {
            let n = args
                .get("todos")
                .and_then(|v| v.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("Plan · {n} step{}", if n == 1 { "" } else { "s" })
        }
        "clarify" | "memory_ask" | "telegram_ask" => format!(
            "Ask · {}",
            first_line_clip(field("question").unwrap_or(""), 48)
        ),
        n if n.ends_with("_search") || n == "search" => {
            format!(
                "Search the web · {}",
                first_line_clip(field("query").or_else(|| field("q")).unwrap_or(""), 44)
            )
        }
        _ => return None,
    })
}

/// The last path segment of `p` (handles both `/` and `\`), so a long absolute path renders as just
/// the file name in the action line. Empty stays empty.
fn basename(p: &str) -> &str {
    p.rsplit(|c| c == '/' || c == '\\').next().unwrap_or(p)
}

/// The host of a URL for a compact fetch/crawl label — `https://docs.rs/x` → `docs.rs`. Falls back
/// to a clipped form of the raw string when there's no recognisable scheme/host.
fn url_host(u: &str) -> String {
    let after = u.split("://").nth(1).unwrap_or(u);
    let host = after.split(['/', '?', '#']).next().unwrap_or(after);
    if host.is_empty() {
        first_line_clip(u, 40)
    } else {
        host.to_string()
    }
}

/// Pull a readable target out of a shell command: prefer an explicit `-File <script>` (PowerShell)
/// or the first token that looks like a script/path (has an extension or a `./`/`.\` prefix),
/// rendered as its basename. Otherwise fall back to the clipped command itself.
fn shell_target(cmd: &str) -> String {
    let toks: Vec<&str> = cmd.split_whitespace().collect();
    // `-File <path>` — the canonical "run this script" form on Windows.
    if let Some(i) = toks.iter().position(|t| t.eq_ignore_ascii_case("-file")) {
        if let Some(p) = toks.get(i + 1) {
            return basename(p.trim_matches('"').trim_matches('\'')).to_string();
        }
    }
    // First token that reads like a script/path: `./x.sh`, `foo.py`, `src\a.ps1`.
    for t in &toks {
        let clean = t.trim_matches('"').trim_matches('\'');
        let looks_pathy = clean.starts_with("./")
            || clean.starts_with(".\\")
            || (clean.contains('.')
                && clean
                    .rsplit('.')
                    .next()
                    .map(|e| e.len() <= 4 && e.chars().all(|c| c.is_ascii_alphanumeric()))
                    .unwrap_or(false));
        if looks_pathy && !clean.starts_with('-') {
            return basename(clean).to_string();
        }
    }
    first_line_clip(cmd, 52)
}

/// A human-readable one-line trace of a tool call for the TUI — the *salient* argument shown
/// unescaped (real newlines collapsed, clipped), instead of raw escaped JSON. Falls back to
/// `compact_args` for tools whose key field we don't recognise.
fn tool_trace(name: &str, args: &serde_json::Value) -> String {
    let field = |k: &str| args.get(k).and_then(|v| v.as_str());
    let salient = match name {
        "shell_run" | "bash" | "powershell" | "shell" => field("command").or_else(|| field("cmd")),
        "file_edit" | "multi_edit" | "edit_file" | "file_write" | "write_file" | "apply_patch"
        | "symbol_replace" | "symbol_insert" => field("path")
            .or_else(|| field("file"))
            .or_else(|| field("symbol")),
        "file_move" | "move_file" | "rename_file" | "file_rename" => field("from"),
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

    // ── display: the ◆ call line + ⎿ result summary ─────────────────────────
    #[test]
    fn tool_call_line_shows_raw_name_then_target() {
        // The mockup shape: `<icon> <raw tool_name>   <target>` — raw tool id first, salient target
        // after (a basename here), no English verb and no parenthesised footnote.
        let line = tool_call_line("file_read", &serde_json::json!({"path": "src/main.rs"}));
        let plain = console::strip_ansi_codes(&line).to_string();
        assert!(
            plain.contains("file_read"),
            "raw tool name shown: {plain:?}"
        );
        assert!(plain.contains("main.rs"), "salient target shown: {plain:?}");
        assert!(!plain.contains("Read "), "no English verb: {plain:?}");
    }

    #[test]
    fn tool_call_line_unmapped_shows_bare_name() {
        // An unknown tool with no salient field renders just the raw name (no crash, no JSON dump).
        let line = tool_call_line("mystery_tool", &serde_json::json!({"foo": "bar"}));
        let plain = console::strip_ansi_codes(&line).to_string();
        assert!(plain.contains("mystery_tool"), "{plain:?}");
    }

    #[test]
    fn tool_action_maps_common_tools_to_english_verbs() {
        let action = |n: &str, a: serde_json::Value| tool_action(n, &a).unwrap();
        assert_eq!(
            action(
                "file_write",
                serde_json::json!({"path": "C:\\Users\\admin\\scan.ps1"})
            ),
            "Write scan.ps1"
        );
        assert_eq!(
            action("file_read", serde_json::json!({"path": "/tmp/foo/bar.rs"})),
            "Read bar.rs"
        );
        assert_eq!(
            action(
                "shell_run",
                serde_json::json!({"command": "powershell -NoProfile -File C:\\Users\\admin\\scan.ps1"})
            ),
            "Run scan.ps1"
        );
        assert_eq!(
            action("skill_load", serde_json::json!({"name": "scan-windows"})),
            "Load skill scan-windows"
        );
        assert_eq!(
            action("todo_write", serde_json::json!({"todos": [{}, {}, {}]})),
            "Plan · 3 steps"
        );
        assert_eq!(
            action("todo_write", serde_json::json!({"todos": [{}]})),
            "Plan · 1 step"
        );
        assert_eq!(
            action(
                "web_fetch",
                serde_json::json!({"url": "https://docs.rs/tokio/index.html"})
            ),
            "Fetch docs.rs"
        );
        // an unmapped tool has no natural verb
        assert!(tool_action("mystery_tool", &serde_json::json!({})).is_none());
    }

    #[test]
    fn summarize_result_reads_signal_from_each_tool() {
        assert_eq!(
            summarize_result("file_read", "l1\nl2\nl3"),
            (true, "read 3 lines".to_string())
        );
        assert_eq!(
            summarize_result("shell_run", "exit 0\nok"),
            (true, "exit 0".to_string())
        );
        assert_eq!(
            summarize_result("shell_run", "exit 2\nboom"),
            (false, "exit 2".to_string())
        );
        assert_eq!(
            summarize_result("file_glob", "a.rs\nb.rs"),
            (true, "2 files".to_string())
        );
        assert_eq!(
            summarize_result("file_glob", "(no files match 'x')"),
            (true, "0 files".to_string())
        );
        // an edit result → target + counts derived from the embedded unified diff
        let edit = "edited src/x.rs (1 replacement(s))\n a\n-old\n+new\n b";
        let (ok, s) = summarize_result("file_edit", edit);
        assert!(
            ok && s.starts_with("edited src/x.rs") && s.contains("+1"),
            "{s:?}"
        );
        assert_eq!(
            summarize_result("file_edit", "created src/n.rs"),
            (true, "created src/n.rs".to_string())
        );
        // a tool with no special arm reuses its own header (sans a trailing ':')
        assert_eq!(
            summarize_result("search_files", "7 match(es) in 2 file(s):\nsrc/a.rs:3: hit"),
            (true, "7 match(es) in 2 file(s)".to_string())
        );
        // an error is coloured as a failure
        let (ok, s) = summarize_result("file_edit", "error: old_string not found");
        assert!(!ok && s.starts_with("error:"), "{s:?}");
    }

    #[test]
    fn count_diff_counts_only_column0_markers() {
        let out = "edited x (1)\n a\n-gone\n+added\n+also\n…(3 more lines added)\n b";
        assert_eq!(
            count_diff(out),
            (2, 1),
            "two '+' lines, one '-'; '…' and ' ' ignored"
        );
    }

    #[test]
    fn edit_target_labels_create_vs_edit() {
        assert_eq!(
            edit_target("edited src/x.rs (1 replacement(s))"),
            "edited src/x.rs"
        );
        assert_eq!(edit_target("created src/n.rs"), "created src/n.rs");
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
            Ok(args
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string())
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
        fn workspace_effect(
            &self,
            _args: &serde_json::Value,
        ) -> crate::agent::tools::WorkspaceEffect {
            crate::agent::tools::WorkspaceEffect::Paths
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

    /// Returns CONSTANT bytes regardless of args — models a tool called with distinct arguments
    /// that nonetheless surfaces no new information (the successful-but-useless re-read, W4).
    struct ConstTool;
    impl Tool for ConstTool {
        fn name(&self) -> &str {
            "konst"
        }
        fn description(&self) -> &str {
            "returns constant bytes"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{"i":{"type":"string"}}})
        }
        fn execute(&self, _args: &serde_json::Value) -> Result<String> {
            Ok("CONST".into())
        }
    }

    /// Returns Ok(...) with a body that carries NO `error:`/`exit N` shape but self-declares failure
    /// via `result_is_error` — models an MCP tool encoding `{"isError":true}` (W12).
    struct SelfErrTool;
    impl Tool for SelfErrTool {
        fn name(&self) -> &str {
            "selferr"
        }
        fn description(&self) -> &str {
            "returns a body that self-declares failure"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{"i":{"type":"string"}}})
        }
        fn execute(&self, _args: &serde_json::Value) -> Result<String> {
            Ok("{\"isError\":true,\"detail\":\"upstream 500\"}".into())
        }
        fn result_is_error(&self, result: &str) -> Option<bool> {
            Some(result.contains("\"isError\":true"))
        }
    }

    /// Named "file_read" (a relevance-truncatable tool per `is_relevance_truncatable`) so tests can
    /// exercise the W22 fetch-budget split without a real file. Returns a long fixed document.
    struct LongReadTool;
    impl Tool for LongReadTool {
        fn name(&self) -> &str {
            "file_read"
        }
        fn description(&self) -> &str {
            "test stand-in for file_read"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{}})
        }
        fn execute(&self, _args: &serde_json::Value) -> Result<String> {
            Ok((0..500)
                .map(|i| format!("line {i} of filler text"))
                .collect::<Vec<_>>()
                .join("\n"))
        }
    }

    /// Stateful: returns an INCREMENTING value each call, so every invocation surfaces NEW content
    /// regardless of args — models a legitimate poll/consume loop, used to prove a productive
    /// repeated-signature loop is NOT hard-stopped as divergence.
    struct TickTool(std::sync::atomic::AtomicUsize);
    impl Tool for TickTool {
        fn name(&self) -> &str {
            "tick"
        }
        fn description(&self) -> &str {
            "returns an incrementing counter"
        }
        fn parameters(&self) -> serde_json::Value {
            serde_json::json!({"type":"object","properties":{"s":{"type":"string"}}})
        }
        fn execute(&self, _args: &serde_json::Value) -> Result<String> {
            let n = self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Ok(format!("tick-{n}"))
        }
    }

    // ── helpers ──────────────────────────────────────────────────────────────
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
    /// A turn with SEVERAL tool calls (for testing turns that pad a failing call with a throwaway).
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
    fn call(id: &str, name: &str, args: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            kind: "function".into(),
            function: FunctionCall {
                name: name.into(),
                arguments: args.into(),
            },
        }
    }

    fn registry() -> ToolRegistry {
        let mut r = ToolRegistry::new();
        r.register(Box::new(EchoTool));
        r.register(Box::new(FailTool));
        r.register(Box::new(DeleteTool));
        r.register(Box::new(ConstTool));
        r.register(Box::new(SelfErrTool));
        r.register(Box::new(TickTool(std::sync::atomic::AtomicUsize::new(0))));
        r
    }

    fn cfg() -> AgentConfig {
        // Verify gate OFF in unit tests (it spawns a real `cargo check`, non-hermetic).
        AgentConfig {
            max_iters: 5,
            auto_extend_to: 5,
            max_tool_result_chars: 4096,
            max_fetch_result_chars: 12_000,
            approval_mode: crate::core::approval::ApprovalMode::Ask,
            cancel: crate::core::cancel::TurnCancel::new(),
            exec_ctx: crate::core::exec_ctx::ExecutionContext::default(),
            quiet: true,
            enable_verify_gate: false,
            verify_gate_timeout_secs: 90,
            auto_checkpoint: false, // OFF in tests: cwd is a real repo — no checkpoint pollution
            checkpoint_each_edit: false, // OFF in tests for the same reason
            context_window: 0, // guard off by default in tests; the guard test sets it explicitly
            keep_recent_tool_results: 8,
            clear_tool_result_min_chars: 1024,
            clear_at_pct: 60,
            clear_target_pct: 45,
            clear_step_pct: 10,
            clear_cooldown_iters: 6,
            todo_reminder_every: 0, // recitation OFF in unit tests (todo state is process-global)
            compact_at_pct: 80,
            context_guard_pct: 90,
            max_verify_attempts: 2,
            enable_self_review: false,
            enable_lsp: false,
            lsp_request_timeout_secs: 20,
            // P0 harness OFF in unit tests unless a test arms them (process-global todos + no
            // accidental early-exit blocks on unrelated scripts).
            enable_todo_poke: false,
            max_todo_poke_attempts: 2,
            enable_confidence_gate: false,
            conf_high: 90,
            conf_spike_delta: 40,
            enable_hill_climb: false,
            hill_climb_gate: 90,
            hill_climb_reminder_every: 6,
            // Mirrors the production top-level default: a chat error is fatal unless a test opts in
            // (the delegated-loop tests set it explicitly).
            max_transient_retries: 0,
            goal: None, // goal mode OFF in unit tests unless a test arms it
            // Steering OFF by default in unit tests: the mailbox is process-global, so an unrelated
            // script must not pick up a steer a steering test left behind.
            enable_steering: false,
            on_progress: None, // no live-history publishing in unit tests
        }
    }

    /// A scripted fake model: pops the next turn; empties → a final "stop".
    fn scripted(
        turns: Vec<ChatTurn>,
    ) -> impl Fn(Vec<Message>, Vec<ToolDef>) -> std::future::Ready<Result<ChatTurn>> {
        let q = Mutex::new(VecDeque::from(turns));
        move |_m, _d| {
            let next = q
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| final_turn("stop"));
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
        crate::core::cancel::with_current(cfg.cancel.clone(), || {
            run_tool_body(
                tool,
                &args,
                cfg.quiet,
                cfg.max_tool_result_chars,
                cfg.max_fetch_result_chars,
            )
        })
    }

    /// Drive the async executor the way the loop does: pre-filled placeholder sink, results out.
    async fn exec(r: &ToolRegistry, calls: &[ToolCall], c: &AgentConfig) -> Vec<(String, String)> {
        let mut sink: Vec<Message> = calls
            .iter()
            .map(|tc| Message::tool_result(tc.id.clone(), INTERRUPTED_TOOL_PLACEHOLDER.to_string()))
            .collect();
        let mut checkpointed = false;
        let mut writer_lease = None;
        let results = execute_calls(
            r,
            calls,
            c,
            &mut sink,
            Vec::new(),
            &mut checkpointed,
            &mut writer_lease,
        )
        .await;
        // The sink must mirror the returned results (the loop relies on it).
        for (k, (_, out)) in results.iter().enumerate() {
            assert_eq!(
                sink[k].content.as_deref(),
                Some(out.as_str()),
                "sink[{k}] mirrors the result"
            );
        }
        results
    }

    #[test]
    fn hard_floor_blocks_even_under_yolo() {
        // THE security invariant: a catastrophic command is refused even with yolo approval.
        // The floor runs BEFORE the approval short-circuit, so yolo cannot bypass it.
        let mut r = ToolRegistry::new();
        r.register(Box::new(ShellStub));
        let mut c = cfg();
        c.approval_mode = crate::core::approval::ApprovalMode::Yolo; // yolo
        let out =
            execute_one_for_test(&r, &call("1", "shell_run", r#"{"command":"rm -rf /"}"#), &c);
        assert!(
            out.contains("blocked by the hard safety floor"),
            "got: {out}"
        );
        assert!(!out.contains("RAN"), "the command must NOT have executed");
    }

    #[test]
    fn smart_auto_runs_readonly_without_approval() {
        // Under `smart` (and non-TTY, where approve() would otherwise deny), a read-only command runs.
        let mut r = ToolRegistry::new();
        r.register(Box::new(ShellStub));
        let mut c = cfg();
        c.approval_mode = crate::core::approval::ApprovalMode::Smart;
        let out = execute_one_for_test(&r, &call("1", "shell_run", r#"{"command":"ls -la"}"#), &c);
        assert_eq!(out, "RAN", "read-only shell should auto-run under smart");
    }

    #[test]
    fn smart_still_asks_for_writes() {
        // A write-shaped command under smart (non-TTY) → safe-deny, NOT auto-run.
        let mut r = ToolRegistry::new();
        r.register(Box::new(ShellStub));
        let mut c = cfg();
        c.approval_mode = crate::core::approval::ApprovalMode::Smart; // not yolo
        let out = execute_one_for_test(
            &r,
            &call("1", "shell_run", r#"{"command":"rm -rf node_modules"}"#),
            &c,
        );
        assert!(
            !out.contains("RAN"),
            "a write must not auto-run under smart; got: {out}"
        );
    }

    #[test]
    fn auto_extension_policy_requires_a_real_initial_cap_and_convergence() {
        assert!(should_auto_extend(3, 3, 6, false, 0, false));
        assert!(!should_auto_extend(0, 0, 6, false, 0, false));
        assert!(!should_auto_extend(3, 3, 3, false, 0, false));
        assert!(!should_auto_extend(3, 3, 6, true, 0, false));
        assert!(!should_auto_extend(
            3,
            3,
            6,
            false,
            STUCK_NUDGE_STREAK,
            false
        ));
        assert!(!should_auto_extend(3, 3, 6, false, 0, true));
        assert!(
            !should_auto_extend(3, 6, 8, true, 0, false),
            "extension is one-shot"
        );
    }

    #[tokio::test]
    async fn done_gate_at_initial_cap_can_reach_final_answer_after_extension() {
        let r = registry();
        // Two gate-only iterations reach the initial cap without traversing the tool path. The
        // boundary must still grant the extension before the third model call.
        let c = AgentConfig {
            max_iters: 2,
            auto_extend_to: 4,
            enable_steering: false,
            ..cfg()
        };
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        let out = run_agent_loop(
            scripted(vec![
                tool_turn("echo", r#"{"text":"first"}"#),
                tool_turn("echo", r#"{"text":"second"}"#),
                final_turn("done after extension"),
            ]),
            &c,
            &r,
            &mut messages,
        )
        .await
        .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(out.final_text.as_deref(), Some("done after extension"));
        assert!(messages.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|s| s.starts_with(NUDGE_STEP_LIMIT))));
    }

    #[tokio::test]
    async fn final_answer_immediately_is_done() {
        let r = registry();
        let out = run_agent(
            scripted(vec![final_turn("hello")]),
            &cfg(),
            &r,
            "sys",
            "task",
        )
        .await
        .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(out.final_text.as_deref(), Some("hello"));
        assert_eq!(out.iters, 1);
    }

    #[tokio::test]
    async fn cancellation_is_turn_local() {
        let cancelled_cfg = cfg();
        let live_cfg = cfg();
        cancelled_cfg.cancel.cancel();
        let r1 = registry();
        let r2 = registry();
        let (cancelled, live) = tokio::join!(
            run_agent(
                scripted(vec![final_turn("should-not-run")]),
                &cancelled_cfg,
                &r1,
                "sys",
                "task"
            ),
            run_agent(
                scripted(vec![final_turn("still-runs")]),
                &live_cfg,
                &r2,
                "sys",
                "task"
            ),
        );
        let cancelled = cancelled.unwrap();
        let live = live.unwrap();
        assert_eq!(cancelled.stop, StopReason::Cancelled);
        assert_eq!(cancelled.iters, 0);
        assert_eq!(live.stop, StopReason::Done);
        assert_eq!(live.final_text.as_deref(), Some("still-runs"));
    }

    #[tokio::test]
    async fn detects_tools_despite_finish_reason_stop_then_finishes() {
        let r = registry();
        let out = run_agent(
            scripted(vec![
                tool_turn("echo", r#"{"text":"hi"}"#),
                final_turn("done"),
            ]),
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
        let out = run_agent(
            scripted(vec![same(), same(), same()]),
            &cfg(),
            &r,
            "sys",
            "task",
        )
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
        // ask mode + non-TTY test env → safe-deny → "declined" fed back → model stops.
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
        let out = run_agent(scripted(turns), &cfg(), &r, "sys", "task")
            .await
            .unwrap();
        assert_eq!(out.stop, StopReason::MaxIters);
        assert_eq!(out.iters, 5);
    }

    #[tokio::test]
    async fn thrash_guard_stops_distinct_but_all_failing_turns() {
        let r = registry();
        // Every turn calls the always-failing `fail` tool with DIFFERENT args, so the identical-
        // signature Divergence check never trips — the thrash guard (all-fail, no-edit streak) must
        // still stop the flail well before the (raised) step cap.
        let c = AgentConfig {
            max_iters: 20,
            auto_extend_to: 20,
            ..cfg()
        };
        let turns = vec![
            tool_turn("fail", r#"{"n":"1"}"#),
            tool_turn("fail", r#"{"n":"2"}"#),
            tool_turn("fail", r#"{"n":"3"}"#),
            tool_turn("fail", r#"{"n":"4"}"#),
            tool_turn("fail", r#"{"n":"5"}"#),
            tool_turn("fail", r#"{"n":"6"}"#),
            tool_turn("fail", r#"{"n":"7"}"#),
            final_turn("should not reach"),
        ];
        let out = run_agent(scripted(turns), &c, &r, "sys", "task")
            .await
            .unwrap();
        assert_eq!(
            out.stop,
            StopReason::Divergence,
            "thrash guard must stop the all-failing flail"
        );
        // Stops on the STUCK_STOP_STREAK-th consecutive unproductive turn, not at the cap of 20.
        assert_eq!(out.iters, STUCK_STOP_STREAK);
    }

    #[tokio::test]
    async fn auto_extend_grants_more_room() {
        let r = registry();
        let c = AgentConfig {
            max_iters: 2,
            auto_extend_to: 4,
            quiet: true,
            enable_todo_poke: false,
            enable_confidence_gate: false,
            enable_hill_climb: false,
            ..Default::default()
        };
        // 3 distinct tool turns then finish: would hit max_iters=2, but auto-extend to 4 lets it finish.
        let turns = vec![
            tool_turn("echo", r#"{"text":"1"}"#),
            tool_turn("echo", r#"{"text":"2"}"#),
            tool_turn("echo", r#"{"text":"3"}"#),
            final_turn("done"),
        ];
        let out = run_agent(scripted(turns), &c, &r, "sys", "task")
            .await
            .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert!(
            out.iters > 2,
            "auto-extend should let it run past the initial cap"
        );
    }

    // ── P1 anti-loop overhaul (W1/W2/W3/W4/W6/W9/W10) ─────────────────────────

    #[tokio::test]
    async fn two_cycle_oscillation_stops() {
        let r = registry();
        // A,B,A,B,… never converging. The old single-slot last_sig ran to MaxIters (consecutive
        // signatures always differ); the ring's 2-cycle detector must stop it within a few turns.
        let c = AgentConfig {
            max_iters: 30,
            auto_extend_to: 30,
            ..cfg()
        };
        let turns = vec![
            tool_turn("echo", r#"{"text":"a"}"#),
            tool_turn("echo", r#"{"text":"b"}"#),
            tool_turn("echo", r#"{"text":"a"}"#),
            tool_turn("echo", r#"{"text":"b"}"#),
            tool_turn("echo", r#"{"text":"a"}"#),
            tool_turn("echo", r#"{"text":"b"}"#),
            tool_turn("echo", r#"{"text":"a"}"#),
            final_turn("unreached"),
        ];
        let out = run_agent(scripted(turns), &c, &r, "sys", "task")
            .await
            .unwrap();
        assert_eq!(
            out.stop,
            StopReason::Divergence,
            "A,B,A,B oscillation must be caught"
        );
        // EXACTLY 6: the 2-cycle detector stops on the 6th turn. If is_two_cycle were broken, only the
        // thrash guard would act and it would stop at 7 (streak 5) — so this pins the detector, not the
        // fallback. (Complemented by the is_two_cycle unit test below.)
        assert_eq!(
            out.iters, 6,
            "the 2-cycle detector must stop at 6, not fall through to thrash at 7"
        );
    }

    #[test]
    fn is_two_cycle_detects_abab_only() {
        use std::collections::VecDeque;
        let ring = |v: &[&str]| VecDeque::from(v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        // Genuine A,B,A,B: ring tail [A,B,A], current B completes it.
        assert!(is_two_cycle(&ring(&["a", "b", "a"]), "b"));
        // Immediate repeat A,A,A is NOT a 2-cycle (the two alternating signatures must differ).
        assert!(!is_two_cycle(&ring(&["a", "a", "a"]), "a"));
        // Three distinct calls: no cycle.
        assert!(!is_two_cycle(&ring(&["a", "b", "c"]), "d"));
        // Too short to judge.
        assert!(!is_two_cycle(&ring(&["a", "b"]), "a"));
        // INTERSPERSED (…B,X,A + current B) is intentionally NOT caught — left to the thrash guard.
        assert!(!is_two_cycle(&ring(&["b", "x", "a"]), "b"));
        // ring tail [A,B,A] + current A is an immediate repeat, not a 2-cycle.
        assert!(!is_two_cycle(&ring(&["a", "b", "a"]), "a"));
    }

    #[tokio::test]
    async fn productive_poll_loop_not_stopped() {
        let r = registry();
        // A,B,A,B of two fixed-arg calls that each return NEW content every time (a legit poll/consume
        // loop). The 2-cycle detector must NOT hard-stop it: the novel content clears the latch each
        // time, so it runs to completion. (Regression test for the false-positive the review found.)
        let c = AgentConfig {
            max_iters: 8,
            auto_extend_to: 8,
            ..cfg()
        };
        let turns = vec![
            tool_turn("tick", r#"{"s":"a"}"#),
            tool_turn("tick", r#"{"s":"b"}"#),
            tool_turn("tick", r#"{"s":"a"}"#),
            tool_turn("tick", r#"{"s":"b"}"#),
            tool_turn("tick", r#"{"s":"a"}"#),
            tool_turn("tick", r#"{"s":"b"}"#),
            final_turn("done polling"),
        ];
        let out = run_agent(scripted(turns), &c, &r, "sys", "task")
            .await
            .unwrap();
        assert_eq!(
            out.stop,
            StopReason::Done,
            "a productive poll loop must not be flagged as divergence"
        );
        assert_eq!(out.final_text.as_deref(), Some("done polling"));
    }

    #[test]
    fn canonicalization_collapses_whitespace_and_key_order() {
        // W2: reformatted-JSON whitespace and key reordering must NOT dodge the signature.
        let a = vec![call("1", "echo", r#"{"text":"x"}"#)];
        let b = vec![call("1", "echo", r#"{  "text" :   "x"  }"#)];
        assert_eq!(
            turn_signature(&a),
            turn_signature(&b),
            "whitespace must not change the signature"
        );
        let c = vec![call("1", "t", r#"{"b":1,"a":2}"#)];
        let d = vec![call("1", "t", r#"{"a":2,"b":1}"#)];
        assert_eq!(
            turn_signature(&c),
            turn_signature(&d),
            "key order must not change the signature"
        );
    }

    #[test]
    fn pagination_window_kept_distinct_in_signature() {
        // W2 guardrail: a DIFFERENT read window is different work and must stay a distinct signature —
        // legit sequential paging must never be collapsed into a divergence.
        let p1 = vec![call("1", "file_read", r#"{"path":"X","start":1,"end":50}"#)];
        let p2 = vec![call(
            "1",
            "file_read",
            r#"{"path":"X","start":51,"end":100}"#,
        )];
        assert_ne!(
            turn_signature(&p1),
            turn_signature(&p2),
            "distinct read windows must stay distinct (the useless same-bytes re-read is caught by content novelty, not the signature)"
        );
    }

    #[tokio::test]
    async fn padded_flail_with_stale_read_still_stops() {
        let r = registry();
        // Every turn pads a failing call with a throwaway echo of CONSTANT bytes. The old guard reset
        // on the one non-failure result and never stopped; now the stale echo is not novel, so the
        // unproductive streak climbs to a stop.
        let c = AgentConfig {
            max_iters: 20,
            auto_extend_to: 20,
            ..cfg()
        };
        let turns = vec![
            multi_tool_turn(&[("fail", r#"{"n":"1"}"#), ("echo", r#"{"text":"same"}"#)]),
            multi_tool_turn(&[("fail", r#"{"n":"2"}"#), ("echo", r#"{"text":"same"}"#)]),
            multi_tool_turn(&[("fail", r#"{"n":"3"}"#), ("echo", r#"{"text":"same"}"#)]),
            multi_tool_turn(&[("fail", r#"{"n":"4"}"#), ("echo", r#"{"text":"same"}"#)]),
            multi_tool_turn(&[("fail", r#"{"n":"5"}"#), ("echo", r#"{"text":"same"}"#)]),
            multi_tool_turn(&[("fail", r#"{"n":"6"}"#), ("echo", r#"{"text":"same"}"#)]),
            multi_tool_turn(&[("fail", r#"{"n":"7"}"#), ("echo", r#"{"text":"same"}"#)]),
            final_turn("unreached"),
        ];
        let out = run_agent(scripted(turns), &c, &r, "sys", "task")
            .await
            .unwrap();
        assert_eq!(
            out.stop,
            StopReason::Divergence,
            "a padded flail must still stop"
        );
    }

    #[tokio::test]
    async fn useless_successful_reread_loop_stops() {
        let r = registry();
        // Distinct args (no divergence) but the tool returns CONSTANT bytes — novel only once, then
        // the streak climbs to a stop. A successful-but-useless loop must terminate, not run to cap.
        let c = AgentConfig {
            max_iters: 20,
            auto_extend_to: 20,
            ..cfg()
        };
        let turns = vec![
            tool_turn("konst", r#"{"i":"1"}"#),
            tool_turn("konst", r#"{"i":"2"}"#),
            tool_turn("konst", r#"{"i":"3"}"#),
            tool_turn("konst", r#"{"i":"4"}"#),
            tool_turn("konst", r#"{"i":"5"}"#),
            tool_turn("konst", r#"{"i":"6"}"#),
            tool_turn("konst", r#"{"i":"7"}"#),
            final_turn("unreached"),
        ];
        let out = run_agent(scripted(turns), &c, &r, "sys", "task")
            .await
            .unwrap();
        assert_eq!(
            out.stop,
            StopReason::Divergence,
            "a useless successful re-read loop must stop"
        );
    }

    #[tokio::test]
    async fn legit_reread_after_failed_edit_recovers() {
        let r = registry();
        // THE hardest trap: the system-prompt-sanctioned recovery — a failed edit, then a re-read
        // (stale bytes) to copy exact text, then a SUCCESSFUL edit. Peak streak 2 < STUCK_NUDGE_STREAK,
        // so NO nudge and NO stop — the recovery must never be punished.
        let c = AgentConfig {
            approval_mode: crate::core::approval::ApprovalMode::Yolo,
            max_iters: 10,
            auto_extend_to: 10,
            ..cfg()
        };
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        let chat = scripted(vec![
            tool_turn("echo", r#"{"text":"filecontent"}"#), // seed: read the file
            tool_turn("fail", r#"{}"#),                     // failed edit
            tool_turn("echo", r#"{"text":"filecontent"}"#), // re-read to copy exact text (stale bytes)
            tool_turn("delete", r#"{"x":"1"}"#),            // successful edit → resets everything
            final_turn("recovered"),
        ]);
        // Same home-stability need as the divergence test above: the delete call's writer lease
        // resolves its lock path through `nextgen_home()`, and a concurrent sandbox flip can fail
        // it — turning the "successful edit resets everything" step into a failure.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(
            out.stop,
            StopReason::Done,
            "legit re-read→retry recovery must not be punished"
        );
        assert_eq!(out.final_text.as_deref(), Some("recovered"));
        // Peak streak is 2 (< STUCK_NUDGE_STREAK) and no signature repeats, so NO nudge of either
        // kind must appear — the documented "not punished" property, now asserted.
        assert!(
            !messages.iter().any(|m| m.role == "system"
                && m.content.as_deref().is_some_and(|c| {
                    c.starts_with(NUDGE_STUCK) || c.starts_with(NUDGE_DIVERGENCE)
                })),
            "the recovery path must trigger no divergence/stuck nudge"
        );
    }

    #[tokio::test]
    async fn repeated_identical_destructive_call_stops() {
        let r = registry();
        // A repeated IDENTICAL successful destructive call (same signature, same body — models
        // `git commit --allow-empty`, `>> log`, a no-op file_write) must hard-stop as Divergence, not
        // run to the extended cap re-executing the side effect. Regression guard: a successful edit is
        // "productive" for the thrash streak but must NOT clear the divergence latch (only novel
        // content does), or the insert()==false hard-stop becomes unreachable.
        let c = AgentConfig {
            approval_mode: crate::core::approval::ApprovalMode::Yolo,
            max_iters: 4,
            auto_extend_to: 20,
            ..cfg()
        };
        let turns = vec![
            tool_turn("delete", r#"{}"#),
            tool_turn("delete", r#"{}"#),
            tool_turn("delete", r#"{}"#),
            tool_turn("delete", r#"{}"#),
            tool_turn("delete", r#"{}"#),
            final_turn("unreached"),
        ];
        // Serialize with home-MUTATING sandbox tests: the destructive call's writer lease resolves
        // its lock path through `nextgen_home()` AT ACQUIRE TIME, so a concurrent sandbox flip
        // (AIZEN_HOME repointed / tree deleted) can fail call #1 — then the first SUCCESS lands on
        // the nudge iteration, its novel content clears the divergence latch once, and the
        // hard-stop drifts 3 → 4. The exact-iteration assertion below needs a stable home.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let out = run_agent(scripted(turns), &c, &r, "sys", "task")
            .await
            .unwrap();
        assert_eq!(
            out.stop,
            StopReason::Divergence,
            "a repeated identical destructive call must stop"
        );
        assert_eq!(
            out.iters, 3,
            "must hard-stop by the 3rd identical call, not run to the extended cap"
        );
    }

    #[test]
    fn auto_checkpoint_defaults_on_and_off_in_tests() {
        // W15: production defaults the auto-checkpoint latch ON, but the unit-test cfg() forces it
        // OFF (the test cwd is a real git repo — a checkpoint per destructive test would pollute it).
        assert!(
            AgentConfig::default().auto_checkpoint,
            "production default must be ON"
        );
        assert!(
            !cfg().auto_checkpoint,
            "test cfg must force it OFF to avoid repo pollution"
        );
        // The per-edit-turn checkpoint (Cline-style: a restore point after EACH editing turn, not
        // just once before the first) is gated behind the SAME latch, so it also defaults ON in
        // production and OFF in tests — a per-edit checkpoint would pollute the real test repo.
        assert!(
            AgentConfig::default().checkpoint_each_edit,
            "per-edit checkpoint default must be ON"
        );
        assert!(
            !cfg().checkpoint_each_edit,
            "test cfg must force per-edit checkpoint OFF"
        );
    }

    #[tokio::test]
    async fn large_task_many_distinct_edits_no_false_positive() {
        let r = registry();
        // A big task that lands many DISTINCT successful edits must never be flagged: each edit is
        // productive (streak pinned 0) and distinct args keep signatures distinct (no divergence).
        let c = AgentConfig {
            approval_mode: crate::core::approval::ApprovalMode::Yolo,
            max_iters: 30,
            auto_extend_to: 30,
            ..cfg()
        };
        let turns = vec![
            tool_turn("delete", r#"{"d":"1"}"#),
            tool_turn("delete", r#"{"d":"2"}"#),
            tool_turn("delete", r#"{"d":"3"}"#),
            tool_turn("delete", r#"{"d":"4"}"#),
            tool_turn("delete", r#"{"d":"5"}"#),
            tool_turn("delete", r#"{"d":"6"}"#),
            tool_turn("delete", r#"{"d":"7"}"#),
            tool_turn("delete", r#"{"d":"8"}"#),
            final_turn("all done"),
        ];
        let out = run_agent(scripted(turns), &c, &r, "sys", "task")
            .await
            .unwrap();
        assert_eq!(
            out.stop,
            StopReason::Done,
            "a legit many-edit task must complete"
        );
        assert_eq!(out.final_text.as_deref(), Some("all done"));
    }

    #[tokio::test]
    async fn repeat_after_progress_gets_fresh_nudge() {
        let r = registry();
        // W6: a repeated call, then genuine progress (clears the nudge memory), then the SAME call
        // repeats again — it must earn a FRESH nudge and recover. The old run-global latch would have
        // hard-stopped Divergence at the later repeat.
        let c = AgentConfig {
            max_iters: 12,
            auto_extend_to: 12,
            ..cfg()
        };
        let turns = vec![
            tool_turn("echo", r#"{"text":"x"}"#),
            tool_turn("echo", r#"{"text":"x"}"#), // immediate repeat → nudge
            tool_turn("echo", r#"{"text":"y"}"#), // progress → clears nudged_sigs
            tool_turn("echo", r#"{"text":"x"}"#),
            tool_turn("echo", r#"{"text":"x"}"#), // repeat again → FRESH nudge (not a hard stop)
            final_turn("recovered"),
        ];
        let out = run_agent(scripted(turns), &c, &r, "sys", "task")
            .await
            .unwrap();
        assert_eq!(
            out.stop,
            StopReason::Done,
            "a repeat after real progress gets a fresh nudge, not a stop"
        );
        assert_eq!(out.final_text.as_deref(), Some("recovered"));
    }

    #[tokio::test]
    async fn maxiters_synthesizes_final_answer() {
        let r = registry();
        // W9: 3 productive echoes exhaust the cap; the tool-free synthesis call then produces a final
        // answer where the old code returned None.
        let c = AgentConfig {
            max_iters: 3,
            auto_extend_to: 3,
            ..cfg()
        };
        let turns = vec![
            tool_turn("echo", r#"{"text":"1"}"#),
            tool_turn("echo", r#"{"text":"2"}"#),
            tool_turn("echo", r#"{"text":"3"}"#),
            final_turn("synth-answer"), // consumed by the synthesis call
        ];
        let out = run_agent(scripted(turns), &c, &r, "sys", "task")
            .await
            .unwrap();
        assert_eq!(out.stop, StopReason::MaxIters);
        assert_eq!(out.iters, 3);
        assert_eq!(
            out.final_text.as_deref(),
            Some("synth-answer"),
            "MaxIters must synthesize an answer"
        );
    }

    #[tokio::test]
    async fn maxiters_synthesis_degrades_to_none_on_error() {
        let r = registry();
        // If the synthesis call itself errors, MaxIters degrades to final_text None without panicking.
        let c = AgentConfig {
            max_iters: 3,
            auto_extend_to: 3,
            ..cfg()
        };
        let calls = Mutex::new(0usize);
        let chat = move |msgs: Vec<Message>, _defs: Vec<ToolDef>| {
            let is_synth = msgs
                .last()
                .and_then(|m| m.content.as_deref())
                .is_some_and(|c| c.contains("reached the step limit"));
            let mut n = calls.lock().unwrap();
            *n += 1;
            let out: Result<ChatTurn> = if is_synth {
                Err(anyhow::anyhow!("synthesis boom"))
            } else {
                Ok(tool_turn("echo", &format!(r#"{{"text":"{n}"}}"#)))
            };
            std::future::ready(out)
        };
        let out = run_agent(chat, &c, &r, "sys", "task").await.unwrap();
        assert_eq!(out.stop, StopReason::MaxIters);
        assert_eq!(
            out.final_text, None,
            "a failed synthesis degrades cleanly to None"
        );
    }

    #[tokio::test]
    async fn maxiters_synthesis_uses_throwaway_clone_and_empty_defs() {
        let r = registry();
        let c = AgentConfig {
            max_iters: 2,
            auto_extend_to: 2,
            ..cfg()
        };
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        let saw_synth = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let defs_empty = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ss = saw_synth.clone();
        let de = defs_empty.clone();
        let calls = Mutex::new(0usize);
        let chat = move |msgs: Vec<Message>, defs: Vec<ToolDef>| {
            use std::sync::atomic::Ordering::Relaxed;
            let is_synth = msgs
                .last()
                .and_then(|m| m.content.as_deref())
                .is_some_and(|c| c.contains("reached the step limit"));
            let mut n = calls.lock().unwrap();
            *n += 1;
            let out: Result<ChatTurn> = if is_synth {
                ss.store(true, Relaxed);
                de.store(defs.is_empty(), Relaxed);
                Ok(final_turn("final answer"))
            } else {
                Ok(tool_turn("echo", &format!(r#"{{"text":"{n}"}}"#)))
            };
            std::future::ready(out)
        };
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        use std::sync::atomic::Ordering::Relaxed;
        assert_eq!(out.stop, StopReason::MaxIters);
        assert!(
            saw_synth.load(Relaxed),
            "the synthesis call must fire on MaxIters"
        );
        assert!(
            defs_empty.load(Relaxed),
            "synthesis must pass EMPTY tool defs (tool-free)"
        );
        assert_eq!(out.final_text.as_deref(), Some("final answer"));
        // Throwaway-clone invariant (#3): the synthesis PROMPT must not leak into real history…
        assert!(
            !messages.iter().any(|m| m
                .content
                .as_deref()
                .is_some_and(|c| c.contains("reached the step limit"))),
            "the synthesis prompt must live only on the throwaway clone, never real messages"
        );
        // …but the synthesized ANSWER is appended so multi-turn callers keep it.
        assert!(
            messages
                .iter()
                .any(|m| m.role == "assistant" && m.content.as_deref() == Some("final answer")),
            "the synthesized answer must be appended for the caller"
        );
    }

    #[tokio::test]
    async fn wanderer_not_auto_extended() {
        let r = registry();
        // W10: a wandering run (unproductive at the cap) must NOT be granted the extension — it
        // proceeds to the MaxIters synthesis at the base cap instead of getting a bigger budget.
        let c = AgentConfig {
            max_iters: 3,
            auto_extend_to: 6,
            ..cfg()
        };
        let turns = vec![
            tool_turn("fail", r#"{"n":"1"}"#),
            tool_turn("fail", r#"{"n":"2"}"#),
            tool_turn("fail", r#"{"n":"3"}"#),
            final_turn("final"),
        ];
        let out = run_agent(scripted(turns), &c, &r, "sys", "task")
            .await
            .unwrap();
        assert_eq!(
            out.stop,
            StopReason::MaxIters,
            "a wanderer must hit MaxIters, not get extended"
        );
        assert_eq!(out.iters, 3, "the extension to 6 must be denied");
    }

    #[tokio::test]
    async fn context_guard_warns_once_when_window_nearly_full() {
        let r = registry();
        let c = AgentConfig {
            max_iters: 5,
            auto_extend_to: 5,
            quiet: true,
            context_window: 100,
            enable_todo_poke: false,
            enable_confidence_gate: false,
            enable_hill_climb: false,
            ..Default::default()
        };
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
            .filter(|m| {
                m.content
                    .as_deref()
                    .is_some_and(|c| c.contains("Context is nearly full"))
            })
            .count();
        assert_eq!(
            warnings, 1,
            "the budget nudge must fire exactly once, not per-iteration"
        );
    }

    #[tokio::test]
    async fn context_guard_disabled_when_window_zero() {
        let r = registry();
        // context_window defaults to 0 → guard off even with a huge history.
        let c = AgentConfig {
            max_iters: 5,
            auto_extend_to: 5,
            quiet: true,
            enable_todo_poke: false,
            enable_confidence_gate: false,
            enable_hill_climb: false,
            ..Default::default()
        };
        let mut messages = vec![Message::system("sys"), Message::user("x".repeat(5000))];
        let chat = scripted(vec![
            tool_turn("echo", r#"{"text":"a"}"#),
            final_turn("done"),
        ]);
        run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert!(
            !messages.iter().any(|m| m
                .content
                .as_deref()
                .is_some_and(|c| c.contains("Context is nearly full"))),
            "guard must stay silent when context_window is 0"
        );
    }

    #[tokio::test]
    async fn context_guard_threshold_is_config_driven() {
        // P-ctx4: the wrap-up guard reads context_guard_pct, not a hardcoded 90. Set it to 50 and a
        // half-full window must trip it; set it to 0 and even a full window must not.
        let r = registry();
        let mk = |pct: u8| AgentConfig {
            max_iters: 5,
            auto_extend_to: 5,
            quiet: true,
            context_window: 100,
            context_guard_pct: pct,
            clear_at_pct: 0,
            compact_at_pct: 0,
            enable_todo_poke: false,
            enable_confidence_gate: false,
            enable_hill_climb: false,
            ..Default::default()
        };
        // ~260 chars ≈ 65 tok → past 50% but under 90% of the 100-tok window.
        let hist = || vec![Message::system("sys"), Message::user("x".repeat(260))];
        let fired = |msgs: &[Message]| {
            msgs.iter().any(|m| {
                m.content
                    .as_deref()
                    .is_some_and(|c| c.contains("Context is nearly full"))
            })
        };

        let mut m50 = hist();
        run_agent_loop(
            scripted(vec![
                tool_turn("echo", r#"{"text":"a"}"#),
                final_turn("done"),
            ]),
            &mk(50),
            &r,
            &mut m50,
        )
        .await
        .unwrap();
        assert!(fired(&m50), "guard at 50% must trip on a ~65% window");

        let mut m0 = hist();
        run_agent_loop(
            scripted(vec![
                tool_turn("echo", r#"{"text":"a"}"#),
                final_turn("done"),
            ]),
            &mk(0),
            &r,
            &mut m0,
        )
        .await
        .unwrap();
        assert!(
            !fired(&m0),
            "context_guard_pct=0 disables the wrap-up guard"
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
        assert!(
            tok >= 1000,
            "4000-char arguments must dominate the estimate, got {tok}"
        );
        // Content-only messages count content/4 plus the flat envelope.
        let plain = Message::user("abcd".repeat(100)); // 400 chars → 100 tok
        assert_eq!(estimate_message_tokens(&plain), 100 + MSG_OVERHEAD_TOK);
    }

    #[test]
    fn defs_overhead_is_deterministic_and_published() {
        let r = registry();
        let tok = estimate_defs_tokens(&r.defs());
        assert!(
            tok > 0,
            "two registered tools must have a nonzero schema cost"
        );
        assert_eq!(
            estimate_defs_tokens(&r.defs()),
            tok,
            "same defs → same estimate"
        );
        assert!(
            schema_overhead_tokens() > 0,
            "the loop-published global must be readable"
        );
    }

    #[test]
    fn effective_tokens_prefers_anchor_and_tracks_growth() {
        assert_eq!(
            effective_tokens(500, None),
            500,
            "no anchor → plain estimate"
        );
        let a = RealAnchor {
            tokens: 900,
            est_at: 300,
        };
        assert_eq!(
            effective_tokens(300, Some(&a)),
            900,
            "at the anchor point → the real number"
        );
        assert_eq!(
            effective_tokens(350, Some(&a)),
            950,
            "growth rides on the real base"
        );
        assert_eq!(
            effective_tokens(250, Some(&a)),
            900,
            "never below the real base (saturating)"
        );
    }

    #[test]
    fn prompt_tier_heuristic_and_override() {
        // Small/local families and size suffixes → strict.
        assert_eq!(
            prompt_tier_for("qwen2.5-coder-7b", None),
            PromptTier::Strict
        );
        assert_eq!(
            prompt_tier_for("Llama-3.3-70B-Instruct", None),
            PromptTier::Strict,
            "llama family is strict"
        );
        assert_eq!(
            prompt_tier_for("gpt-4o-mini", None),
            PromptTier::Strict,
            "mini tier is strict"
        );
        assert_eq!(
            prompt_tier_for("mistral-small-latest", None),
            PromptTier::Strict
        );
        assert_eq!(
            prompt_tier_for("some-model-14b", None),
            PromptTier::Strict,
            "size suffix"
        );
        // Frontier / unknown → full (the safe default).
        assert_eq!(prompt_tier_for("claude-sonnet-4-6", None), PromptTier::Full);
        assert_eq!(prompt_tier_for("gpt-4o", None), PromptTier::Full);
        assert_eq!(
            prompt_tier_for("totally-unknown-model", None),
            PromptTier::Full
        );
        // Whole-token matching: substrings never false-positive.
        assert_eq!(prompt_tier_for("geminiacs-pro", None), PromptTier::Full);
        assert_eq!(
            prompt_tier_for("nanotech-writer-xl", None),
            PromptTier::Full
        );
        // Config override beats the heuristic, both ways.
        assert_eq!(
            prompt_tier_for("gpt-4o", Some("strict")),
            PromptTier::Strict
        );
        assert_eq!(
            prompt_tier_for("qwen2.5-coder-7b", Some("full")),
            PromptTier::Full
        );
    }

    #[test]
    fn system_prompt_is_byte_stable_per_tier() {
        // build_system_prompt reads global HOME state (skills/persona/config) — serialize with the
        // other sandboxing tests or a concurrent skill-write makes the two builds differ.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Same inputs → identical bytes (the prefix-cache invariant), on both tiers.
        let a = build_system_prompt("/w", "linux", "2026-07-05", "gpt-4o", None);
        let b = build_system_prompt("/w", "linux", "2026-07-05", "gpt-4o", None);
        assert_eq!(a, b, "full tier must be deterministic");
        let s1 = build_system_prompt("/w", "linux", "2026-07-05", "qwen2.5-coder-7b", None);
        let s2 = build_system_prompt("/w", "linux", "2026-07-05", "qwen2.5-coder-7b", None);
        assert_eq!(s1, s2, "strict tier must be deterministic");
        assert!(
            s1.starts_with(system_base_strict().trim_end()),
            "strict base leads the strict prompt"
        );
        assert!(s1.contains("OUTPUT CONTRACT"));
    }

    #[test]
    fn obfuscated_prompts_decode_correctly() {
        // The build-time XOR must round-trip: a wrong key/decoder would corrupt UTF-8 (panic in
        // decode) or drop the branding. Cheap guard so obfuscation can't silently break the prompt.
        let base = system_base();
        assert!(!base.is_empty(), "base prompt decoded empty");
        assert!(
            base.to_lowercase().contains("aizen"),
            "decoded base prompt mentions aizen"
        );
        let strict = system_base_strict();
        assert!(!strict.is_empty(), "strict prompt decoded empty");
        assert!(
            strict.contains("OUTPUT CONTRACT"),
            "strict prompt keeps its contract section"
        );
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
        assert!(
            !accept_anchor(24, 100),
            "below est/4 → cumulative-gateway garbage"
        );
        assert!(!accept_anchor(401, 100), "above est*4 → garbage");
    }

    #[tokio::test]
    async fn real_usage_anchor_triggers_wrapup_before_estimate_would() {
        let r = registry();
        let c = AgentConfig {
            max_iters: 5,
            auto_extend_to: 5,
            quiet: true,
            context_window: 1000,
            enable_todo_poke: false,
            enable_confidence_gate: false,
            enable_hill_climb: false,
            ..Default::default()
        };
        // ~1200 chars ≈ 300 tok estimated — far under 90% of the 1000-tok window on its own.
        let mut messages = vec![Message::system("sys"), Message::user("x".repeat(1200))];
        // …but the provider reports the request REALLY was 950 prompt tokens (code-heavy tokenization).
        let mut anchored = tool_turn("echo", r#"{"text":"a"}"#);
        anchored.usage = Some(crate::core::types::Usage {
            prompt_tokens: Some(950),
            ..Default::default()
        });
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
        let c = AgentConfig {
            context_window: 100,
            compact_at_pct: 80,
            ..cfg()
        };
        let mut messages = vec![
            Message::system("sys"),
            Message::user(format!("u1 {}", "x".repeat(200))),
            Message::assistant("a1"),
            Message::user(format!("u2 {}", "y".repeat(200))),
            Message::assistant("a2"),
            Message::user(format!("u3 {}", "z".repeat(200))),
        ];
        let summarize = |_msgs: Vec<Message>| async { Ok("DENSE_SUMMARY_OK".to_string()) };
        let out = run_agent_loop_compacting(
            scripted(vec![final_turn("done")]),
            summarize,
            &c,
            &r,
            &mut messages,
        )
        .await
        .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(
            messages[0].content.as_deref(),
            Some("sys"),
            "system prompt preserved at [0]"
        );
        assert!(
            messages.iter().any(|m| m
                .content
                .as_deref()
                .is_some_and(|x| x.contains("DENSE_SUMMARY_OK"))),
            "older turns were summarized into the injected compaction note"
        );
        // The bulky first turn was folded into the summary (no longer present verbatim).
        assert!(
            !messages.iter().any(|m| m
                .content
                .as_deref()
                .is_some_and(|x| x.contains(&"x".repeat(200)))),
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
        assert_eq!(
            results,
            vec![
                ("1".to_string(), "first".to_string()),
                ("2".to_string(), "second".to_string()),
                ("3".to_string(), "third".to_string()),
            ]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_calls_parallel_is_fail_soft() {
        // echo + fail are both read-only/safe → concurrent; one tool's error must not drop its
        // sibling's result (fail-soft, no sibling abort).
        let r = registry();
        let calls = vec![
            call("1", "echo", r#"{"text":"ok"}"#),
            call("2", "fail", "{}"),
        ];
        let results = exec(&r, &calls, &cfg()).await;
        assert_eq!(results[0], ("1".to_string(), "ok".to_string()));
        assert_eq!(results[1].0, "2");
        assert!(
            results[1].1.contains("boom"),
            "tool error fed back, got {:?}",
            results[1].1
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_calls_destructive_is_a_barrier_siblings_still_run() {
        // delete is destructive → a BARRIER (approval gating preserved), but the safe sibling
        // BEFORE it still executes concurrently-eligible; order kept.
        // non-TTY test env → delete safe-denied.
        let r = registry();
        let calls = vec![
            call("1", "echo", r#"{"text":"x"}"#),
            call("2", "delete", "{}"),
        ];
        let results = exec(&r, &calls, &cfg()).await;
        assert_eq!(results.len(), 2);
        assert_eq!(results[0], ("1".to_string(), "x".to_string()));
        assert!(
            results[1].1.contains("declined"),
            "destructive denied non-TTY, got {:?}",
            results[1].1
        );
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
        let c = AgentConfig {
            enable_self_review: true,
            approval_mode: crate::core::approval::ApprovalMode::Yolo,
            ..cfg()
        };
        let mut messages = vec![Message::system("sys"), Message::user("edit something")];
        let chat = scripted(vec![
            tool_turn("delete", "{}"), // a successful destructive op arms made_any_edits
            final_turn("first done"),  // intercepted by the self-review nudge
            final_turn("second done"), // accepted
        ]);
        // Home-stability: a lease failure on the delete call would leave made_any_edits false and
        // the property under test (exactly one review turn) silently unexercised.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(out.final_text.as_deref(), Some("second done"));
        let reviews = messages
            .iter()
            .filter(|m| {
                m.role == "user"
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.starts_with("[self-review]"))
            })
            .count();
        assert_eq!(reviews, 1, "exactly one review turn, never a loop");
        assert_valid_history(&messages);
    }

    #[test]
    fn review_request_uses_newest_real_turn_and_strips_injected_prefixes() {
        let messages = vec![
            Message::user("old request"),
            Message::tool_result("clarify-1", "question"),
            Message::user(
                "<codebase_context>\nretrieved source\n</codebase_context>\n\nRecalled memory\n\noption 2",
            ),
        ];
        let request = capture_review_request(&messages);
        assert!(request.contains("old request"));
        assert!(request.contains("option 2"));
        assert!(!request.contains("retrieved source"));
        assert!(!request.contains("Recalled memory"));
    }

    #[test]
    fn review_blocking_classification() {
        // A [BLOCKING] tag anywhere → gates Done; an all-[ADVISORY] review does not.
        assert!(review_is_blocking(
            "[BLOCKING] src/a.rs:10 off-by-one in the loop bound"
        ));
        assert!(
            review_is_blocking(
                "[ADVISORY] rename foo\n[BLOCKING] src/b.rs:3 missed the null check"
            ),
            "one blocking line among advisories still gates"
        );
        assert!(review_is_blocking("[blocking] lowercase tag still counts"));
        assert!(!review_is_blocking(
            "[ADVISORY] tidy the imports\n[ADVISORY] add a doc comment"
        ));
        assert!(!review_is_blocking("looks fine, just nits"));
    }

    #[tokio::test]
    async fn self_review_skipped_without_edits() {
        let r = registry();
        let c = AgentConfig {
            enable_self_review: true,
            ..cfg()
        };
        let mut messages = vec![Message::system("sys"), Message::user("just a question")];
        let chat = scripted(vec![
            tool_turn("echo", r#"{"text":"look"}"#),
            final_turn("answer"),
        ]);
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(out.final_text.as_deref(), Some("answer"));
        assert!(
            !messages.iter().any(|m| m
                .content
                .as_deref()
                .is_some_and(|c| c.starts_with("[self-review]"))),
            "read-only runs never pay the review turn"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn execute_calls_adopts_eager_handles_by_position() {
        let r = registry();
        let calls = vec![
            call("1", "echo", r#"{"text":"fresh"}"#),
            call("2", "echo", r#"{"text":"normal"}"#),
        ];
        // Position 0 was eagerly started (with a distinguishable payload) — adoption must use it,
        // never re-run the tool.
        let h = tokio::task::spawn_blocking(|| "EAGER_RESULT".to_string());
        let mut sink: Vec<Message> = calls
            .iter()
            .map(|tc| Message::tool_result(tc.id.clone(), INTERRUPTED_TOOL_PLACEHOLDER.to_string()))
            .collect();
        let mut checkpointed = false;
        let mut writer_lease = None;
        let results = execute_calls(
            &r,
            &calls,
            &cfg(),
            &mut sink,
            vec![(0, h)],
            &mut checkpointed,
            &mut writer_lease,
        )
        .await;
        assert_eq!(results[0].1, "EAGER_RESULT", "adopted, not re-executed");
        assert_eq!(results[1].1, "normal", "non-eager sibling runs normally");
        assert_eq!(
            sink[0].content.as_deref(),
            Some("EAGER_RESULT"),
            "sink mirrors the adopted result"
        );
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
        assert!(
            starter(1, &call("2", "delete", "{}")).is_none(),
            "writes never start early"
        );
        // …so a safe call AFTER it must not start either (prefix rule = barrier semantics).
        assert!(
            starter(2, &call("3", "echo", r#"{"text":"b"}"#)).is_none(),
            "post-barrier calls wait"
        );
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
            self.log
                .lock()
                .unwrap()
                .push(format!("{}:start", self.name));
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
        r.register(Box::new(RecordingTool {
            name: "read_a",
            destructive: false,
            log: log.clone(),
            delay_ms: 20,
        }));
        r.register(Box::new(RecordingTool {
            name: "read_b",
            destructive: false,
            log: log.clone(),
            delay_ms: 5,
        }));
        r.register(Box::new(RecordingTool {
            name: "write_w",
            destructive: true,
            log: log.clone(),
            delay_ms: 5,
        }));
        r.register(Box::new(RecordingTool {
            name: "read_c",
            destructive: false,
            log: log.clone(),
            delay_ms: 5,
        }));
        let mut c = cfg();
        c.approval_mode = crate::core::approval::ApprovalMode::Yolo; // clear the write barrier without a prompt
        let calls = vec![
            call("1", "read_a", "{}"),
            call("2", "read_b", "{}"),
            call("3", "write_w", "{}"),
            call("4", "read_c", "{}"),
        ];
        let results = exec(&r, &calls, &c).await;
        assert!(
            results.iter().all(|(_, out)| out.contains("done")),
            "{results:?}"
        );
        let events = log.lock().unwrap().clone();
        let pos = |e: &str| {
            events
                .iter()
                .position(|x| x == e)
                .unwrap_or_else(|| panic!("missing {e} in {events:?}"))
        };
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
        r.register(Box::new(RecordingTool {
            name: "slow_read",
            destructive: false,
            log,
            delay_ms: 400,
        }));
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
        assert_eq!(
            last.role, "tool",
            "history ends in the placeholder tool result"
        );
        assert_eq!(last.content.as_deref(), Some(INTERRUPTED_TOOL_PLACEHOLDER));
    }

    // Turn cancellation is covered with independent tokens above; no test mutates TUI-global state.

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
        let out = run_agent(
            scripted(vec![multi, final_turn("done")]),
            &cfg(),
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
    fn turn_made_edits_only_on_successful_destructive() {
        // The verify gate arms (made_edits=true) only when a destructive tool SUCCEEDED. A
        // read-only turn, an unknown tool, or a denied/errored destructive op must NOT arm.
        let r = registry();
        let res = |id: &str, s: &str| vec![(id.to_string(), s.to_string())];
        assert!(turn_made_edits(
            &r,
            &[call("1", "delete", "{}")],
            &res("1", "deleted")
        ));
        assert!(
            !turn_made_edits(
                &r,
                &[call("1", "delete", "{}")],
                &res("1", "error: the user declined this action")
            ),
            "a denied/errored destructive op must not arm the gate"
        );
        assert!(
            !turn_made_edits(&r, &[call("1", "echo", "{}")], &res("1", "hi")),
            "read-only never arms"
        );
        assert!(
            !turn_made_edits(&r, &[call("1", "nope", "{}")], &res("1", "error: unknown")),
            "unknown never arms"
        );
        // mixed: one denied destructive + one successful destructive → arms
        let calls = vec![call("1", "delete", "{}"), call("2", "delete", "{}")];
        let results = vec![
            ("1".to_string(), "error: declined".to_string()),
            ("2".to_string(), "deleted".to_string()),
        ];
        assert!(turn_made_edits(&r, &calls, &results));
        // W16: a write tool that no-op'd (identical content) wrote nothing → must not arm the gate.
        let noop = format!(
            "{}: f.txt already holds this exact content",
            crate::agent::builtin::NOOP_WRITE_PREFIX
        );
        assert!(
            !turn_made_edits(&r, &[call("1", "delete", "{}")], &res("1", &noop)),
            "a no-op write must not arm the verify gate"
        );
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
        // Gate ENABLED, but the run only reads → made_any_edits stays false → gate never fires (so
        // no `cargo check` subprocess), and the loop reports Done normally.
        let r = registry();
        let c = AgentConfig {
            enable_verify_gate: true,
            quiet: true,
            ..cfg()
        };
        let out = run_agent(
            scripted(vec![
                tool_turn("echo", r#"{"text":"hi"}"#),
                final_turn("done"),
            ]),
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

    // ── GOAL MODE (`/goal <text>`): the real loop paths ─────────────────────────
    // These drive `run_agent_loop` with `cfg.goal = Some(..)` through scripted turns to exercise the
    // ACTUAL goal gate + smart-retry code (not a re-implementation): premature-stop re-poke, the
    // declared+verified handshake reaching Done, empty-200 retry (not treated as done), transient
    // retry-then-succeed, permanent give-up after a bounded count, and clean Esc/cancel. They
    // serialize on `goal::TEST_LOCK` because the completion handshake uses a process-global `PENDING`.

    fn empty_turn() -> ChatTurn {
        // An HTTP-200 with neither content nor a tool call — this provider's most common silent
        // failure. Goal mode must retry it, ordinary mode feeds it to the done cascade.
        ChatTurn {
            content: None,
            tool_calls: vec![],
            finish_reason: Some("stop".into()),
            usage: None,
            eager: Vec::new(),
        }
    }

    /// Like `scripted`, but each queued item is a full `Result` so a test can script `chat()` ERRORS
    /// (goal mode's retry classifies these); exhausting the queue yields a final `Ok("stop")`.
    fn scripted_results(
        items: Vec<Result<ChatTurn>>,
    ) -> impl Fn(Vec<Message>, Vec<ToolDef>) -> std::future::Ready<Result<ChatTurn>> {
        let q = Mutex::new(VecDeque::from(items));
        move |_m, _d| {
            let next = q
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(final_turn("stop")));
            std::future::ready(next)
        }
    }

    fn goal_registry() -> ToolRegistry {
        // The real `goal_complete` tool, so a scripted call actually records the PENDING claim the
        // goal gate then drains — the genuine handshake, not a stub.
        let mut r = registry();
        r.register(Box::new(crate::agent::goal::GoalComplete));
        r
    }

    #[tokio::test]
    async fn goal_gate_pokes_premature_stop_then_done_after_complete() {
        let _g = crate::agent::goal::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::agent::goal::clear();
        let r = goal_registry();
        let c = AgentConfig {
            goal: Some("add a --version flag".into()),
            quiet: true,
            ..cfg()
        };
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        // 1) model stops WITHOUT declaring → must be poked, not Done. 2) it calls goal_complete
        // (records PENDING). 3) it stops → gate drains PENDING → Done.
        let chat = scripted(vec![
            final_turn("I think that's everything"),
            tool_turn("goal_complete", r#"{"summary":"added the flag"}"#),
            final_turn("all done"),
        ]);
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(
            out.stop,
            StopReason::Done,
            "reaches Done only after declared + (no-op) verify"
        );
        let pokes = messages
            .iter()
            .filter(|m| {
                m.role == "user"
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.starts_with(GOAL_POKE_PREFIX))
            })
            .count();
        assert_eq!(pokes, 1, "the one premature stop was poked exactly once");
        assert!(
            crate::agent::goal::take_pending().is_none(),
            "the gate drained the completion claim on the way to Done"
        );
        crate::agent::goal::clear();
    }

    #[tokio::test]
    async fn goal_mode_retries_empty_200_instead_of_treating_it_as_done() {
        let _g = crate::agent::goal::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::agent::goal::clear();
        let r = goal_registry();
        let c = AgentConfig {
            goal: Some("do the thing".into()),
            quiet: true,
            ..cfg()
        };
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        // An empty-200 FIRST: if it were fed to the done cascade the goal gate would poke (take_pending
        // is None). Instead it must be retried silently — so we expect ZERO `[goal]` pokes and a
        // non-empty Done from the real turn that follows.
        let chat = scripted(vec![
            empty_turn(),
            tool_turn("goal_complete", r#"{"summary":"finished"}"#),
            final_turn("done"),
        ]);
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(
            out.final_text.as_deref(),
            Some("done"),
            "returns the real turn, not the empty one"
        );
        let pokes = messages
            .iter()
            .filter(|m| {
                m.role == "user"
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.starts_with(GOAL_POKE_PREFIX))
            })
            .count();
        assert_eq!(
            pokes, 0,
            "empty-200 was retried, never poked as a premature stop"
        );
        crate::agent::goal::clear();
    }

    #[tokio::test]
    async fn goal_mode_retries_transient_chat_error_then_succeeds() {
        let _g = crate::agent::goal::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::agent::goal::clear();
        let r = goal_registry();
        let c = AgentConfig {
            goal: Some("keep going".into()),
            quiet: true,
            ..cfg()
        };
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        // A 5xx (transient) then the real work — goal mode must survive it (ordinary mode would return
        // Err here, see `normal_mode_chat_error_is_fatal`).
        let chat = scripted_results(vec![
            Err(anyhow::anyhow!(
                "upstream returned HTTP 503 Service Unavailable: overloaded"
            )),
            Ok(tool_turn(
                "goal_complete",
                r#"{"summary":"done despite the blip"}"#,
            )),
            Ok(final_turn("finished")),
        ]);
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(
            out.stop,
            StopReason::Done,
            "a transient error is retried, not fatal"
        );
        crate::agent::goal::clear();
    }

    #[tokio::test]
    async fn goal_mode_gives_up_after_bounded_permanent_retries() {
        let _g = crate::agent::goal::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::agent::goal::clear();
        let r = goal_registry();
        let c = AgentConfig {
            goal: Some("nope".into()),
            quiet: true,
            ..cfg()
        };
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        // A permanent 401 on every call: goal mode retries a small bounded number of times then
        // SURFACES the error (it can't be fixed by retrying) — the "retry a few times then stop"
        // product decision. Contrast with the transient case above which retries indefinitely.
        let chat = scripted_results(vec![
            Err(anyhow::anyhow!(
                "upstream returned HTTP 401 Unauthorized: bad key"
            )),
            Err(anyhow::anyhow!(
                "upstream returned HTTP 401 Unauthorized: bad key"
            )),
            Err(anyhow::anyhow!(
                "upstream returned HTTP 401 Unauthorized: bad key"
            )),
            Err(anyhow::anyhow!(
                "upstream returned HTTP 401 Unauthorized: bad key"
            )),
            Err(anyhow::anyhow!(
                "upstream returned HTTP 401 Unauthorized: bad key"
            )),
        ]);
        let res = run_agent_loop(chat, &c, &r, &mut messages).await;
        assert!(
            res.is_err(),
            "a permanent client error stops the run after the bounded retries"
        );
        crate::agent::goal::clear();
    }

    #[tokio::test]
    async fn normal_mode_chat_error_is_fatal_and_no_goal_tool() {
        // The control: with goal mode OFF, a chat error stays fatal (the old behavior is unchanged),
        // proving the retry logic is gated strictly on `cfg.goal`.
        let r = registry();
        let c = cfg(); // goal: None
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        let chat = scripted_results(vec![Err(anyhow::anyhow!(
            "upstream returned HTTP 503: overloaded"
        ))]);
        let res = run_agent_loop(chat, &c, &r, &mut messages).await;
        assert!(res.is_err(), "ordinary turns keep the fatal-on-error path");
        assert_eq!(
            c.max_transient_retries, 0,
            "the top-level default is what keeps it fatal"
        );
        assert_eq!(
            AgentConfig::default().max_transient_retries,
            0,
            "production top-level config agrees with the test cfg"
        );
    }

    #[tokio::test]
    async fn delegated_loop_absorbs_bounded_transient_errors_but_not_permanent_ones() {
        // A DELEGATED loop (task/workflow child) sets max_transient_retries > 0: nobody is watching
        // it, so one 429/5xx must not discard the steps it already completed. Goal mode is OFF here —
        // this is the ordinary path, proving the retry is driven by the budget, not by `cfg.goal`.
        let r = registry();
        let c = AgentConfig {
            max_transient_retries: 4,
            quiet: true,
            ..cfg()
        };

        // Two transient blips then real work → survives, and the work still lands.
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        let chat = scripted_results(vec![
            Err(anyhow::anyhow!(
                "upstream returned HTTP 503 Service Unavailable: overloaded"
            )),
            Err(anyhow::anyhow!("request failed after retries")),
            Ok(final_turn("survived the blips")),
        ]);
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(out.final_text.as_deref(), Some("survived the blips"));

        // A PERMANENT 4xx is NOT retried even with budget left — retrying can't fix a bad key, and
        // burning backoff on it only delays the report. One call, then the error.
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        let counting = move |_m: Vec<Message>, _d: Vec<ToolDef>| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            std::future::ready(Err(anyhow::anyhow!(
                "upstream returned HTTP 401 Unauthorized: bad key"
            )))
        };
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        assert!(run_agent_loop(counting, &c, &r, &mut messages)
            .await
            .is_err());
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "permanent → no retry"
        );

        // The budget is a CEILING: transient errors past it are still fatal (no infinite retry — that
        // is goal mode's contract, not a sub-agent's).
        let c2 = AgentConfig {
            max_transient_retries: 1,
            quiet: true,
            ..cfg()
        };
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        let chat = scripted_results(vec![
            Err(anyhow::anyhow!("upstream returned HTTP 503: overloaded")),
            Err(anyhow::anyhow!("upstream returned HTTP 503: overloaded")),
            Ok(final_turn("never reached")),
        ]);
        assert!(
            run_agent_loop(chat, &c2, &r, &mut messages).await.is_err(),
            "one retry allowed, the second failure is fatal"
        );
    }

    #[tokio::test]
    async fn esc_during_a_delegated_transient_retry_returns_cancelled() {
        // Esc must escape the new retry backoff cleanly (Cancelled, not a swallowed error).
        let r = registry();
        let c = AgentConfig {
            max_transient_retries: 4,
            quiet: true,
            ..cfg()
        };
        c.cancel.cancel();
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        let chat = scripted_results(vec![Err(anyhow::anyhow!(
            "upstream returned HTTP 503: overloaded"
        ))]);
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(out.stop, StopReason::Cancelled);
    }

    #[tokio::test]
    async fn goal_mode_esc_during_retry_returns_cancelled() {
        let _g = crate::agent::goal::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        crate::agent::goal::clear();
        let r = goal_registry();
        let c = AgentConfig {
            goal: Some("interrupt me".into()),
            quiet: true,
            ..cfg()
        };
        c.cancel.cancel(); // Esc already pressed → the retry loop's `cancel::race` must bail cleanly.
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        let chat = scripted(vec![empty_turn(), final_turn("never reached")]);
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(
            out.stop,
            StopReason::Cancelled,
            "Esc exits goal mode as Cancelled, not Done"
        );
        crate::agent::goal::clear();
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
    fn relevance_keywords_tokenizes_and_filters() {
        let k = relevance_keywords("How does the RetryPolicy backoff work?");
        assert!(k.contains(&"retrypolicy".to_string()));
        assert!(k.contains(&"backoff".to_string()));
        assert!(
            !k.iter().any(|t| t.chars().count() < 3),
            "short tokens dropped"
        );
        // URL stopwords are filtered so a url source doesn't dilute scoring.
        let ku = relevance_keywords("https://docs.rs/tokio/latest/tokio/task");
        assert!(!ku.contains(&"https".to_string()) && !ku.contains(&"www".to_string()));
        assert!(ku.contains(&"tokio".to_string()) && ku.contains(&"task".to_string()));
    }

    #[test]
    fn truncate_relevant_keeps_the_matching_region() {
        // A long doc where the ONLY mention of the query term is in the MIDDLE — blind head+tail
        // would drop it; relevance truncation must keep it.
        let mut lines: Vec<String> = (0..200)
            .map(|i| format!("filler line number {i} lorem ipsum"))
            .collect();
        lines[100] = "the CriticalSetting flag toggles the special behavior here".to_string();
        let doc = lines.join("\n");
        let kw = relevance_keywords("CriticalSetting");
        let out = truncate_relevant(&doc, 400, &kw);
        assert!(
            out.contains("CriticalSetting"),
            "the matching region must survive: {out}"
        );
        assert!(
            out.chars().count() <= 400 + 120,
            "stays near the budget (+markers)"
        );
    }

    #[test]
    fn truncate_relevant_degrades_to_head_tail_without_signal() {
        // No keyword match anywhere → must behave EXACTLY like the old head+tail truncation.
        let s = "a".repeat(300) + &"b".repeat(300);
        let kw = relevance_keywords("nonexistentzzz");
        let rel = truncate_relevant(&s, 120, &kw);
        let plain = truncate_result(&s, 120);
        assert_eq!(
            rel, plain,
            "no signal must degrade to the exact old behavior"
        );
    }

    #[test]
    fn truncate_relevant_passthrough_when_within_budget() {
        let s = "short enough";
        assert_eq!(truncate_relevant(s, 4096, &["short".to_string()]), s);
    }

    #[test]
    fn run_tool_body_gives_relevance_truncatable_tools_the_larger_fetch_budget() {
        // W22: a read/fetch tool's output must be measured against `max_fetch_chars`, NOT the
        // smaller `max_chars` — otherwise the reach layer's 20k fetch gets halved to 4k before
        // relevance-truncation ever sees the full document (the double-cut the plan calls out).
        let long_tool = std::sync::Arc::new(LongReadTool) as std::sync::Arc<dyn Tool>;
        let small_budget = 200usize;
        let large_budget = 6000usize;
        let out = run_tool_body(
            long_tool,
            &serde_json::json!({}),
            true,
            small_budget,
            large_budget,
        );
        assert!(
            out.chars().count() > small_budget,
            "file_read output should use the larger fetch budget, not the small default: got {} chars",
            out.chars().count()
        );
        assert!(
            out.chars().count() <= large_budget + 200,
            "still bounded by the larger budget"
        );

        // A non-truncatable tool (positional output) stays at the SMALL budget regardless.
        let konst_out = "x".repeat(1000);
        let plain = truncate_result(&konst_out, small_budget);
        assert!(
            plain.chars().count() <= small_budget + 40,
            "non-fetch tools keep the small budget"
        );
    }

    #[test]
    fn relevance_query_from_args_pulls_known_keys() {
        let a = serde_json::json!({"pattern": "RetryPolicy", "glob": "*.rs"});
        assert_eq!(relevance_query_from_args(&a), "RetryPolicy");
        let u = serde_json::json!({"url": "https://x.com/tokio"});
        assert_eq!(relevance_query_from_args(&u), "https://x.com/tokio");
        assert_eq!(
            relevance_query_from_args(&serde_json::json!({"other": 1})),
            ""
        );
    }

    #[test]
    fn is_relevance_truncatable_matches_read_fetch_only() {
        assert!(is_relevance_truncatable("file_read"));
        assert!(is_relevance_truncatable("web_fetch"));
        assert!(is_relevance_truncatable("search_files"));
        assert!(
            !is_relevance_truncatable("file_edit"),
            "edit output is positional"
        );
        assert!(
            !is_relevance_truncatable("shell_run"),
            "shell log is positional"
        );
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
                let id = m
                    .tool_call_id
                    .as_deref()
                    .expect("tool message carries tool_call_id");
                assert!(
                    declared.contains(id),
                    "tool result '{id}' has no preceding assistant tool_call"
                );
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
        assert!(
            stats.chars_reclaimed > 1900,
            "reclaimed most of the 2000-char body: {stats:?}"
        );
        assert_eq!(stats.cleared, 1);
        assert_eq!(stats.failures_trimmed, 0);
        assert_eq!(
            msgs[3].content.as_deref(),
            Some(CLEARED_TOOL_PLACEHOLDER),
            "old result cleared"
        );
        assert_eq!(
            msgs[3].tool_call_id.as_deref(),
            Some("1"),
            "tool_call_id preserved (no orphan)"
        );
        assert_eq!(
            msgs[5].content.as_deref(),
            Some(big.as_str()),
            "recent result kept verbatim"
        );
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
        assert_eq!(
            msgs[1].content.as_deref(),
            Some("tiny"),
            "small result untouched"
        );
        // Running again reclaims nothing (cleared bodies are shorter than min_chars).
        let second = clear_tool_results_to_floor(&mut msgs, 1, 1024, 0, 0);
        assert_eq!(second, ClearStats::default(), "idempotent — no re-clearing");
    }

    #[test]
    fn clear_to_floor_noop_when_within_keep_window() {
        let mut msgs = vec![
            Message::system("sys"),
            Message::tool_result("1", "x".repeat(2000)),
        ];
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
        assert_eq!(
            stats.cleared, 2,
            "oldest-first until ≤ target, then stop: {stats:?}"
        );
        assert_eq!(
            msgs[4].content.as_deref().map(|c| c.len()),
            Some(4000),
            "third success untouched"
        );
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
        assert_eq!(
            stats.failures_trimmed, 0,
            "failures survive pass 1 untouched"
        );
        assert_eq!(msgs[2].content.as_deref(), Some(fail_body.as_str()));
        assert_eq!(msgs[4].content.as_deref(), Some(exit_fail.as_str()));
        // Target 0: now failures must be TRIMMED (first line + sentinel), never blanked.
        let stats2 = clear_tool_results_to_floor(&mut msgs, 1, 1024, 0, 0);
        assert_eq!(stats2.failures_trimmed, 2, "{stats2:?}");
        let t1 = msgs[2].content.as_deref().unwrap();
        assert!(
            t1.starts_with("error: build failed"),
            "first line survives: {t1}"
        );
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
    fn result_is_failure_consults_tool_then_heuristic() {
        // W12: a tool that self-declares failure (SelfErrTool) overrides the generic heuristic even
        // though its Ok body has no `error:`/`exit N` shape; a tool returning None defers to it.
        let r = registry();
        let err_body = "{\"isError\":true,\"detail\":\"x\"}";
        assert!(
            result_is_failure(&r, "selferr", err_body),
            "self-declared failure must count"
        );
        assert!(
            !result_is_failure(&r, "selferr", "{\"isError\":false}"),
            "self-declared OK must not"
        );
        // A tool that doesn't override (echo → None) falls back to the heuristic.
        assert!(
            result_is_failure(&r, "echo", "error: boom"),
            "None defers to heuristic (error:)"
        );
        assert!(
            !result_is_failure(&r, "echo", "plain contents"),
            "None defers to heuristic (ok)"
        );
        // Unknown tool → pure heuristic.
        assert!(result_is_failure(&r, "ghost", "exit 3\n"));
    }

    #[test]
    fn push_nudge_collapses_same_kind_keeps_others() {
        let mut msgs = vec![Message::system("sys prompt"), Message::user("task")];
        push_nudge(
            &mut msgs,
            NUDGE_DIVERGENCE,
            "You repeated the same tool call(s) — v1",
        );
        msgs.push(Message::user("more work"));
        push_nudge(
            &mut msgs,
            NUDGE_DIVERGENCE,
            "You repeated the same tool call(s) — v2",
        );
        let divergence = msgs
            .iter()
            .filter(|m| {
                m.role == "system"
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.starts_with(NUDGE_DIVERGENCE))
            })
            .count();
        assert_eq!(divergence, 1, "same-kind nudges collapse to the newest");
        assert!(
            msgs.last()
                .unwrap()
                .content
                .as_deref()
                .unwrap()
                .ends_with("v2"),
            "newest wins, at the tail"
        );
        assert_eq!(
            msgs[0].content.as_deref(),
            Some("sys prompt"),
            "system prompt never touched"
        );
        // A DIFFERENT kind is additive, and doesn't disturb the existing one.
        push_nudge(&mut msgs, NUDGE_CONTEXT, "Context is nearly full — wrap up");
        assert!(msgs.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|c| c.starts_with(NUDGE_DIVERGENCE))));
        assert!(msgs.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|c| c.starts_with(NUDGE_CONTEXT))));
    }

    #[test]
    fn push_nudge_never_touches_index_zero_even_if_matching() {
        // Pathological: a system PROMPT that happens to start with a nudge prefix must survive.
        let mut msgs = vec![Message::system(
            "Context is nearly full — just kidding, SYSTEM PROMPT",
        )];
        push_nudge(
            &mut msgs,
            NUDGE_CONTEXT,
            "Context is nearly full (~90%) — wrap up",
        );
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0]
            .content
            .as_deref()
            .unwrap()
            .contains("SYSTEM PROMPT"));
    }

    #[test]
    fn budget_band_floors_below_50_and_deciles_above() {
        // P-ctx1: no band below 50% (don't nag on an empty window), then one band per decile.
        assert_eq!(budget_band(0, 200_000), None);
        assert_eq!(budget_band(99_000, 200_000), None); // 49% → still quiet
        assert_eq!(budget_band(100_000, 200_000), Some(5)); // 50%
        assert_eq!(budget_band(126_000, 200_000), Some(6)); // 63% → decile 6
        assert_eq!(budget_band(199_999, 200_000), Some(9)); // 99%
        assert_eq!(budget_band(200_000, 200_000), Some(10)); // full
        assert_eq!(budget_band(999_999, 200_000), Some(10)); // clamps at 100%, never overflows
        assert_eq!(budget_band(500, 0), None); // disabled window is never a band
    }

    #[test]
    fn budget_nudge_text_is_prefixed_and_reports_remaining() {
        // Must start with NUDGE_BUDGET so push_nudge collapses the prior one, and carry the honest
        // remaining figure the model plans against.
        let t = budget_nudge_text(150_000, 200_000);
        assert!(
            t.starts_with(NUDGE_BUDGET),
            "prefix drives push_nudge collapse: {t}"
        );
        assert!(
            t.contains("50.0K remaining"),
            "remaining = window - est: {t}"
        );
        assert!(t.contains("25% left"), "pct left: {t}");
        // Over-budget never underflows the remaining figure.
        let over = budget_nudge_text(210_000, 200_000);
        assert!(
            over.contains("0 remaining") && over.contains("0% left"),
            "saturating: {over}"
        );
    }

    #[tokio::test]
    async fn running_budget_nudge_appears_once_per_band_and_survives_wrapup() {
        // With a small window, a few large results push usage across bands; the budget nudge must
        // appear (collapsed to ONE, newest wins) and NOT be popped when the 90% wrap-up nudge lands
        // on top of it (the wrap-up is the tail; error-rollback pops only that).
        let r = registry();
        let mut c = cfg();
        c.context_window = 1000; // tiny window so a couple of turns cross 50%/90%
        c.clear_at_pct = 0; // disable clearing so history only grows (isolate the budget signal)
        let big = "X".repeat(3000); // ~750 tok result → crosses bands fast
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        let chat = scripted(vec![
            tool_turn("echo", &format!(r#"{{"text":"{big}"}}"#)),
            tool_turn("echo", &format!(r#"{{"text":"{big}"}}"#)),
            final_turn("done"),
        ]);
        let _ = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        let budget_msgs = messages
            .iter()
            .filter(|m| {
                m.role == "system"
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.starts_with(NUDGE_BUDGET))
            })
            .count();
        assert_eq!(
            budget_msgs, 1,
            "the running budget nudge collapses to a single message"
        );
    }

    #[tokio::test]
    async fn save_before_clear_warns_first_then_evicts_next_turn() {
        // P-ctx2: the FIRST time clearing is due, the loop must WARN (so the model can persist) and
        // NOT evict yet — the old result bodies are still in context that turn. The eviction happens
        // on a LATER turn. Assert both: the warning appears, and at least one bulky result gets
        // blanked to the placeholder by the end (proving the deferral didn't disable clearing).
        let r = registry();
        let mut c = cfg();
        c.max_iters = 10;
        c.auto_extend_to = 10;
        c.context_window = 1200; // tiny window
        c.clear_at_pct = 40; // arm early
        c.clear_target_pct = 20;
        c.clear_step_pct = 1; // cadence trivially satisfied so the deferred pass re-fires next turn
        c.clear_cooldown_iters = 0;
        c.keep_recent_tool_results = 1; // keep only the newest → older bulky ones are clearable
        c.clear_tool_result_min_chars = 100;
        let big = "Y".repeat(2400); // ~600 tok each → a couple crosses the 40% arm
        let turns: Vec<ChatTurn> = (0..6)
            .map(|_| tool_turn("echo", &format!(r#"{{"text":"{big}"}}"#)))
            .collect();
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        let _ = run_agent_loop(scripted(turns), &c, &r, &mut messages)
            .await
            .unwrap();
        let warned = messages.iter().any(|m| {
            m.role == "system"
                && m.content
                    .as_deref()
                    .is_some_and(|c| c.starts_with(NUDGE_SAVE_BEFORE_CLEAR))
        });
        assert!(
            warned,
            "the save-before-clear warning must be injected before eviction"
        );
        let evicted = messages
            .iter()
            .any(|m| m.role == "tool" && m.content.as_deref() == Some(CLEARED_TOOL_PLACEHOLDER));
        assert!(
            evicted,
            "clearing must still happen (a later turn) — the warning only defers, not disables"
        );
        // And the warning is one-shot: exactly one such system message.
        let warn_count = messages
            .iter()
            .filter(|m| {
                m.role == "system"
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.starts_with(NUDGE_SAVE_BEFORE_CLEAR))
            })
            .count();
        assert_eq!(
            warn_count, 1,
            "the save-before-clear warning fires at most once per run"
        );
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
            .filter(|m| {
                m.role == "system"
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.starts_with(NUDGE_DIVERGENCE))
            })
            .count();
        assert_eq!(
            nudges, 1,
            "two invocations, ONE divergence nudge (collapsed, not accreted)"
        );
        assert_valid_history(&messages);
    }

    #[tokio::test]
    async fn todo_recitation_fires_replaces_and_respects_cadence() {
        let _g = todo::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        todo::set(vec![
            todo::Todo::new("map the module", todo::Status::Done),
            todo::Todo::new("fix the parser", todo::Status::InProgress),
            todo::Todo::new("run the tests", todo::Status::Pending),
        ]);
        let r = registry();
        let c = AgentConfig {
            todo_reminder_every: 2,
            ..cfg()
        };
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
            .filter(|m| {
                m.role == "system"
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.starts_with(NUDGE_TODO))
            })
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
            !messages2.iter().any(|m| m
                .content
                .as_deref()
                .is_some_and(|c| c.starts_with(NUDGE_TODO))),
            "no todos → no recitation"
        );
    }

    #[tokio::test]
    async fn todo_poke_blocks_done_with_pending() {
        let _g = todo::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        todo::set(vec![
            todo::Todo::new("done-bit", todo::Status::Done),
            todo::Todo::new("still-open", todo::Status::Pending),
        ]);
        let r = registry();
        let c = AgentConfig {
            enable_todo_poke: true,
            max_todo_poke_attempts: 2,
            max_iters: 6,
            auto_extend_to: 6,
            ..cfg()
        };
        let mut messages = vec![Message::system("sys"), Message::user("multi-step")];
        // First text-only → poke; second text-only after model "finishes" todos in real life would
        // pass — here we clear between by scripting a tool that doesn't clear; instead second
        // final still incomplete → second poke; third final with list still open but budget=2 → Done.
        let chat = scripted(vec![
            final_turn("done early"),
            final_turn("still done"),
            final_turn("forced done"),
        ]);
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(out.stop, StopReason::Done);
        let pokes = messages
            .iter()
            .filter(|m| {
                m.role == "user"
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.starts_with(TODO_POKE_PREFIX))
            })
            .count();
        assert_eq!(
            pokes, 2,
            "exactly max_todo_poke_attempts pokes, then Done: {pokes}"
        );
        assert!(
            messages.iter().any(|m| {
                m.role == "user"
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.contains("[ ] still-open"))
            }),
            "poke lists the open item"
        );
        todo::clear();
    }

    /// Steering is a process-global mailbox (the keyboard thread has no handle on the running turn),
    /// so these tests must not interleave with each other.
    fn steer_guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::OnceLock<Mutex<()>> = std::sync::OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[tokio::test]
    async fn steer_is_folded_into_the_running_turn_at_the_next_iteration() {
        let _g = steer_guard();
        let r = registry();
        let c = AgentConfig {
            enable_steering: true,
            max_iters: 6,
            auto_extend_to: 6,
            ..cfg()
        };
        let mut messages = vec![Message::system("sys"), Message::user("original task")];
        // A tool turn, then a final answer. The steer is posted BEFORE the loop runs, so it is drained
        // at the very first iteration boundary — the same path a mid-flight Alt+Enter takes.
        crate::core::steer::arm();
        assert!(crate::core::steer::push("also update the README"));
        let out = run_agent_loop(
            scripted(vec![
                tool_turn("echo", r#"{"text":"hi"}"#),
                final_turn("done"),
            ]),
            &c,
            &r,
            &mut messages,
        )
        .await
        .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        let injected: Vec<&Message> = messages
            .iter()
            .filter(|m| {
                m.role == "user"
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.starts_with(crate::core::steer::PREFIX))
            })
            .collect();
        assert_eq!(
            injected.len(),
            1,
            "the steer is injected exactly once, not re-delivered"
        );
        assert!(injected[0]
            .content
            .as_deref()
            .unwrap()
            .contains("also update the README"));
        // The ORIGINAL task survives: steering augments the run, it does not replace history.
        assert!(messages
            .iter()
            .any(|m| m.content.as_deref() == Some("original task")));
        assert_eq!(crate::core::steer::pending(), 0, "drained");
        let _ = crate::core::steer::disarm();
    }

    #[tokio::test]
    async fn steer_arriving_at_the_final_answer_blocks_done_and_grants_another_turn() {
        let _g = steer_guard();
        let r = registry();
        let c = AgentConfig {
            enable_steering: true,
            max_iters: 6,
            auto_extend_to: 6,
            ..cfg()
        };
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        // The steer must land INSIDE the model call — after the top-of-loop drain, while the answer is
        // being composed. That is the gap the Done gate exists to close: without it the mailbox would
        // still be full at `return`, and `disarm` would defer the correction to a whole new turn,
        // arriving after the work it meant to redirect was already reported finished. Pushing from the
        // fake model reproduces that window exactly (pushing before the loop would be drained at the
        // first boundary instead — that is the other test).
        crate::core::steer::arm();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let chat = move |_m: Vec<Message>, _d: Vec<ToolDef>| {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if n == 0 {
                assert!(crate::core::steer::push("wait, use tabs not spaces"));
                std::future::ready(Ok(final_turn("all done")))
            } else {
                std::future::ready(Ok(final_turn("adjusted, now really done")))
            }
        };
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(
            out.final_text.as_deref(),
            Some("adjusted, now really done"),
            "the extra turn ran"
        );
        assert!(
            messages.iter().any(|m| {
                m.role == "user"
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.contains("wait, use tabs not spaces"))
            }),
            "the late steer reached the model rather than being dropped"
        );
        // The premature final text is recorded so the injected correction reads coherently.
        assert!(messages
            .iter()
            .any(|m| m.role == "assistant" && m.content.as_deref() == Some("all done")));
        let _ = crate::core::steer::disarm();
    }

    #[tokio::test]
    async fn steering_is_ignored_when_disabled_so_a_steer_cannot_leak_into_a_subagent() {
        let _g = steer_guard();
        let r = registry();
        // Sub-agents / workflow children keep the default (false): a steer aimed at the top-level task
        // must not be swallowed by whatever a delegated child happens to be doing.
        let c = cfg();
        assert!(!c.enable_steering, "steering is opt-in — default OFF");
        let mut messages = vec![Message::system("sys"), Message::user("child task")];
        crate::core::steer::arm();
        assert!(crate::core::steer::push("meant for the parent"));
        let out = run_agent_loop(
            scripted(vec![final_turn("child done")]),
            &c,
            &r,
            &mut messages,
        )
        .await
        .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert!(
            !messages.iter().any(|m| m
                .content
                .as_deref()
                .is_some_and(|c| c.starts_with(crate::core::steer::PREFIX))),
            "a child never consumes the steering mailbox"
        );
        assert_eq!(
            crate::core::steer::pending(),
            1,
            "left intact for the parent loop"
        );
        let leftover = crate::core::steer::disarm();
        assert_eq!(leftover, vec!["meant for the parent".to_string()]);
    }

    #[tokio::test]
    async fn todo_poke_allows_done_when_all_done() {
        let _g = todo::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        todo::set(vec![todo::Todo::new("only", todo::Status::Done)]);
        let r = registry();
        let c = AgentConfig {
            enable_todo_poke: true,
            max_todo_poke_attempts: 2,
            ..cfg()
        };
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        let out = run_agent_loop(
            scripted(vec![final_turn("all good")]),
            &c,
            &r,
            &mut messages,
        )
        .await
        .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(out.iters, 1);
        assert!(
            !messages.iter().any(|m| m
                .content
                .as_deref()
                .is_some_and(|c| c.starts_with(TODO_POKE_PREFIX))),
            "no poke when all todos done"
        );
        todo::clear();
    }

    #[tokio::test]
    async fn todo_poke_disabled_or_empty_list() {
        let _g = todo::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Pending todos but poke OFF → Done immediately.
        todo::set(vec![todo::Todo::new("open", todo::Status::Pending)]);
        let r = registry();
        let c = AgentConfig {
            enable_todo_poke: false,
            ..cfg()
        };
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        let out = run_agent_loop(scripted(vec![final_turn("bye")]), &c, &r, &mut messages)
            .await
            .unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert_eq!(out.iters, 1);

        // Empty list + poke ON → Done.
        todo::clear();
        let c2 = AgentConfig {
            enable_todo_poke: true,
            max_todo_poke_attempts: 2,
            ..cfg()
        };
        let mut messages2 = vec![Message::system("sys"), Message::user("task")];
        let out2 = run_agent_loop(scripted(vec![final_turn("bye")]), &c2, &r, &mut messages2)
            .await
            .unwrap();
        assert_eq!(out2.stop, StopReason::Done);
        assert!(!messages2.iter().any(|m| m
            .content
            .as_deref()
            .is_some_and(|c| c.starts_with(TODO_POKE_PREFIX))));
    }

    #[tokio::test]
    async fn confidence_spike_arms_one_shot_gate() {
        let _g = todo::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        todo::clear();
        let mut r = registry();
        r.register(Box::new(todo::TodoWrite));
        let c = AgentConfig {
            enable_todo_poke: false, // isolate confidence gate
            enable_confidence_gate: true,
            conf_high: 90,
            conf_spike_delta: 40,
            max_iters: 8,
            auto_extend_to: 8,
            ..cfg()
        };
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        let chat = scripted(vec![
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
        ]);
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(out.stop, StopReason::Done);
        let gates = messages
            .iter()
            .filter(|m| {
                m.role == "user"
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.starts_with(CONFIDENCE_GATE_PREFIX))
            })
            .count();
        assert_eq!(gates, 1, "confidence gate fires exactly once");
        todo::clear();
    }

    #[tokio::test]
    async fn confidence_stepwise_or_omitted_no_gate() {
        let _g = todo::TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        todo::clear();
        let mut r = registry();
        r.register(Box::new(todo::TodoWrite));
        let c = AgentConfig {
            enable_todo_poke: false,
            enable_confidence_gate: true,
            conf_high: 90,
            conf_spike_delta: 40,
            max_iters: 8,
            auto_extend_to: 8,
            ..cfg()
        };
        // Stepwise 40→70→95: no single jump ≥40 into ≥90 from prev.
        // Actually 70→95 is +25 < 40; 40→70 is +30. No gate.
        let mut messages = vec![Message::system("sys"), Message::user("task")];
        let chat = scripted(vec![
            tool_turn(
                "todo_write",
                r#"{"todos":[{"content":"ship","status":"in_progress","confidence":40}]}"#,
            ),
            tool_turn(
                "todo_write",
                r#"{"todos":[{"content":"ship","status":"in_progress","confidence":70}]}"#,
            ),
            tool_turn(
                "todo_write",
                r#"{"todos":[{"content":"ship","status":"done","confidence":95}]}"#,
            ),
            final_turn("done"),
        ]);
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert!(
            !messages.iter().any(|m| {
                m.role == "user"
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.starts_with(CONFIDENCE_GATE_PREFIX))
            }),
            "stepwise rises must not arm the gate"
        );

        // Omitted confidence → no gate.
        todo::clear();
        let mut messages2 = vec![Message::system("sys"), Message::user("task")];
        let chat2 = scripted(vec![
            tool_turn(
                "todo_write",
                r#"{"todos":[{"content":"x","status":"done"}]}"#,
            ),
            final_turn("done"),
        ]);
        run_agent_loop(chat2, &c, &r, &mut messages2).await.unwrap();
        assert!(!messages2.iter().any(|m| {
            m.role == "user"
                && m.content
                    .as_deref()
                    .is_some_and(|c| c.starts_with(CONFIDENCE_GATE_PREFIX))
        }));
        todo::clear();
    }

    #[tokio::test]
    async fn hill_climb_keyword_reframe() {
        let r = registry();
        let c = AgentConfig {
            enable_hill_climb: true,
            hill_climb_reminder_every: 0, // reframe only
            max_iters: 6,
            auto_extend_to: 6,
            ..cfg()
        };
        let mut messages = vec![
            Message::system("sys"),
            Message::user("please optimize the float-print hot path"),
        ];
        let chat = scripted(vec![
            tool_turn("echo", r#"{"text":"measure baseline"}"#),
            final_turn("improved"),
        ]);
        let out = run_agent_loop(chat, &c, &r, &mut messages).await.unwrap();
        assert_eq!(out.stop, StopReason::Done);
        assert!(
            messages.iter().any(|m| {
                m.role == "system"
                    && m.content
                        .as_deref()
                        .is_some_and(|c| c.starts_with(NUDGE_HILL_CLIMB))
            }),
            "hill-climb reframe must inject after first tool turn"
        );
    }

    #[test]
    fn task_looks_hill_climbable_keywords() {
        let msgs = vec![
            Message::system("sys"),
            Message::user("please optimize latency in the parser"),
        ];
        assert!(task_looks_hill_climbable(&msgs));
        let msgs2 = vec![Message::system("sys"), Message::user("rename the helper")];
        assert!(!task_looks_hill_climbable(&msgs2));
    }

    #[test]
    fn update_confidence_tracking_spike_and_stepwise() {
        let mut map = std::collections::HashMap::new();
        let mut armed = false;
        let items1 = vec![todo::Todo {
            content: "t".into(),
            status: todo::Status::InProgress,
            confidence: Some(40),
            hill_climbable: None,
        }];
        update_confidence_tracking(&items1, &mut map, &mut armed, 90, 40);
        assert!(!armed);
        let items2 = vec![todo::Todo {
            content: "t".into(),
            status: todo::Status::Done,
            confidence: Some(100),
            hill_climbable: None,
        }];
        update_confidence_tracking(&items2, &mut map, &mut armed, 90, 40);
        assert!(armed, "40→100 at Done must arm");

        let mut map2 = std::collections::HashMap::new();
        let mut armed2 = false;
        update_confidence_tracking(
            &[todo::Todo {
                content: "t".into(),
                status: todo::Status::InProgress,
                confidence: Some(40),
                hill_climbable: None,
            }],
            &mut map2,
            &mut armed2,
            90,
            40,
        );
        update_confidence_tracking(
            &[todo::Todo {
                content: "t".into(),
                status: todo::Status::Done,
                confidence: Some(70),
                hill_climbable: None,
            }],
            &mut map2,
            &mut armed2,
            90,
            40,
        );
        assert!(!armed2, "70 < conf_high → no arm");
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
        assert!(
            p1.starts_with(system_base().trim_end()),
            "static base must lead the prompt"
        );
        assert!(p1.contains("cwd: /a"));
        // check the INJECTED block (the base prose also mentions <user_memory>).
        assert!(p1.contains("\n<user_memory>\n") && p1.contains("- terse"));
        // empty frozen core → no injected user_memory block
        let p2 = build_system_prompt("/a", "linux", "2026-06-20", "m", Some("   "));
        assert!(!p2.contains("\n<user_memory>\n"));
        // static prefix byte-identical regardless of dynamic inputs (prefix-cache safety)
        let p3 = build_system_prompt("/b", "macos", "2026-01-01", "n", None);
        assert!(p3.starts_with(system_base().trim_end()));
    }

    #[test]
    fn prompt_lanes_stable_byte_identical_when_only_memory_changes() {
        // Lane invariant: the frozen core (user memory) lives in the DYNAMIC lane, so changing it
        // must leave the STABLE lane byte-identical — otherwise the provider prefix cache is busted
        // every time a fact is learned. Only the dynamic lane may differ.
        let a = build_system_prompt_bundle("/w", "linux", "2026-07-20", "m", Some("- fact one"));
        let b = build_system_prompt_bundle("/w", "linux", "2026-07-20", "m", Some("- fact two"));
        assert_eq!(
            a.stable, b.stable,
            "stable lane must not change when only memory changes"
        );
        assert_ne!(
            a.dynamic, b.dynamic,
            "the differing memory lands in the dynamic lane"
        );
        assert!(a.dynamic.contains("fact one") && b.dynamic.contains("fact two"));
        // The environment (cwd/os/date/model) is what the stable lane carries.
        assert!(a.stable.contains("cwd: /w") && a.stable.contains("model: m"));
    }

    #[tokio::test]
    async fn clarify_yields_awaiting_input_with_valid_history() {
        // The load-bearing wiring: a `clarify` call PAUSES the loop — it returns AwaitingInput
        // carrying the question, having left a valid (resumable) history ending in the tool result.
        let _g = crate::agent::clarify::TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
                assert!(
                    q.contains("1. A") && q.contains("2. B"),
                    "carries the options: {q}"
                );
            }
            other => panic!("expected AwaitingInput, got {other:?}"),
        }
        // Resumable: last message is the clarify tool result (the user's next turn continues from
        // here), and the loop drained the pending cell.
        assert_eq!(
            messages.last().unwrap().role,
            "tool",
            "history ends in the tool result"
        );
        assert!(
            crate::agent::clarify::take_pending().is_none(),
            "loop drained the pending cell"
        );
    }

    #[test]
    fn signature_is_order_insensitive() {
        let a = vec![
            ToolCall {
                id: "1".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "x".into(),
                    arguments: "{}".into(),
                },
            },
            ToolCall {
                id: "2".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "y".into(),
                    arguments: "{}".into(),
                },
            },
        ];
        let b = vec![
            ToolCall {
                id: "3".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "y".into(),
                    arguments: "{}".into(),
                },
            },
            ToolCall {
                id: "4".into(),
                kind: "function".into(),
                function: FunctionCall {
                    name: "x".into(),
                    arguments: "{}".into(),
                },
            },
        ];
        assert_eq!(turn_signature(&a), turn_signature(&b));
    }

    /// Pin all four agent source dirs into an isolated sandbox so `<agents>` discovery is deterministic.
    fn with_agent_sandbox<T>(tag: &str, f: impl FnOnce(&std::path::Path) -> T) -> T {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let root = std::env::temp_dir().join(format!("ng-tlp-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::env::set_var("USERPROFILE", &root);
        std::env::set_var("HOME", &root);
        std::env::set_var("AIZEN_HOME", root.join(".aizen"));
        std::env::set_var("NEXTGEN_HOME", root.join(".aizen"));
        std::env::set_var("NG_PROJECT_ROOT", root.join("proj"));
        let out = f(&root);
        for v in [
            "USERPROFILE",
            "HOME",
            "AIZEN_HOME",
            "NEXTGEN_HOME",
            "NG_PROJECT_ROOT",
        ] {
            std::env::remove_var(v);
        }
        let _ = std::fs::remove_dir_all(&root);
        out
    }

    #[test]
    fn top_level_prompt_equals_base_when_optional_suffixes_are_off() {
        with_agent_sandbox("none", |_root| {
            crate::core::cli_config::save(&crate::core::cli_config::CliConfig {
                response_visuals: Some(crate::core::cli_config::ResponseVisuals::Off),
                ..Default::default()
            })
            .unwrap();
            let base = build_system_prompt("/w", "linux", "2026-06-20", "m", None);
            let top = build_top_level_system_prompt("/w", "linux", "2026-06-20", "m", None);
            assert_eq!(base, top);
            assert!(!top.contains("<agents>"));
            assert!(!top.contains("<response_visuals"));
        });
    }

    #[test]
    fn visual_contract_is_top_level_only_and_mode_specific() {
        let auto =
            response_visuals_prompt_block(crate::core::cli_config::ResponseVisuals::Auto).unwrap();
        assert!(auto.contains("mode=\"auto\""));
        assert!(auto.contains("Skip it for yes/no"));
        assert!(auto.contains("fenced `diagram`"));
        assert!(auto.contains("Never emit Mermaid"));

        let always =
            response_visuals_prompt_block(crate::core::cli_config::ResponseVisuals::Always)
                .unwrap();
        assert!(always.contains("at least ONE meaningful compact visual"));
        assert!(always.contains("exact JSON/code"));
        assert!(
            response_visuals_prompt_block(crate::core::cli_config::ResponseVisuals::Off).is_none()
        );

        let sub = build_subagent_base_prompt("/w", "linux", "2026-06-20", "m", false);
        assert!(
            !sub.contains("<response_visuals"),
            "sub-agents must not pay the visual contract tax"
        );
    }

    #[test]
    fn top_level_prompt_adds_agents_block_and_keeps_base_prefix() {
        with_agent_sandbox("some", |root| {
            crate::core::cli_config::save(&crate::core::cli_config::CliConfig {
                response_visuals: Some(crate::core::cli_config::ResponseVisuals::Off),
                ..Default::default()
            })
            .unwrap();
            let dir = root.join(".aizen/agents");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join("code-reviewer.md"),
                "---\nname: Code Reviewer\ndescription: reviews diffs\n---\nbody",
            )
            .unwrap();
            let top = build_top_level_system_prompt("/w", "linux", "2026-06-20", "m", None);
            assert!(top.contains("<agents>"), "installed agent ⇒ index present");
            assert!(
                top.contains("task(agent="),
                "tells the model how to dispatch"
            );
            assert!(
                top.starts_with(system_base().trim_end()),
                "static base prefix is preserved"
            );
            // The block is a pure SUFFIX: stripping it yields exactly the base prompt.
            let base = build_system_prompt("/w", "linux", "2026-06-20", "m", None);
            assert!(
                top.starts_with(&base),
                "agents block is appended after the unchanged base"
            );
        });
    }
}
