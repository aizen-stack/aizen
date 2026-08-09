//! `aizen workflow <spec.json>` — the lean orchestration layer (fan-out + mixture-of-agents synth).
//!
//! A workflow is a flat set of sub-tasks run CONCURRENTLY (each a role-scoped sub-agent reusing
//! `run_agent`), followed by ONE synthesis pass that merges their results into a single
//! deliverable (mixture-of-agents). Deliberately NO DAG / dependencies / wave-retry / durable
//! state: the multi-agent research proved product task-composition saturates around ~4 agents,
//! so a flat bounded fan-out captures the value at a fraction of the orchestrator cost. Anything
//! requiring real dependencies is reachable serially via the `task` tool.
//!
//! Concurrency is bounded to the process-global sub-agent cap (see
//! `task_tool::max_parallel_subagents_pub` — machine-derived from the core count, overridable by
//! env/config), chunked. Unlike the `task` tool (which bridges sync→async because `Tool::execute`
//! is sync), the workflow runs directly in the async command path, so it fans out with plain
//! `join_all` — no `block_in_place`. The model decides how many tasks to request; the gate only
//! limits how many run AT ONCE.
//!
//! Honesty note (không bịa đặt): the synthesis model is the configured model unless the spec
//! overrides it (`synthesis.model`). We do NOT auto-"escalate to a stronger tier" — the CLI is
//! provider-agnostic and cannot know a gateway's tier names; tiering is the spec author's choice.

use crate::agent::builtin::{agent_registry, role_registry};
use crate::agent::task_tool::{build_agent_subagent_prompt, build_subagent_prompt};
use crate::agent::{run_agent, AgentConfig, StopReason};
use crate::core::types::{Message, ToolDef};
use crate::llm::client::{chat_with_tools, stream_chat_with_visual_contract};
use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

/// Per-child step budget (mirrors `task` default `max_steps=15`). Workflow children are narrower
/// than a top-level agent — keep them from burning the full 25/50 default.
const CHILD_MAX_ITERS: usize = 15;
/// Hard ceiling on auto-extend for a workflow child (`2 × CHILD_MAX_ITERS`, same shape as task).
const CHILD_AUTO_EXTEND: usize = 30;
/// Transient model-call failures a workflow child absorbs per turn before giving up (see
/// `AgentConfig::max_transient_retries`). Matches the `task` tool's sub-agent policy
/// (`SUBAGENT_TRANSIENT_RETRIES`) — kept EQUAL on purpose: both are unwatched loops on the same
/// gateway, so a value that is right for one is right for the other, and a divergence here would
/// mean the same provider blip loses a workflow child while a `task` dispatch rides it out.
///
/// Raised 4 → 6 with the empty-200 fix in [`crate::agent::run_agent_loop`]: exhausting the budget is
/// now an `Err` rather than a fall-through reported as a finished run, which makes the number
/// load-bearing instead of merely deciding how long a doomed turn took to give up.
const CHILD_TRANSIENT_RETRIES: usize = 6;
/// Fresh step budgets a STILL-PROGRESSING workflow child may be granted after its auto-extend is
/// spent, instead of returning partial work as `status: "max-iters"` (see
/// `AgentConfig::max_continuations`). Smaller than the top level's: children are meant to be narrow,
/// and up to the process-global cap of them run at once, so the worst case is bounded by
/// `cap × (CHILD_AUTO_EXTEND + CHILD_CONTINUATIONS × CHILD_MAX_ITERS)`.
const CHILD_CONTINUATIONS: usize = 2;

/// Cap each child's summary before stuffing it into the synthesis prompt (chars). Prevents 5 verbose
/// children from blowing the synth context / $$.
const SUMMARY_CHAR_CAP: usize = 4_000;

#[derive(Debug, Deserialize)]
pub struct WorkflowSpec {
    pub name: String,
    pub tasks: Vec<WorkflowTask>,
    #[serde(default)]
    pub synthesis: Option<Synthesis>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct WorkflowTask {
    pub id: String,
    #[serde(default = "default_role")]
    pub role: String,
    /// Optional specialist slug (see [`crate::agents`]). When set and it resolves, this task runs as
    /// that specialist (superseding `role`), mirroring the `task` tool's `agent` param.
    #[serde(default)]
    pub agent: Option<String>,
    pub prompt: String,
    /// Optional per-task model override (mixture-of-agents: diversify WHICH model runs WHICH task —
    /// e.g. a cheap model scouts, a strong one reviews). Mirrors `Synthesis.model`; the spec author
    /// controls it. The CLI never auto-"escalates a tier" — it can't know a gateway's tier names.
    #[serde(default)]
    pub model: Option<String>,
}

fn default_role() -> String {
    "coder".to_string()
}

#[derive(Debug, Deserialize)]
pub struct Synthesis {
    /// Optional model override for the synthesis pass (else the configured model).
    #[serde(default)]
    pub model: Option<String>,
    /// Optional merge instruction (else a sensible default).
    #[serde(default)]
    pub prompt: Option<String>,
}

/// The outcome of one fanned-out sub-task.
pub struct TaskOutcome {
    pub id: String,
    pub role: String,
    /// The model that actually ran this task (the per-task override, else the workflow default).
    pub model: String,
    pub status: String,
    pub summary: String,
    pub iters: usize,
}

/// Run a workflow: fan out the tasks (bounded), then synthesize. Synthesis streams to stdout.
/// `trace`, when set, writes a JSON record of the fan-out (per-task model + outcome + the synthesis
/// model) — useful for auditing a mixture-of-agents run's model diversity.
///
/// Safety parity with the model-callable `workflow` tool:
/// - singular-writer invariant (at most one write-capable task),
/// - process-global `SubagentSlot` accounting,
/// - orchestration Track for `/workflows`,
/// - cooperative cancel observed by each child loop.
pub async fn run_workflow(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    approval_mode: crate::core::approval::ApprovalMode,
    spec: &WorkflowSpec,
    trace: Option<&Path>,
) -> Result<()> {
    let cancel = crate::core::cancel::TurnCancel::new();
    run_workflow_with_cancel(
        http,
        base_url,
        api_key,
        model,
        approval_mode,
        spec,
        trace,
        cancel,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn run_workflow_with_cancel(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    approval_mode: crate::core::approval::ApprovalMode,
    spec: &WorkflowSpec,
    trace: Option<&Path>,
    cancel: crate::core::cancel::TurnCancel,
) -> Result<()> {
    validate_spec_ids(spec)?;
    // Same singular-writer gate as `workflow_tool::build_spec` — CLI specs used to skip it and could
    // launch two default-role `coder`s in parallel (file/build races).
    enforce_singular_writer(spec)?;

    let root = std::env::current_dir()
        .context("resolving cwd")?
        .canonicalize()
        .context("canonicalizing cwd")?;
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();

    // Honest width: reserve one global slot per concurrent child (shared with in-REPL `task` calls).
    let want = crate::agent::task_tool::max_parallel_subagents_pub().min(spec.tasks.len());
    let slots = crate::agent::task_tool::SubagentSlot::acquire_up_to(want);
    if slots.is_empty() {
        bail!("sub-agent concurrency limit reached — retry when running tasks finish");
    }
    let width = slots.len();
    eprintln!(
        "workflow '{}': {} task(s), up to {} in parallel (slots reserved)",
        spec.name,
        spec.tasks.len(),
        width
    );

    let wf_track = crate::agent::orchestration::start_workflow(&spec.name, spec.tasks.len());
    let parent_id = wf_track.id();
    let results = fan_out_tracked(
        http,
        base_url,
        api_key,
        model,
        approval_mode,
        &root,
        &date,
        &spec.tasks,
        width,
        Some(parent_id),
        cancel.clone(),
        0,
    )
    .await;
    drop(slots);

    for r in &results {
        eprintln!(
            "  • {} ({}/{}) — {} [{} step(s)]",
            r.id, r.role, r.model, r.status, r.iters
        );
    }

    if results.iter().any(|r| r.status == "cancelled") {
        wf_track.finish_err("cancelled");
        bail!("workflow '{}': cancelled by user", spec.name);
    }

    // If every task errored there is nothing to synthesize — don't spend a synthesis call
    // feeding the model only "error: …" strings.
    if results.iter().all(|r| r.status == "error") {
        wf_track.finish_err("all tasks failed");
        bail!(
            "workflow '{}': all {} task(s) failed (see per-task errors above) — nothing to synthesize",
            spec.name,
            results.len()
        );
    }

    // Mixture-of-agents synthesis: merge the per-task summaries into one deliverable.
    let synth_model = spec
        .synthesis
        .as_ref()
        .and_then(|s| s.model.as_deref())
        .unwrap_or(model);
    let default_instruction = "Synthesize the sub-agent results below into ONE coherent, \
        deduplicated answer. Resolve any conflicts explicitly and note which task each key point \
        came from. Do not invent results that no task reported.";
    let instruction = spec
        .synthesis
        .as_ref()
        .and_then(|s| s.prompt.as_deref())
        .unwrap_or(default_instruction);
    let synth_prompt = build_synthesis_prompt_capped(&spec.name, instruction, &results, None);

    // Optional audit trace of the fan-out (per-task model + outcome + the synthesis model). Written
    // BEFORE synthesis so a synthesis failure still leaves the fan-out record. Best-effort.
    if let Some(path) = trace {
        if let Err(e) = write_trace(path, &spec.name, &results, synth_model) {
            eprintln!("  (trace not written: {e})");
        } else {
            eprintln!("  trace → {}", path.display());
        }
    }

    if cancel.is_cancelled() {
        wf_track.finish_err("cancelled before synthesis");
        bail!("workflow '{}': cancelled by user", spec.name);
    }

    wf_track.set_phase(
        crate::agent::orchestration::Phase::Synthesizing,
        format!("via {synth_model}"),
    );
    eprintln!("synthesizing ({synth_model})…\n");
    match stream_chat_with_visual_contract(
        http,
        base_url,
        api_key,
        synth_model,
        vec![Message::user(synth_prompt)],
        true,
    )
    .await
    {
        Ok(_) => {
            wf_track.finish_ok("synthesized");
            Ok(())
        }
        Err(e) => {
            wf_track.finish_err(format!("synthesis failed: {e}"));
            Err(e).context("workflow synthesis failed")
        }
    }
}

/// Blank/duplicate task-id guard shared by CLI + tool paths.
fn validate_spec_ids(spec: &WorkflowSpec) -> Result<()> {
    if spec.tasks.is_empty() {
        bail!("workflow '{}' has no tasks", spec.name);
    }
    let mut seen = std::collections::HashSet::new();
    for t in &spec.tasks {
        if t.id.trim().is_empty() {
            bail!("workflow '{}' has a blank task id", spec.name);
        }
        if !seen.insert(t.id.as_str()) {
            bail!(
                "workflow '{}' has a duplicate task id '{}'",
                spec.name,
                t.id
            );
        }
    }
    Ok(())
}

/// At most ONE write-capable task per fan-out (capability resolved the same way as `run_one_task`).
/// Public so the tool path and tests share one source of truth.
pub(crate) fn enforce_singular_writer(spec: &WorkflowSpec) -> Result<()> {
    let writers = spec
        .tasks
        .iter()
        .filter(|t| task_is_writer(&t.role, t.agent.as_deref()))
        .count();
    if writers > 1 {
        bail!(
            "at most ONE write-capable task per workflow (a coder/tester role or a write-scoped agent) \
             — parallel writers race edits; keep the write singular and fan out the reads"
        );
    }
    Ok(())
}

/// Does this task resolve to a WRITE-capable sub-agent? Mirrors the runner's resolution
/// (`run_one_task`): a named `agent` supersedes `role`. Unresolvable slug → coder (write) fallback.
pub(crate) fn task_is_writer(role: &str, agent: Option<&str>) -> bool {
    if let Some(slug) = agent.map(str::trim).filter(|s| !s.is_empty()) {
        return match crate::agents::load(slug) {
            Some(def) => !crate::agent::task_tool::dispatch_is_read_only(
                &crate::agent::builtin::agent_registry(&def, std::path::Path::new(".")),
            ),
            None => true, // unresolvable slug → runner uses coder scope → treat as a writer
        };
    }
    matches!(role, "coder" | "tester")
}

/// Fan the tasks out, bounded to the process-global sub-agent cap via chunking — shared by the CLI
/// runner and the model-callable `workflow` tool (see `workflow_tool.rs`).
///
/// `max_parallel` bounds the chunk size: both CLI and tool paths pass the number of sub-agent slots
/// they actually reserved (`SubagentSlot::acquire_up_to`), so a fan-out never runs more concurrent
/// children than the process-global cap allows. Clamped to `1..=max_parallel_subagents_pub()` (the
/// machine-derived cap; env/config can raise it, only `HARD_CEILING` is absolute).
///
/// Cooperative cancel: if Esc is pressed mid-fan-out, remaining chunks are skipped (already-running
/// children stop at their next loop boundary via `StopReason::Cancelled`).
#[allow(clippy::too_many_arguments)]
#[allow(dead_code)] // kept as a thin public alias for external/test callers; production uses tracked
pub(crate) async fn fan_out(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    approval_mode: crate::core::approval::ApprovalMode,
    root: &Path,
    date: &str,
    tasks: &[WorkflowTask],
    max_parallel: usize,
) -> Vec<TaskOutcome> {
    fan_out_tracked(
        http,
        base_url,
        api_key,
        model,
        approval_mode,
        root,
        date,
        tasks,
        max_parallel,
        None,
        crate::core::cancel::TurnCancel::new(),
        0,
    )
    .await
}

/// Like [`fan_out`], but each child is registered under optional parent id for `/workflows`.
#[allow(clippy::too_many_arguments)]
async fn fan_out_tracked(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    approval_mode: crate::core::approval::ApprovalMode,
    root: &Path,
    date: &str,
    tasks: &[WorkflowTask],
    max_parallel: usize,
    parent: Option<u64>,
    cancel: crate::core::cancel::TurnCancel,
    context_window: usize,
) -> Vec<TaskOutcome> {
    let width = max_parallel.clamp(1, crate::agent::task_tool::max_parallel_subagents_pub());
    let mut results: Vec<TaskOutcome> = Vec::with_capacity(tasks.len());
    for chunk in tasks.chunks(width) {
        if cancel.is_cancelled() {
            // Skip not-yet-started tasks; mark them cancelled so the parent doesn't synthesize junk.
            for t in chunk {
                results.push(TaskOutcome {
                    id: t.id.clone(),
                    role: t.role.clone(),
                    model: model.to_string(),
                    status: "cancelled".into(),
                    summary: "cancelled by user before start".into(),
                    iters: 0,
                });
            }
            // Also mark any remaining tasks past this chunk.
            let done = results.len();
            for t in tasks.iter().skip(done) {
                results.push(TaskOutcome {
                    id: t.id.clone(),
                    role: t.role.clone(),
                    model: model.to_string(),
                    status: "cancelled".into(),
                    summary: "cancelled by user before start".into(),
                    iters: 0,
                });
            }
            break;
        }
        let futs = chunk.iter().map(|t| {
            run_one_task(
                http,
                base_url,
                api_key,
                model,
                approval_mode,
                root,
                date,
                t,
                parent,
                cancel.clone(),
                context_window,
            )
        });
        results.extend(futures_util::future::join_all(futs).await);
    }
    results
}

/// [`run_workflow`]'s COLLECTING sibling for the in-conversation `workflow` tool: same validation
/// + fan-out, but a NON-streaming synthesis, everything returned as one result string (per-task
/// status lines + the merged answer) instead of printed.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_workflow_collect(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    approval_mode: crate::core::approval::ApprovalMode,
    spec: &WorkflowSpec,
    synthesize: bool,
    root: &Path,
    context_window: usize,
) -> Result<String> {
    let cancel = crate::core::cancel::current().unwrap_or_default();
    validate_spec_ids(spec)?;
    // Writer guard is applied in `workflow_tool::build_spec` before we get here; re-check so any
    // future direct caller can't bypass it.
    enforce_singular_writer(spec)?;

    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    // Reserve ONE global sub-agent slot per concurrent child (bounded by both the process-global
    // sub-agent cap and the number of tasks), so a fan-out is accounted against the global cap at
    // its real width — not as a single slot for the whole call. The reserved slots are held for the
    // duration of the fan-out and released on drop; the fan-out chunks at exactly the width we
    // secured. An empty reservation (gate already full) is a soft error.
    let want = crate::agent::task_tool::max_parallel_subagents_pub().min(spec.tasks.len());
    let slots = crate::agent::task_tool::SubagentSlot::acquire_up_to(want);
    if slots.is_empty() {
        bail!("sub-agent concurrency limit reached — retry when running tasks finish");
    }
    let width = slots.len();
    // Header line into the transcript so the user sees the fan-out begin (TUI path only; no-op on
    // the CLI runner, which prints its own eprintln banner).
    wf_header(&format!(
        "workflow: {} · {} task(s)",
        spec.name,
        spec.tasks.len()
    ));
    // Live status for `/workflows` — parent + per-child tracks (children updated in run_one_task).
    let wf_track = crate::agent::orchestration::start_workflow(&spec.name, spec.tasks.len());
    let parent_id = wf_track.id();
    let results = fan_out_tracked(
        http,
        base_url,
        api_key,
        model,
        approval_mode,
        &root,
        &date,
        &spec.tasks,
        width,
        Some(parent_id),
        cancel.clone(),
        context_window,
    )
    .await;
    drop(slots); // explicit: hold the reservation across the whole fan-out, free it here

    if results.iter().any(|r| r.status == "cancelled") {
        wf_track.finish_err("cancelled");
        bail!("workflow '{}': cancelled by user", spec.name);
    }
    if results.iter().all(|r| r.status == "error") {
        wf_track.finish_err("all tasks failed");
        bail!(
            "workflow '{}': all {} task(s) failed — nothing to report",
            spec.name,
            results.len()
        );
    }

    let mut out = format!("[workflow: {}, {} task(s)]\n", spec.name, results.len());
    for r in &results {
        out.push_str(&format!(
            "  • {} ({}/{}) — {} [{} step(s)]\n",
            r.id, r.role, r.model, r.status, r.iters
        ));
    }
    if !synthesize {
        // Verify-style callers want the RAW per-task outputs, not a merged narrative.
        for r in &results {
            out.push_str(&format!("\n── {} ──\n{}\n", r.id, r.summary));
        }
        let fails = results.iter().filter(|r| r.status == "error").count();
        if fails > 0 {
            wf_track.finish_err(format!("{fails} failed · raw verdicts"));
        } else {
            wf_track.finish_ok("raw verdicts");
        }
        return Ok(out.trim_end().to_string());
    }
    if cancel.is_cancelled() {
        wf_track.finish_err("cancelled before synthesis");
        bail!("workflow '{}': cancelled by user", spec.name);
    }
    let synth_model = spec
        .synthesis
        .as_ref()
        .and_then(|s| s.model.as_deref())
        .unwrap_or(model);
    let default_instruction = "Synthesize the sub-agent results below into ONE coherent, \
        deduplicated answer. Resolve any conflicts explicitly and note which task each key point \
        came from. Do not invent results that no task reported.";
    let instruction = spec
        .synthesis
        .as_ref()
        .and_then(|s| s.prompt.as_deref())
        .unwrap_or(default_instruction);
    let synth_prompt = build_synthesis_prompt_capped(
        &spec.name,
        instruction,
        &results,
        context_window
            .checked_mul(2)
            .filter(|&chars| chars >= SUMMARY_CHAR_CAP),
    );
    wf_track.set_phase(
        crate::agent::orchestration::Phase::Synthesizing,
        format!("via {synth_model}"),
    );
    wf_trace(&format!("⋯ synthesizing ({synth_model})…"));
    let synth_deadline = crate::agent::task_tool::subagent_call_timeout();
    let merged = match await_synthesis(
        &cancel,
        synth_deadline,
        crate::llm::client::chat_with_tools(
            http,
            base_url,
            api_key,
            synth_model,
            &[Message::user(synth_prompt)],
            &[],
        ),
    )
    .await
    {
        SynthOutcome::Cancelled => {
            wf_track.finish_err("cancelled during synthesis");
            bail!("workflow '{}': cancelled by user", spec.name);
        }
        SynthOutcome::Merged(text) => text,
        SynthOutcome::Failed(e) => {
            wf_track.finish_err(format!("synthesis failed: {e}"));
            return Err(e).context("workflow synthesis failed");
        }
    };
    out.push('\n');
    out.push_str(merged.trim());
    wf_track.finish_ok("synthesized");
    Ok(out)
}

/// What the bounded synthesis await produced.
enum SynthOutcome {
    /// Esc (or `/workflows stop`) landed while the synthesis call was in flight.
    Cancelled,
    /// The merged answer (possibly empty text, which the caller reports as-is).
    Merged(String),
    /// The call failed, or its deadline elapsed.
    Failed(anyhow::Error),
}

/// Await the synthesis call under BOTH cooperative cancel and a wall-clock deadline.
///
/// Every child of a fan-out is time-bounded (see [`crate::agent::task_tool::subagent_call_timeout`]
/// and `subagent_wall_deadline`), but the synthesis pass that merges their work was bounded only by
/// cancel — and cancel needs somebody watching. This is the one call in a fan-out with no step budget
/// and no stall watchdog behind it: it runs on the NON-streaming path, so the streaming inter-event
/// watchdog never applies, and reqwest's `read_timeout` only fires when the socket goes BYTE-silent,
/// which a keepalive-warm gateway never does. So a gateway that answers 200 and then drips could park
/// a fan-out AFTER every child had already finished: all the work done, the slots freed, and no result
/// — the failure shape that reads as "the workflow just never came back".
///
/// A deadline turns that into an ordinary `Err`, which the caller already reports as a failed
/// synthesis while keeping the per-task status lines it had assembled. Generic over the future so the
/// deadline is testable without a network call.
async fn await_synthesis<F>(
    cancel: &crate::core::cancel::TurnCancel,
    deadline: std::time::Duration,
    call: F,
) -> SynthOutcome
where
    F: std::future::Future<Output = Result<crate::llm::client::ChatTurn>>,
{
    match crate::core::cancel::race(cancel, tokio::time::timeout(deadline, call)).await {
        None => SynthOutcome::Cancelled,
        Some(Ok(Ok(turn))) => SynthOutcome::Merged(turn.content.unwrap_or_default()),
        Some(Ok(Err(e))) => SynthOutcome::Failed(e),
        Some(Err(_)) => SynthOutcome::Failed(anyhow!(
            "synthesis call exceeded {}s with no response (set AIZEN_SUBAGENT_CALL_SECS to raise \
             the limit)",
            deadline.as_secs()
        )),
    }
}

/// Run one task as a role-scoped sub-agent (silent; non-streaming). Errors are captured into the
/// outcome (a failed task never aborts the workflow — its siblings + the synthesis still run).
#[allow(clippy::too_many_arguments)]
async fn run_one_task(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    approval_mode: crate::core::approval::ApprovalMode,
    root: &Path,
    date: &str,
    task: &WorkflowTask,
    parent: Option<u64>,
    cancel: crate::core::cancel::TurnCancel,
    context_window: usize,
) -> TaskOutcome {
    // Bail early if cancel already landed before this child was scheduled.
    if cancel.is_cancelled() {
        return TaskOutcome {
            id: task.id.clone(),
            role: task.role.clone(),
            model: model.to_string(),
            status: "cancelled".into(),
            summary: "cancelled by user before start".into(),
            iters: 0,
        };
    }
    // A resolvable `agent` slug supersedes `role` (the specialist/fusion path), mirroring the `task`
    // tool. Model precedence: per-task `model` > the specialist's `def.model` > the workflow default.
    let spec = task
        .agent
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(crate::agents::load);
    // Model precedence: per-task `model` > the specialist's `def.model` > the workflow default.
    // The resolved model is then routed through the model-endpoint registry so a task pinned to
    // another provider's model carries ITS gateway (base_url/api_key) — the caller it inherits from
    // is the workflow's own endpoint, mirroring the `task` tool's `endpoint_for_model` routing.
    let caller = crate::core::cli_config::ResolvedEndpoint {
        base_url: base_url.to_string(),
        api_key: api_key.to_string(),
        model: model.to_string(),
    };
    let (label, ep, registry, system) = match &spec {
        Some(def) => {
            let m = task
                .model
                .as_deref()
                .or(def.model.as_deref())
                .unwrap_or(model);
            let ep = crate::core::cli_config::endpoint_for_model(m, &caller);
            let system = build_agent_subagent_prompt(def, root, &ep.model, date, None);
            (def.slug(), ep, agent_registry(def, root), system)
        }
        None => {
            let m = task.model.as_deref().unwrap_or(model);
            let ep = crate::core::cli_config::endpoint_for_model(m, &caller);
            let system = build_subagent_prompt(
                &task.role,
                root,
                &ep.model,
                date,
                None,
                Some(task.prompt.as_str()),
            );
            (
                task.role.clone(),
                ep,
                role_registry(&task.role, root),
                system,
            )
        }
    };

    let client = http.clone();
    let base = ep.base_url.clone();
    let key = ep.api_key.clone();
    let model_s = ep.model.clone();
    // The result-model label (reported back in TaskOutcome) — a separate owned copy so the `move`
    // chat closure below can consume `model_s` without leaving the outcome unable to name the model.
    let result_model = ep.model.clone();
    let chat = move |msgs: Vec<Message>, defs: Vec<ToolDef>| {
        let client = client.clone();
        let base = base.clone();
        let key = key.clone();
        let model = model_s.clone();
        async move {
            // Same wall-clock deadline as a `task` child (see
            // `task_tool::SUBAGENT_CALL_TIMEOUT`): none of this child's budgets count TIME, so a
            // call that never returns would park its slot — and, because `join_all` below has no
            // per-task timeout, stall the entire chunk with no result and no error. An elapsed
            // deadline is an ordinary `Err`, which `run_one_task` records as a failed task while
            // its siblings and the synthesis still run.
            let deadline = crate::agent::task_tool::subagent_call_timeout();
            match tokio::time::timeout(
                deadline,
                chat_with_tools(&client, &base, &key, &model, &msgs, &defs),
            )
            .await
            {
                Ok(result) => result,
                Err(_) => Err(anyhow!(
                    "model call exceeded {}s with no response (set AIZEN_SUBAGENT_CALL_SECS to \
                     raise the limit)",
                    deadline.as_secs()
                )),
            }
        }
    };
    // Mirror `task` budgets: narrow step cap, bounded auto-extend, no nested todo recitation.
    // Verify gate OFF: concurrent cargo checks thrash the same build lock (top-level verifies).
    // Checkpoint: keep pre-edit once for writers (Time Machine safety) but skip per-edit stamps
    // across parallel children — N children × each-edit would spam the store.
    let is_writer = !crate::agent::task_tool::dispatch_is_read_only(&registry);
    // A DERIVED token per child, so `/workflows stop #<id>` can end ONE child of a fan-out while its
    // siblings and the pending synthesis carry on. Esc still stops everything: cancellation flows down
    // from the turn token this was derived from (see `TurnCancel::child`). Armed on the board below.
    let own_cancel = cancel.child();
    let cfg = AgentConfig {
        approval_mode,
        cancel: own_cancel.clone(),
        // Inherit the parent turn's conversation identity so a workflow child's tool bodies (e.g.
        // browser) scope to the SAME conversation as the orchestrating turn. `default` on the CLI
        // `run_workflow` path, which has no conversation thread — matching the pre-context behavior.
        exec_ctx: crate::core::exec_ctx::current().unwrap_or_default(),
        quiet: true,
        enable_verify_gate: false,
        max_iters: CHILD_MAX_ITERS,
        auto_extend_to: CHILD_AUTO_EXTEND,
        auto_checkpoint: is_writer,
        checkpoint_each_edit: false,
        todo_reminder_every: 0,
        // P0: workflow children use ScopedTodo; keep process-global poke/gates off.
        enable_todo_poke: false,
        enable_confidence_gate: false,
        enable_hill_climb: false,
        // Same reason as the `task` tool: a workflow child runs unwatched, and a transient gateway
        // error used to reduce a whole child's work to `status: "error"` in the synthesis input.
        max_transient_retries: CHILD_TRANSIENT_RETRIES,
        // Unlike the `task` tool, nothing resumes a workflow child — a budget exhaustion here becomes
        // `status: "max-iters"` in the synthesis input, i.e. partial work presented as a result. Grant
        // the same in-loop continuation the top level gets, but smaller: children are meant to be
        // narrow, and N of them run concurrently. Stall recovery stays off — it reads the
        // process-global todo list, which belongs to the orchestrating turn, not this child.
        max_continuations: CHILD_CONTINUATIONS,
        max_stall_recoveries: 0,
        // The PARENT's resolved window, so tool-result clearing and the wrap-up guard are ON for a
        // child (the clearing/guard knobs themselves come from `AgentConfig::default()` below, which
        // already carries the production percentages — they are inert while this is 0).
        //
        // A child is not short: `CHILD_AUTO_EXTEND` plus `CHILD_CONTINUATIONS` fresh budgets is up to
        // 60 steps of tool results, and unlike a `task` dispatch nothing above it compacts. Leaving
        // this 0 meant a read-heavy fan-out child could overflow its provider context mid-run — the
        // one failure the cheap deterministic clearing exists to prevent. `0` (the CLI `run_workflow`
        // path, which has no resolved window) keeps the previous behavior exactly.
        context_window,
        // WALL-CLOCK ceiling per child, same knob as a `task` dispatch. It matters MORE here than
        // there: `join_all` below has no per-task timeout, so one child still grinding holds up the
        // whole chunk's synthesis — every sibling can be done and the fan-out still produces nothing.
        // A child that hits the ceiling becomes `status: "deadline"` in the synthesis input, so the
        // rest of the work is reported instead of waiting on the slowest one indefinitely.
        deadline: crate::agent::task_tool::subagent_wall_deadline(),
        // enable_lsp default true is fine; tools only appear if registered in the sub-agent registry.
        // The clearing knobs themselves come from the defaults below — they are inert while
        // `context_window` is 0 and become live the moment a real window is threaded in.
        ..AgentConfig::default()
    };

    // Live progress: workflow children run `quiet` (no nested trace) and only their MERGED result
    // reaches the parent at the end, so without this the user stares at a blank screen while ≤5
    // agents run. Emit a start + finish line per task into the transcript (only under the sticky
    // TUI — the CLI path prints its own eprintln status and isn't TUI-active, so no double lines).
    // Also register on the process-global orchestration board for `/workflows`.
    // `task.id` is positional (`t1`, `refute-3`) — it identifies a row but says nothing about the
    // work, and `label` only names the ROLE. With N children running silently and concurrently, a
    // trace of `⋯ t1 (reviewer) running…` gives the user no way to tell them apart, so carry each
    // child's own subject. Same clipper as the `workflow` spawn line, so one task cannot appear
    // under two different names on two surfaces.
    let subject =
        crate::agent::subagent_subject(&serde_json::json!({"prompt": task.prompt.as_str()}), 44);
    let child_track = crate::agent::orchestration::start_workflow_child(
        parent,
        &task.id,
        // The board's own row already prints the id; spend its label on role + subject.
        if subject.is_empty() {
            label.clone()
        } else {
            format!("{label} · {subject}")
        },
    );
    child_track.arm_stop(own_cancel);
    if subject.is_empty() {
        wf_trace(&format!("⋯ {} ({label}) running…", task.id));
    } else {
        wf_trace(&format!("⋯ {} ({label}) {subject} …", task.id));
    }

    match run_agent(chat, &cfg, &registry, &system, &task.prompt).await {
        Ok(o) => {
            let status = match o.stop {
                StopReason::Done => "done",
                StopReason::Divergence => "diverged",
                StopReason::MaxIters => "max-iters",
                StopReason::VerificationFailed => "verification-failed",
                // Unreachable: workflow sub-agents have no `clarify` tool (nobody to answer).
                StopReason::AwaitingInput(_) => "awaiting-input",
                StopReason::Cancelled => "cancelled",
                StopReason::Deadline => "deadline",
            };
            let ok = status == "done";
            let detail = format!("{status} [{} step(s)]", o.iters);
            if ok {
                child_track.finish_ok(detail);
            } else {
                child_track.finish_err(detail);
            }
            wf_trace_done(
                ok,
                &format!("{} ({label}) — {status} [{} step(s)]", task.id, o.iters),
            );
            TaskOutcome {
                id: task.id.clone(),
                role: label.clone(),
                model: result_model.clone(),
                status: status.to_string(),
                summary: o.final_text.unwrap_or_else(|| {
                    if status == "cancelled" {
                        "cancelled by user".to_string()
                    } else {
                        "(no final answer)".to_string()
                    }
                }),
                iters: o.iters,
            }
        }
        Err(e) => {
            child_track.finish_err(format!("error: {e}"));
            wf_trace_done(false, &format!("{} ({label}) — error", task.id));
            TaskOutcome {
                id: task.id.clone(),
                role: label.clone(),
                model: result_model.clone(),
                status: "error".to_string(),
                summary: format!("error: {e}"),
                iters: 0,
            }
        }
    }
}

/// Emit the workflow header line (the fan-out banner) into the sticky-TUI transcript — a moonlight
/// `✦` + label, matching the turn-start whimsy line's accent so it reads as a section opener.
fn wf_header(line: &str) {
    if crate::ui::tui::active() {
        let star = console::style("✦")
            .color256(crate::ui::splash::ACCENT)
            .bold();
        crate::ui::tui::emit_line(&format!("{star} {}", crate::ui::theme::accent(line)));
    }
}

/// Emit one workflow progress line into the sticky-TUI transcript (a quiet `⎿`-prefixed trace,
/// same shape as the agent loop's tool trace). A no-op when the TUI isn't active — the standalone
/// CLI runner prints its own `eprintln!` status and isn't TUI-active, so this never double-prints.
fn wf_trace(line: &str) {
    if crate::ui::tui::active() {
        crate::ui::tui::emit_line(&format!(
            "  {} {}",
            crate::ui::theme::faint("└"),
            crate::ui::theme::faint(line)
        ));
    }
}

/// Emit a workflow task's FINISH line — the corner + text turn salmon on failure so a diverged /
/// errored task reads at a glance, matching the agent loop's `emit_tool_result` styling.
fn wf_trace_done(ok: bool, line: &str) {
    if !crate::ui::tui::active() {
        return;
    }
    let corner = if ok {
        crate::ui::theme::faint("└")
    } else {
        crate::ui::theme::err("└")
    };
    let body = if ok {
        crate::ui::theme::faint(line).to_string()
    } else {
        crate::ui::theme::err(line).to_string()
    };
    crate::ui::tui::emit_line(&format!("  {corner} {body}"));
}

/// Write a JSON audit trace of the fan-out (best-effort; caller logs any error).
fn write_trace(path: &Path, name: &str, results: &[TaskOutcome], synth_model: &str) -> Result<()> {
    let tasks: Vec<serde_json::Value> = results
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "role": r.role,
                "model": r.model,
                "status": r.status,
                "iters": r.iters,
                "summary": r.summary,
            })
        })
        .collect();
    let doc = serde_json::json!({
        "workflow": name,
        "synthesis_model": synth_model,
        "tasks": tasks,
    });
    let text = serde_json::to_string_pretty(&doc).context("serializing workflow trace")?;
    std::fs::write(path, text).with_context(|| format!("writing trace {}", path.display()))?;
    Ok(())
}

/// Build the synthesis prompt from the per-task outcomes (each clearly delimited + labeled).
///
/// [`SUMMARY_CHAR_CAP`] bounds ONE child; `max_chars` bounds the WHOLE request. The two are not
/// substitutes: the per-summary cap was written for a ≤5-child fan-out, but the tool accepts 32
/// tasks per call, so 32 verbose children could assemble a ~128k-char synthesis request — larger
/// than many context windows — while every individual summary sat comfortably under its own cap.
///
/// The cap drops WHOLE blocks rather than clipping the last one mid-sentence: a half-summary reads
/// like a complete finding that happens to end abruptly, which is exactly the shape a synthesis pass
/// would then state as fact. Dropping the block and SAYING SO keeps the omission visible to the
/// model. `None` ⇒ no total cap (the CLI runner, which has no resolved window).
fn build_synthesis_prompt_capped(
    name: &str,
    instruction: &str,
    results: &[TaskOutcome],
    max_chars: Option<usize>,
) -> String {
    let mut s =
        format!("You are synthesizing the results of workflow '{name}'.\n\n{instruction}\n\n");
    let mut omitted: Vec<&str> = Vec::new();
    for r in results {
        let block = format!(
            "=== task: {} (role={}, {}) ===\n{}\n\n",
            r.id,
            r.role,
            r.status,
            truncate_summary(r.summary.trim())
        );
        // Whole-block admission: a block that doesn't fit is named, not clipped.
        if let Some(cap) = max_chars {
            if s.chars().count() + block.chars().count() > cap {
                omitted.push(r.id.as_str());
                continue;
            }
        }
        s.push_str(&block);
    }
    if !omitted.is_empty() {
        s.push_str(&format!(
            "=== omitted for context budget ===\n{} task summar{} left out of this request entirely \
             (ids: {}). Do NOT infer their content — say the synthesis does not cover them.\n\n",
            omitted.len(),
            if omitted.len() == 1 { "y was" } else { "ies were" },
            omitted.join(", ")
        ));
    }
    s.push_str("=== end of results ===\nProduce the final synthesis now.");
    s
}

#[cfg(test)]
fn build_synthesis_prompt(name: &str, instruction: &str, results: &[TaskOutcome]) -> String {
    build_synthesis_prompt_capped(name, instruction, results, None)
}

/// Cap a child summary for the synth prompt (char-based; UTF-8 safe via char boundary walk).
fn truncate_summary(text: &str) -> String {
    if text.len() <= SUMMARY_CHAR_CAP {
        return text.to_string();
    }
    // Walk back to a char boundary so we never split a multi-byte rune.
    let mut end = SUMMARY_CHAR_CAP;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}…\n[truncated: {} chars total, kept first {}]",
        &text[..end],
        text.len(),
        end
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_trace_helpers_are_noop_off_tui() {
        // Off the sticky TUI (the test harness), the progress helpers must be silent no-ops — the
        // CLI runner prints its own eprintln status, so these emitting there would double-print.
        // We can't assert "nothing printed" cheaply, but we CAN assert they don't panic / gate
        // correctly on `tui::active()` (false under tests). A regression to an unconditional emit
        // would still run here without a live TUI and is caught by the smoke path.
        assert!(
            !crate::ui::tui::active(),
            "test harness is never TUI-active"
        );
        wf_header("workflow: t · 2 task(s)");
        wf_trace("⋯ a (reviewer) running…");
        wf_trace_done(true, "a (reviewer) — done [3 step(s)]");
        wf_trace_done(false, "b (coder) — error");
    }

    #[test]
    fn parses_spec_with_defaults() {
        let json = r#"{
            "name": "review",
            "tasks": [
                {"id": "bugs", "role": "reviewer", "prompt": "find bugs"},
                {"id": "impl", "prompt": "implement X"}
            ]
        }"#;
        let spec: WorkflowSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.name, "review");
        assert_eq!(spec.tasks.len(), 2);
        assert_eq!(spec.tasks[0].role, "reviewer");
        assert_eq!(
            spec.tasks[1].role, "coder",
            "missing role defaults to coder"
        );
        assert!(spec.synthesis.is_none(), "synthesis is optional");
    }

    #[test]
    fn parses_synthesis_overrides() {
        let json = r#"{
            "name": "w",
            "tasks": [{"id": "a", "prompt": "p"}],
            "synthesis": {"model": "big-model", "prompt": "merge it"}
        }"#;
        let spec: WorkflowSpec = serde_json::from_str(json).unwrap();
        let synth = spec.synthesis.unwrap();
        assert_eq!(synth.model.as_deref(), Some("big-model"));
        assert_eq!(synth.prompt.as_deref(), Some("merge it"));
    }

    #[test]
    fn parses_task_with_agent_and_defaults_none() {
        let json = r#"{
            "name": "spec",
            "tasks": [
                {"id": "review", "agent": "code-reviewer", "prompt": "review the diff"},
                {"id": "impl", "prompt": "implement X"}
            ]
        }"#;
        let spec: WorkflowSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.tasks[0].agent.as_deref(), Some("code-reviewer"));
        assert!(
            spec.tasks[1].agent.is_none(),
            "agent is optional; defaults to None"
        );
        assert_eq!(
            spec.tasks[1].role, "coder",
            "no agent ⇒ role still defaults to coder"
        );
    }

    #[test]
    fn parses_per_task_model_override() {
        let json = r#"{
            "name": "moa",
            "tasks": [
                {"id": "scout", "prompt": "scan", "model": "cheap-model"},
                {"id": "judge", "prompt": "review"}
            ]
        }"#;
        let spec: WorkflowSpec = serde_json::from_str(json).unwrap();
        assert_eq!(spec.tasks[0].model.as_deref(), Some("cheap-model"));
        assert!(
            spec.tasks[1].model.is_none(),
            "no override → falls back to the workflow model"
        );
    }

    #[test]
    fn synthesis_prompt_labels_each_task() {
        let results = vec![
            TaskOutcome {
                id: "bugs".into(),
                role: "reviewer".into(),
                model: "m".into(),
                status: "done".into(),
                summary: "found a null deref".into(),
                iters: 3,
            },
            TaskOutcome {
                id: "perf".into(),
                role: "reviewer".into(),
                model: "m".into(),
                status: "done".into(),
                summary: "n+1 query".into(),
                iters: 2,
            },
        ];
        let p = build_synthesis_prompt("review", "merge", &results);
        assert!(p.contains("workflow 'review'"));
        assert!(p.contains("task: bugs (role=reviewer, done)"));
        assert!(p.contains("found a null deref"));
        assert!(p.contains("task: perf"));
        assert!(p.contains("merge"));
    }

    #[test]
    fn synthesis_prompt_truncates_long_summaries() {
        let long = "x".repeat(SUMMARY_CHAR_CAP + 500);
        let results = vec![TaskOutcome {
            id: "a".into(),
            role: "reviewer".into(),
            model: "m".into(),
            status: "done".into(),
            summary: long,
            iters: 1,
        }];
        let p = build_synthesis_prompt("w", "merge", &results);
        assert!(p.contains("[truncated:"), "must mark truncation: {p}");
        assert!(
            p.len() < SUMMARY_CHAR_CAP + 800,
            "prompt must not include the full long body"
        );
    }

    #[test]
    fn singular_writer_rejects_two_coders() {
        let spec = WorkflowSpec {
            name: "race".into(),
            tasks: vec![
                WorkflowTask {
                    id: "a".into(),
                    role: "coder".into(),
                    agent: None,
                    prompt: "edit a".into(),
                    model: None,
                },
                WorkflowTask {
                    id: "b".into(),
                    role: "coder".into(),
                    agent: None,
                    prompt: "edit b".into(),
                    model: None,
                },
            ],
            synthesis: None,
        };
        let err = enforce_singular_writer(&spec).unwrap_err().to_string();
        assert!(err.contains("write-capable"), "{err}");
    }

    #[test]
    fn singular_writer_allows_one_coder_plus_readers() {
        let spec = WorkflowSpec {
            name: "ok".into(),
            tasks: vec![
                WorkflowTask {
                    id: "a".into(),
                    role: "coder".into(),
                    agent: None,
                    prompt: "edit".into(),
                    model: None,
                },
                WorkflowTask {
                    id: "b".into(),
                    role: "reviewer".into(),
                    agent: None,
                    prompt: "review".into(),
                    model: None,
                },
            ],
            synthesis: None,
        };
        enforce_singular_writer(&spec).unwrap();
    }

    #[test]
    fn task_is_writer_classifies_roles() {
        assert!(task_is_writer("coder", None));
        assert!(task_is_writer("tester", None));
        assert!(!task_is_writer("reviewer", None));
        assert!(!task_is_writer("planner", None));
        assert!(task_is_writer("reviewer", Some("__no_such_agent__")));
    }

    #[tokio::test]
    async fn empty_tasks_is_rejected() {
        let spec = WorkflowSpec {
            name: "empty".into(),
            tasks: vec![],
            synthesis: None,
        };
        let http = reqwest::Client::new();
        // bails on the empty-tasks check BEFORE any network call.
        let err = run_workflow(
            &http,
            "http://localhost",
            "k",
            "m",
            crate::core::approval::ApprovalMode::Ask,
            &spec,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no tasks"));
    }

    #[tokio::test]
    async fn blank_task_id_rejected() {
        let spec = WorkflowSpec {
            name: "blank".into(),
            tasks: vec![WorkflowTask {
                id: "  ".into(),
                role: "coder".into(),
                agent: None,
                prompt: "x".into(),
                model: None,
            }],
            synthesis: None,
        };
        let http = reqwest::Client::new();
        let err = run_workflow(
            &http,
            "http://localhost",
            "k",
            "m",
            crate::core::approval::ApprovalMode::Ask,
            &spec,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("blank task id"));
    }

    #[tokio::test]
    async fn duplicate_task_ids_rejected() {
        let spec = WorkflowSpec {
            name: "dup".into(),
            tasks: vec![
                WorkflowTask {
                    id: "a".into(),
                    role: "coder".into(),
                    agent: None,
                    prompt: "x".into(),
                    model: None,
                },
                WorkflowTask {
                    id: "a".into(),
                    role: "coder".into(),
                    agent: None,
                    prompt: "y".into(),
                    model: None,
                },
            ],
            synthesis: None,
        };
        let http = reqwest::Client::new();
        let err = run_workflow(
            &http,
            "http://localhost",
            "k",
            "m",
            crate::core::approval::ApprovalMode::Ask,
            &spec,
            None,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("duplicate task id"));
    }

    fn outcome(id: &str, summary: &str) -> TaskOutcome {
        TaskOutcome {
            id: id.into(),
            role: "reviewer".into(),
            model: "m".into(),
            status: "done".into(),
            summary: summary.into(),
            iters: 1,
        }
    }

    #[test]
    fn synthesis_total_cap_drops_whole_blocks_and_names_them() {
        // The per-summary cap bounds ONE child; this bounds the WHOLE request. A 32-task fan-out of
        // individually-legal summaries must not assemble a prompt larger than the window.
        let results: Vec<TaskOutcome> = (1..=10)
            .map(|i| outcome(&format!("t{i}"), &"x".repeat(1_000)))
            .collect();
        let capped = build_synthesis_prompt_capped("big", "merge", &results, Some(3_000));
        assert!(
            capped.chars().count() < 4_000,
            "total cap must bound the request: {} chars",
            capped.chars().count()
        );
        // Whatever DID fit stays intact — the cap never clips a summary mid-sentence, because a
        // half-finding reads like a complete one and the synthesis would state it as fact.
        assert!(capped.contains("=== task: t1 (role=reviewer, done) ==="));
        assert!(
            capped.contains("omitted for context budget"),
            "the omission must be visible to the model: {capped}"
        );
        assert!(
            capped.contains("t10"),
            "omitted ids must be named so the synthesis can disclaim them: {capped}"
        );
        // Uncapped (the CLI runner, no resolved window) keeps every block.
        let full = build_synthesis_prompt_capped("big", "merge", &results, None);
        assert!(!full.contains("omitted for context budget"));
        for i in 1..=10 {
            assert!(full.contains(&format!("=== task: t{i} ")), "block {i} kept");
        }
    }

    #[tokio::test]
    async fn synthesis_deadline_fails_instead_of_parking_the_fan_out() {
        // A gateway that answers 200 then keepalive-drips has no byte-silence for reqwest's
        // read_timeout to catch, and the non-streaming path has no inter-event watchdog. Without a
        // deadline the fan-out would hang AFTER every child finished — all work done, no result.
        let cancel = crate::core::cancel::TurnCancel::new();
        let never = std::future::pending::<Result<crate::llm::client::ChatTurn>>();
        let out = await_synthesis(&cancel, std::time::Duration::from_secs(1), never).await;
        match out {
            SynthOutcome::Failed(e) => {
                let msg = e.to_string();
                assert!(msg.contains("1s"), "names the elapsed budget: {msg}");
                assert!(
                    msg.contains("AIZEN_SUBAGENT_CALL_SECS"),
                    "names the knob that raises it: {msg}"
                );
            }
            SynthOutcome::Cancelled => panic!("a timeout is not a cancel"),
            SynthOutcome::Merged(_) => panic!("a pending call cannot produce a merge"),
        }
    }

    #[tokio::test]
    async fn synthesis_cancel_is_reported_as_cancel_not_failure() {
        // Esc during synthesis must stay distinguishable from a deadline/provider error: the caller
        // bails with "cancelled by user" rather than reporting a failed synthesis.
        let cancel = crate::core::cancel::TurnCancel::new();
        cancel.cancel();
        let never = std::future::pending::<Result<crate::llm::client::ChatTurn>>();
        assert!(matches!(
            await_synthesis(&cancel, std::time::Duration::from_secs(30), never).await,
            SynthOutcome::Cancelled
        ));
    }

    #[tokio::test]
    async fn synthesis_passes_through_a_provider_error() {
        let cancel = crate::core::cancel::TurnCancel::new();
        let failing = async { Err::<crate::llm::client::ChatTurn, _>(anyhow!("upstream 500")) };
        match await_synthesis(&cancel, std::time::Duration::from_secs(30), failing).await {
            SynthOutcome::Failed(e) => assert!(e.to_string().contains("upstream 500")),
            _ => panic!("a provider error must surface as a failure"),
        }
    }

    #[tokio::test]
    async fn collect_resolves_paths_against_the_supplied_root_not_the_process_cwd() {
        // The model-callable `workflow` used to read `current_dir()`, so under `aizen serve` — where
        // several lanes run concurrently with their OWN roots — a fan-out could read, edit, and
        // checkpoint the wrong project. The root now arrives from the registry that built the tool.
        // Proven WITHOUT a network call: an empty-tasks spec bails after the root is resolved, and a
        // non-existent root would surface here if the path were still taken from the process cwd.
        let lane_root = std::env::temp_dir().join("aizen-wf-root-probe");
        std::fs::create_dir_all(&lane_root).expect("probe root");
        let cwd_before = std::env::current_dir().expect("cwd");
        let spec = WorkflowSpec {
            name: "empty".into(),
            tasks: vec![],
            synthesis: None,
        };
        let http = reqwest::Client::new();
        let err = run_workflow_collect(
            &http,
            "http://localhost",
            "k",
            "m",
            crate::core::approval::ApprovalMode::Ask,
            &spec,
            true,
            &lane_root,
            200_000,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no tasks"), "{err}");
        assert_eq!(
            std::env::current_dir().expect("cwd"),
            cwd_before,
            "resolving a lane root must never move the process"
        );
    }
}
