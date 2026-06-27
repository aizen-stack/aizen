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

use crate::agent::builtin::role_registry;
use crate::agent::task_tool::build_subagent_prompt;
use crate::agent::{run_agent, AgentConfig, StopReason};
use crate::client::{chat_with_tools, stream_chat};
use crate::types::{Message, ToolDef};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::Path;

/// Max sub-agents running at once (matches the parallel-tool cap; conservative for a CLI).
const MAX_PARALLEL: usize = 5;

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
pub async fn run_workflow(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    auto_approve: bool,
    spec: &WorkflowSpec,
    trace: Option<&Path>,
) -> Result<()> {
    if spec.tasks.is_empty() {
        bail!("workflow '{}' has no tasks", spec.name);
    }
    // Reject blank/duplicate task ids — the synthesis cites tasks by id; blanks/collisions
    // corrupt the merge.
    let mut seen = std::collections::HashSet::new();
    for t in &spec.tasks {
        if t.id.trim().is_empty() {
            bail!("workflow '{}' has a blank task id", spec.name);
        }
        if !seen.insert(t.id.as_str()) {
            bail!("workflow '{}' has a duplicate task id '{}'", spec.name, t.id);
        }
    }

    let root = std::env::current_dir()
        .context("resolving cwd")?
        .canonicalize()
        .context("canonicalizing cwd")?;
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    eprintln!(
        "workflow '{}': {} task(s), up to {} in parallel",
        spec.name,
        spec.tasks.len(),
        MAX_PARALLEL.min(spec.tasks.len())
    );

    // Fan out, bounded to MAX_PARALLEL via chunking.
    let mut results: Vec<TaskOutcome> = Vec::with_capacity(spec.tasks.len());
    for chunk in spec.tasks.chunks(MAX_PARALLEL) {
        let futs = chunk.iter().map(|t| {
            run_one_task(http, base_url, api_key, model, auto_approve, &root, &date, t)
        });
        results.extend(futures_util::future::join_all(futs).await);
    }

    for r in &results {
        eprintln!("  • {} ({}/{}) — {} [{} step(s)]", r.id, r.role, r.model, r.status, r.iters);
    }

    // If every task errored there is nothing to synthesize — don't spend a synthesis call
    // feeding the model only "error: …" strings.
    if results.iter().all(|r| r.status == "error") {
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

    eprintln!("synthesizing ({synth_model})…\n");
    stream_chat(http, base_url, api_key, synth_model, vec![Message::user(synth_prompt)])
        .await
        .context("workflow synthesis failed")?;
    Ok(())
}

/// Run one task as a role-scoped sub-agent (silent; non-streaming). Errors are captured into the
/// outcome (a failed task never aborts the workflow — its siblings + the synthesis still run).
#[allow(clippy::too_many_arguments)]
async fn run_one_task(
    http: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    auto_approve: bool,
    root: &Path,
    date: &str,
    task: &WorkflowTask,
) -> TaskOutcome {
    // Mixture-of-agents: this task runs on its own model override if the spec set one, else the
    // workflow default. The subagent prompt's `<environment>` reflects the model that truly ran it.
    let task_model = task.model.as_deref().unwrap_or(model);
    let registry = role_registry(&task.role, root);
    let system = build_subagent_prompt(&task.role, root, task_model, date);

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
    // Verify gate OFF in workflow sub-agents: up to MAX_PARALLEL concurrent `cargo check`
    // processes would thrash the same repo's build locks. Verification is a top-level concern.
    let cfg = AgentConfig {
        auto_approve,
        quiet: true,
        enable_verify_gate: false,
        ..AgentConfig::default()
    };

    match run_agent(chat, &cfg, &registry, &system, &task.prompt).await {
        Ok(o) => {
            let status = match o.stop {
                StopReason::Done => "done",
                StopReason::Divergence => "diverged",
                StopReason::MaxIters => "max-iters",
                // Unreachable: workflow sub-agents have no `clarify` tool (nobody to answer).
                StopReason::AwaitingInput(_) => "awaiting-input",
            };
            TaskOutcome {
                id: task.id.clone(),
                role: task.role.clone(),
                model: task_model.to_string(),
                status: status.to_string(),
                summary: o.final_text.unwrap_or_else(|| "(no final answer)".to_string()),
                iters: o.iters,
            }
        }
        Err(e) => TaskOutcome {
            id: task.id.clone(),
            role: task.role.clone(),
            model: task_model.to_string(),
            status: "error".to_string(),
            summary: format!("error: {e}"),
            iters: 0,
        },
    }
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
fn build_synthesis_prompt(name: &str, instruction: &str, results: &[TaskOutcome]) -> String {
    let mut s = format!("You are synthesizing the results of workflow '{name}'.\n\n{instruction}\n\n");
    for r in results {
        s.push_str(&format!(
            "=== task: {} (role={}, {}) ===\n{}\n\n",
            r.id,
            r.role,
            r.status,
            r.summary.trim()
        ));
    }
    s.push_str("=== end of results ===\nProduce the final synthesis now.");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[tokio::test]
    async fn empty_tasks_is_rejected() {
        let spec = WorkflowSpec { name: "empty".into(), tasks: vec![], synthesis: None };
        let http = reqwest::Client::new();
        // bails on the empty-tasks check BEFORE any network call.
        let err = run_workflow(&http, "http://localhost", "k", "m", false, &spec, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no tasks"));
    }

    #[tokio::test]
    async fn blank_task_id_rejected() {
        let spec = WorkflowSpec {
            name: "blank".into(),
            tasks: vec![WorkflowTask { id: "  ".into(), role: "coder".into(), prompt: "x".into(), model: None }],
            synthesis: None,
        };
        let http = reqwest::Client::new();
        let err = run_workflow(&http, "http://localhost", "k", "m", false, &spec, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("blank task id"));
    }

    #[tokio::test]
    async fn duplicate_task_ids_rejected() {
        let spec = WorkflowSpec {
            name: "dup".into(),
            tasks: vec![
                WorkflowTask { id: "a".into(), role: "coder".into(), prompt: "x".into(), model: None },
                WorkflowTask { id: "a".into(), role: "coder".into(), prompt: "y".into(), model: None },
            ],
            synthesis: None,
        };
        let http = reqwest::Client::new();
        let err = run_workflow(&http, "http://localhost", "k", "m", false, &spec, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("duplicate task id"));
    }
}
