//! `ng workflow <spec.json>` — the lean orchestration layer (fan-out + mixture-of-agents synth).
//!
//! A workflow is a flat set of sub-tasks run CONCURRENTLY (each a role-scoped sub-agent reusing
//! `run_agent`), followed by ONE synthesis pass that merges their results into a single
//! deliverable (mixture-of-agents). Deliberately NO DAG / dependencies / wave-retry / durable
//! state: the multi-agent research proved product task-composition saturates around ~4 agents,
//! so a flat bounded fan-out captures the value at a fraction of the orchestrator cost. Anything
//! requiring real dependencies is reachable serially via the `task` tool.
//!
//! Concurrency is bounded to `MAX_PARALLEL` (chunked). Unlike the `task` tool (which bridges
//! sync→async because `Tool::execute` is sync), the workflow runs directly in the async command
//! path, so it fans out with plain `join_all` — no `block_in_place`.
//!
//! Honesty note (không bịa đặt): the synthesis model is the configured model unless the spec
//! overrides it (`synthesis.model`). We do NOT auto-"escalate to a stronger tier" — the CLI is
//! provider-agnostic and cannot know a gateway's tier names; tiering is the spec author's choice.

use crate::agent::builtin::{agent_registry, role_registry};
use crate::agent::task_tool::{build_agent_subagent_prompt, build_subagent_prompt};
use crate::agent::{run_agent, AgentConfig, StopReason};
use crate::llm::client::{chat_with_tools, stream_chat};
use crate::core::types::{Message, ToolDef};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

/// Max sub-agents running at once (matches the parallel-tool cap; conservative for a CLI).
const MAX_PARALLEL: usize = 5;

/// Per-child step budget (mirrors `task` default `max_steps=15`). Workflow children are narrower
/// than a top-level agent — keep them from burning the full 25/50 default.
const CHILD_MAX_ITERS: usize = 15;
/// Hard ceiling on auto-extend for a workflow child (`2 × CHILD_MAX_ITERS`, same shape as task).
const CHILD_AUTO_EXTEND: usize = 30;
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
    let want = MAX_PARALLEL.min(spec.tasks.len());
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
    )
    .await;
    drop(slots);

    for r in &results {
        eprintln!("  • {} ({}/{}) — {} [{} step(s)]", r.id, r.role, r.model, r.status, r.iters);
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
    let synth_prompt = build_synthesis_prompt(&spec.name, instruction, &results);

    // Optional audit trace of the fan-out (per-task model + outcome + the synthesis model). Written
    // BEFORE synthesis so a synthesis failure still leaves the fan-out record. Best-effort.
    if let Some(path) = trace {
        if let Err(e) = write_trace(path, &spec.name, &results, synth_model) {
            eprintln!("  (trace not written: {e})");
        } else {
            eprintln!("  trace → {}", path.display());
        }
    }

    if crate::ui::tui::cancel_requested() {
        wf_track.finish_err("cancelled before synthesis");
        bail!("workflow '{}': cancelled by user", spec.name);
    }

    wf_track.set_phase(crate::agent::orchestration::Phase::Synthesizing, format!("via {synth_model}"));
    eprintln!("synthesizing ({synth_model})…\n");
    match stream_chat(http, base_url, api_key, synth_model, vec![Message::user(synth_prompt)]).await {
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
            bail!("workflow '{}' has a duplicate task id '{}'", spec.name, t.id);
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

/// Fan the tasks out, bounded to `MAX_PARALLEL` via chunking — shared by the CLI runner and the
/// model-callable `workflow` tool (see `workflow_tool.rs`).
///
/// `max_parallel` bounds the chunk size: both CLI and tool paths pass the number of sub-agent slots
/// they actually reserved (`SubagentSlot::acquire_up_to`), so a fan-out never runs more concurrent
/// children than the process-global cap allows. Clamped to `1..=MAX_PARALLEL`.
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
    fan_out_tracked(http, base_url, api_key, model, approval_mode, root, date, tasks, max_parallel, None).await
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
) -> Vec<TaskOutcome> {
    let width = max_parallel.clamp(1, MAX_PARALLEL);
    let mut results: Vec<TaskOutcome> = Vec::with_capacity(tasks.len());
    for chunk in tasks.chunks(width) {
        if crate::ui::tui::cancel_requested() {
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
        let futs = chunk
            .iter()
            .map(|t| run_one_task(http, base_url, api_key, model, approval_mode, root, date, t, parent));
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
) -> Result<String> {
    validate_spec_ids(spec)?;
    // Writer guard is applied in `workflow_tool::build_spec` before we get here; re-check so any
    // future direct caller can't bypass it.
    enforce_singular_writer(spec)?;

    let root = std::env::current_dir().context("resolving cwd")?.canonicalize().context("canonicalizing cwd")?;
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    // Reserve ONE global sub-agent slot per concurrent child (bounded by both the workflow's own
    // MAX_PARALLEL and the number of tasks), so a fan-out is accounted against the process-global
    // cap at its real width — not as a single slot for the whole call. The reserved slots are held
    // for the duration of the fan-out and released on drop; the fan-out chunks at exactly the width
    // we secured. An empty reservation (gate already full) is a soft error.
    let want = MAX_PARALLEL.min(spec.tasks.len());
    let slots = crate::agent::task_tool::SubagentSlot::acquire_up_to(want);
    if slots.is_empty() {
        bail!("sub-agent concurrency limit reached — retry when running tasks finish");
    }
    let width = slots.len();
    // Header line into the transcript so the user sees the fan-out begin (TUI path only; no-op on
    // the CLI runner, which prints its own eprintln banner).
    wf_header(&format!("workflow: {} · {} task(s)", spec.name, spec.tasks.len()));
    // Live status for `/workflows` — parent + per-child tracks (children updated in run_one_task).
    let wf_track = crate::agent::orchestration::start_workflow(&spec.name, spec.tasks.len());
    let parent_id = wf_track.id();
    let results =
        fan_out_tracked(http, base_url, api_key, model, approval_mode, &root, &date, &spec.tasks, width, Some(parent_id))
            .await;
    drop(slots); // explicit: hold the reservation across the whole fan-out, free it here

    if results.iter().any(|r| r.status == "cancelled") {
        wf_track.finish_err("cancelled");
        bail!("workflow '{}': cancelled by user", spec.name);
    }
    if results.iter().all(|r| r.status == "error") {
        wf_track.finish_err("all tasks failed");
        bail!("workflow '{}': all {} task(s) failed — nothing to report", spec.name, results.len());
    }

    let mut out = format!("[workflow: {}, {} task(s)]\n", spec.name, results.len());
    for r in &results {
        out.push_str(&format!("  • {} ({}/{}) — {} [{} step(s)]\n", r.id, r.role, r.model, r.status, r.iters));
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
    if crate::ui::tui::cancel_requested() {
        wf_track.finish_err("cancelled before synthesis");
        bail!("workflow '{}': cancelled by user", spec.name);
    }
    let synth_model = spec.synthesis.as_ref().and_then(|s| s.model.as_deref()).unwrap_or(model);
    let default_instruction = "Synthesize the sub-agent results below into ONE coherent, \
        deduplicated answer. Resolve any conflicts explicitly and note which task each key point \
        came from. Do not invent results that no task reported.";
    let instruction = spec.synthesis.as_ref().and_then(|s| s.prompt.as_deref()).unwrap_or(default_instruction);
    let synth_prompt = build_synthesis_prompt(&spec.name, instruction, &results);
    wf_track.set_phase(crate::agent::orchestration::Phase::Synthesizing, format!("via {synth_model}"));
    wf_trace(&format!("⋯ synthesizing ({synth_model})…"));
    let merged = match crate::llm::client::chat_with_tools(
        http,
        base_url,
        api_key,
        synth_model,
        &[Message::user(synth_prompt)],
        &[],
    )
    .await
    {
        Ok(t) => t.content.unwrap_or_default(),
        Err(e) => {
            wf_track.finish_err(format!("synthesis failed: {e}"));
            return Err(e).context("workflow synthesis failed");
        }
    };
    out.push('\n');
    out.push_str(merged.trim());
    wf_track.finish_ok("synthesized");
    Ok(out)
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
) -> TaskOutcome {
    // Bail early if cancel already landed before this child was scheduled.
    if crate::ui::tui::cancel_requested() {
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
    let (label, task_model, registry, system) = match &spec {
        Some(def) => {
            let m = task.model.as_deref().or(def.model.as_deref()).unwrap_or(model);
            (def.slug(), m, agent_registry(def, root), build_agent_subagent_prompt(def, root, m, date, None))
        }
        None => {
            let m = task.model.as_deref().unwrap_or(model);
            (task.role.clone(), m, role_registry(&task.role, root), build_subagent_prompt(&task.role, root, m, date, None))
        }
    };

    let client = http.clone();
    let base = base_url.to_string();
    let key = api_key.to_string();
    let model_s = task_model.to_string();
    let chat = move |msgs: Vec<Message>, defs: Vec<ToolDef>| {
        let client = client.clone();
        let base = base.clone();
        let key = key.clone();
        let model = model_s.clone();
        async move { chat_with_tools(&client, &base, &key, &model, &msgs, &defs).await }
    };
    // Mirror `task` budgets: narrow step cap, bounded auto-extend, no nested todo recitation.
    // Verify gate OFF: concurrent cargo checks thrash the same build lock (top-level verifies).
    // Checkpoint: keep pre-edit once for writers (Time Machine safety) but skip per-edit stamps
    // across parallel children — N children × each-edit would spam the store.
    let is_writer = !crate::agent::task_tool::dispatch_is_read_only(&registry);
    let cfg = AgentConfig {
        approval_mode,
        quiet: true,
        enable_verify_gate: false,
        max_iters: CHILD_MAX_ITERS,
        auto_extend_to: CHILD_AUTO_EXTEND,
        auto_checkpoint: is_writer,
        checkpoint_each_edit: false,
        todo_reminder_every: 0,
        // Sub-agents leave context_window 0 (no tool-result clearing) — workflow children are short.
        // enable_lsp default true is fine; tools only appear if registered in the sub-agent registry.
        ..AgentConfig::default()
    };

    // Live progress: workflow children run `quiet` (no nested trace) and only their MERGED result
    // reaches the parent at the end, so without this the user stares at a blank screen while ≤5
    // agents run. Emit a start + finish line per task into the transcript (only under the sticky
    // TUI — the CLI path prints its own eprintln status and isn't TUI-active, so no double lines).
    // Also register on the process-global orchestration board for `/workflows`.
    let child_track =
        crate::agent::orchestration::start_workflow_child(parent, &task.id, label.as_str());
    wf_trace(&format!("⋯ {} ({label}) running…", task.id));

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
            };
            let ok = status == "done";
            let detail = format!("{status} [{} step(s)]", o.iters);
            if ok {
                child_track.finish_ok(detail);
            } else {
                child_track.finish_err(detail);
            }
            wf_trace_done(ok, &format!("{} ({label}) — {status} [{} step(s)]", task.id, o.iters));
            TaskOutcome {
                id: task.id.clone(),
                role: label.clone(),
                model: task_model.to_string(),
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
                model: task_model.to_string(),
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
        let star = console::style("✦").color256(crate::ui::splash::ACCENT).bold();
        crate::ui::tui::emit_line(&format!("{star} {}", crate::ui::theme::accent(line)));
    }
}

/// Emit one workflow progress line into the sticky-TUI transcript (a quiet `⎿`-prefixed trace,
/// same shape as the agent loop's tool trace). A no-op when the TUI isn't active — the standalone
/// CLI runner prints its own `eprintln!` status and isn't TUI-active, so this never double-prints.
fn wf_trace(line: &str) {
    if crate::ui::tui::active() {
        crate::ui::tui::emit_line(&format!("  {} {}", crate::ui::theme::faint("└"), crate::ui::theme::faint(line)));
    }
}

/// Emit a workflow task's FINISH line — the corner + text turn salmon on failure so a diverged /
/// errored task reads at a glance, matching the agent loop's `emit_tool_result` styling.
fn wf_trace_done(ok: bool, line: &str) {
    if !crate::ui::tui::active() {
        return;
    }
    let corner = if ok { crate::ui::theme::faint("└") } else { crate::ui::theme::err("└") };
    let body = if ok { crate::ui::theme::faint(line).to_string() } else { crate::ui::theme::err(line).to_string() };
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
/// Long child summaries are hard-capped at [`SUMMARY_CHAR_CAP`] so 5 verbose agents cannot blow
/// the synth context / $$.
fn build_synthesis_prompt(name: &str, instruction: &str, results: &[TaskOutcome]) -> String {
    let mut s = format!("You are synthesizing the results of workflow '{name}'.\n\n{instruction}\n\n");
    for r in results {
        s.push_str(&format!(
            "=== task: {} (role={}, {}) ===\n{}\n\n",
            r.id,
            r.role,
            r.status,
            truncate_summary(r.summary.trim())
        ));
    }
    s.push_str("=== end of results ===\nProduce the final synthesis now.");
    s
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
        assert!(!crate::ui::tui::active(), "test harness is never TUI-active");
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
        assert_eq!(spec.tasks[1].role, "coder", "missing role defaults to coder");
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
        assert!(spec.tasks[1].agent.is_none(), "agent is optional; defaults to None");
        assert_eq!(spec.tasks[1].role, "coder", "no agent ⇒ role still defaults to coder");
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
        assert!(spec.tasks[1].model.is_none(), "no override → falls back to the workflow model");
    }

    #[test]
    fn synthesis_prompt_labels_each_task() {
        let results = vec![
            TaskOutcome { id: "bugs".into(), role: "reviewer".into(), model: "m".into(), status: "done".into(), summary: "found a null deref".into(), iters: 3 },
            TaskOutcome { id: "perf".into(), role: "reviewer".into(), model: "m".into(), status: "done".into(), summary: "n+1 query".into(), iters: 2 },
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
        assert!(p.len() < SUMMARY_CHAR_CAP + 800, "prompt must not include the full long body");
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
        let spec = WorkflowSpec { name: "empty".into(), tasks: vec![], synthesis: None };
        let http = reqwest::Client::new();
        // bails on the empty-tasks check BEFORE any network call.
        let err = run_workflow(&http, "http://localhost", "k", "m", crate::core::approval::ApprovalMode::Ask, &spec, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no tasks"));
    }

    #[tokio::test]
    async fn blank_task_id_rejected() {
        let spec = WorkflowSpec {
            name: "blank".into(),
            tasks: vec![WorkflowTask { id: "  ".into(), role: "coder".into(), agent: None, prompt: "x".into(), model: None }],
            synthesis: None,
        };
        let http = reqwest::Client::new();
        let err = run_workflow(&http, "http://localhost", "k", "m", crate::core::approval::ApprovalMode::Ask, &spec, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("blank task id"));
    }

    #[tokio::test]
    async fn duplicate_task_ids_rejected() {
        let spec = WorkflowSpec {
            name: "dup".into(),
            tasks: vec![
                WorkflowTask { id: "a".into(), role: "coder".into(), agent: None, prompt: "x".into(), model: None },
                WorkflowTask { id: "a".into(), role: "coder".into(), agent: None, prompt: "y".into(), model: None },
            ],
            synthesis: None,
        };
        let http = reqwest::Client::new();
        let err = run_workflow(&http, "http://localhost", "k", "m", crate::core::approval::ApprovalMode::Ask, &spec, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("duplicate task id"));
    }
}
