//! `workflow` — the model-callable fan-out primitive (deterministic orchestration IN conversation).
//!
//! `ng workflow` (workflow.rs) is CLI-only: the model can't invoke it, so multi-agent patterns had
//! to be narrated serially through `task`. This tool exposes the same bounded fan-out with two
//! modes, keeping control flow in CODE and content in the model (the workflows-over-agents rule):
//!
//! - `fanout`: run ≤5 tasks concurrently, then one synthesis pass (mixture-of-agents). At most ONE
//!   `coder` task per call — parallel writers in one repo race edits and build locks; the fan-out
//!   is for READS (investigate/review/test in parallel), the write stays singular.
//! - `verify`: the adversarial-refuter preset — each finding gets a read-only reviewer explicitly
//!   prompted to REFUTE it (industrially measured at ~0.93 accuracy filtering false positives).
//!   No synthesis: the per-finding verdicts return raw.
//!
//! Registration is GATED (config `workflow_tool: true`, or auto when ≥1 specialist agent is
//! ENABLED on the allowlist): the schema costs ~350 tokens on every turn, so only the delegating
//! population pays — agent files merely existing on disk (bulk installs, repo-shipped dirs) don't.
//! Depth 0 only, like `task`. The tool itself is not concurrency-safe — it IS the parallelism.

use crate::agent::tools::Tool;
use crate::agent::workflow::{run_workflow_collect, Synthesis, WorkflowSpec, WorkflowTask};
use anyhow::{bail, Result};
use serde_json::Value;

pub struct WorkflowTool {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    auto_approve: bool,
    depth: usize,
}

impl WorkflowTool {
    pub fn new(
        client: reqwest::Client,
        base_url: String,
        api_key: String,
        model: String,
        auto_approve: bool,
        depth: usize,
    ) -> Self {
        Self { client, base_url, api_key, model, auto_approve, depth }
    }
}

/// The refuter template for `verify` mode: adversarial framing + a fixed verdict contract.
fn refuter_prompt(finding: &str) -> String {
    format!(
        "Adversarially try to REFUTE the following finding. Read the cited code yourself; do not \
         take the claim at face value. Reply with exactly one line `verdict: confirmed` or \
         `verdict: refuted` or `verdict: uncertain`, then your evidence with file:line.\n\nFINDING:\n{finding}"
    )
}

/// Does this task resolve to a WRITE-capable sub-agent? Mirrors the runner's resolution
/// (`run_one_task`): a named `agent` supersedes `role`, and its capability is whether the resolved
/// registry grants any workspace-write tool. The write tool NAMES present don't depend on the root
/// the registry is built at, so a placeholder root is fine here. An unresolvable slug falls back to
/// coder (write) scope at run time, so it counts as a writer. Without an agent, the write roles are
/// `coder` and `tester` (tester runs `shell_run`).
fn task_is_writer(role: &str, agent: Option<&str>) -> bool {
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

/// Build the spec for one call. Pure for role-only tasks; a task naming an `agent` resolves that
/// specialist from disk to classify its write-capability (see [`task_is_writer`]).
pub(crate) fn build_spec(args: &Value) -> Result<(WorkflowSpec, bool)> {
    let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("fanout");
    match mode {
        "fanout" => {
            let tasks_in = args
                .get("tasks")
                .and_then(|v| v.as_array())
                .filter(|a| !a.is_empty())
                .ok_or_else(|| anyhow::anyhow!("fanout mode requires a non-empty 'tasks' array"))?;
            if tasks_in.len() > 5 {
                bail!("workflow caps at 5 tasks per call (got {})", tasks_in.len());
            }
            let mut tasks = Vec::new();
            for (i, t) in tasks_in.iter().enumerate() {
                let role = t.get("role").and_then(|v| v.as_str()).unwrap_or("coder").to_string();
                tasks.push(WorkflowTask {
                    id: t.get("id").and_then(|v| v.as_str()).map(str::to_string).unwrap_or_else(|| format!("t{}", i + 1)),
                    role,
                    agent: t.get("agent").and_then(|v| v.as_str()).map(str::to_string),
                    prompt: t
                        .get("prompt")
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.trim().is_empty())
                        .ok_or_else(|| anyhow::anyhow!("task #{} is missing 'prompt'", i + 1))?
                        .to_string(),
                    model: t.get("model").and_then(|v| v.as_str()).map(str::to_string),
                });
            }
            // Singular-writer invariant: at most ONE write-capable task may fan out — parallel
            // writers race file edits and the build lock. Capability is resolved the SAME way the
            // runner (`run_one_task`) resolves it: a named specialist `agent` supersedes `role`, so
            // a write-scoped agent counts as a writer even when its `role` label says "reviewer"
            // (counting the label alone let that case race a real coder). Read-only roles and
            // read-only specialists fan out freely.
            let writers = tasks.iter().filter(|t| task_is_writer(&t.role, t.agent.as_deref())).count();
            if writers > 1 {
                bail!("at most ONE write-capable task per workflow call (a coder/tester role or a write-scoped agent) — parallel writers race edits; keep the write singular and fan out the reads");
            }
            let synthesis = args
                .get("synthesis")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(|p| Synthesis { model: None, prompt: Some(p.to_string()) });
            Ok((WorkflowSpec { name: "fanout".into(), tasks, synthesis }, true))
        }
        "verify" => {
            let findings = args
                .get("findings")
                .and_then(|v| v.as_array())
                .filter(|a| !a.is_empty())
                .ok_or_else(|| anyhow::anyhow!("verify mode requires a non-empty 'findings' array"))?;
            if findings.len() > 5 {
                bail!("verify caps at 5 findings per call (got {})", findings.len());
            }
            let tasks = findings
                .iter()
                .enumerate()
                .filter_map(|(i, f)| f.as_str().map(|s| (i, s)))
                .map(|(i, f)| WorkflowTask {
                    id: format!("refute-{}", i + 1),
                    role: "reviewer".to_string(), // read-only → these fan out safely
                    agent: None,
                    prompt: refuter_prompt(f),
                    model: None,
                })
                .collect::<Vec<_>>();
            if tasks.is_empty() {
                bail!("verify mode: 'findings' must be strings");
            }
            // No synthesis: per-finding verdicts return raw (a merge would launder the evidence).
            Ok((WorkflowSpec { name: "verify".into(), tasks, synthesis: None }, false))
        }
        other => bail!("unknown workflow mode '{other}' (use fanout or verify)"),
    }
}

impl Tool for WorkflowTool {
    fn name(&self) -> &str {
        "workflow"
    }
    fn description(&self) -> &str {
        "Run several sub-agents CONCURRENTLY (deterministic fan-out; ≤5). mode=fanout: independent \
         tasks in parallel + one synthesized answer — for multi-angle investigation/review (at most \
         ONE coder task; writes stay singular). mode=verify: adversarially re-check findings — each \
         finding gets a read-only refuter, verdicts return per finding. For a single sub-task use \
         `task` instead."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "mode": {"type": "string", "enum": ["fanout", "verify"], "description": "fanout: run tasks in parallel and synthesize · verify: refute findings adversarially"},
                "tasks": {"type": "array", "maxItems": 5, "description": "fanout mode: the tasks to run concurrently", "items": {"type": "object", "properties": {
                    "id": {"type": "string"},
                    "prompt": {"type": "string", "description": "complete, self-contained task"},
                    "role": {"type": "string", "enum": ["coder", "planner", "reviewer", "tester"]},
                    "agent": {"type": "string", "description": "optional specialist slug from <agents>"},
                    "model": {"type": "string"}
                }, "required": ["prompt"], "additionalProperties": false}},
                "synthesis": {"type": "string", "description": "fanout mode: optional merge instruction"},
                "findings": {"type": "array", "maxItems": 5, "items": {"type": "string"}, "description": "verify mode: claims to refute, each self-contained with file:line evidence"}
            },
            "required": ["mode"],
            "additionalProperties": false
        })
    }
    /// Never concurrency-safe: the tool IS the parallelism (its children are the concurrent part),
    /// and its writer arm must keep barrier semantics.
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        if self.depth >= 1 {
            bail!("workflow is depth-capped at 1 — a sub-agent cannot orchestrate further fan-outs");
        }
        let (spec, synthesize) = build_spec(args)?;
        // Sub-agent gate is acquired INSIDE run_workflow_collect — one slot per concurrent child
        // (see SubagentSlot::acquire_up_to), so the fan-out is counted against the global cap at its
        // real width rather than as a single slot for the whole call. Over-limit → soft error there.
        let client = self.client.clone();
        let base = self.base_url.clone();
        let key = self.api_key.clone();
        let model = self.model.clone();
        let auto = self.auto_approve;
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(run_workflow_collect(&client, &base, &key, &model, auto, &spec, synthesize))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_mode_builds_refuter_tasks_no_synthesis() {
        let (spec, synth) = build_spec(&serde_json::json!({
            "mode": "verify",
            "findings": ["off-by-one in src/a.rs:10", "race in src/b.rs:99"]
        }))
        .unwrap();
        assert!(!synth, "verify returns raw verdicts, never a merged narrative");
        assert_eq!(spec.tasks.len(), 2);
        assert_eq!(spec.tasks[0].id, "refute-1");
        assert_eq!(spec.tasks[0].role, "reviewer", "refuters are read-only → they fan out");
        assert!(spec.tasks[0].prompt.contains("REFUTE"));
        assert!(spec.tasks[0].prompt.contains("off-by-one in src/a.rs:10"));
        assert!(spec.tasks[1].prompt.contains("verdict: confirmed"), "fixed verdict contract");
    }

    #[test]
    fn fanout_rejects_parallel_writers() {
        let err = build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [
                {"prompt": "edit a", "role": "coder"},
                {"prompt": "edit b", "role": "coder"}
            ]
        }))
        .unwrap_err()
        .to_string();
        assert!(err.contains("write-capable"), "{err}");
        // One coder + readers is fine.
        let (spec, synth) = build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [
                {"prompt": "edit a", "role": "coder"},
                {"prompt": "review b", "role": "reviewer"}
            ]
        }))
        .unwrap();
        assert!(synth);
        assert_eq!(spec.tasks.len(), 2);
        assert_eq!(spec.tasks[0].id, "t1", "ids default positionally");
    }

    #[test]
    fn spec_validation_rejects_junk() {
        assert!(build_spec(&serde_json::json!({"mode": "fanout"})).is_err(), "no tasks");
        assert!(build_spec(&serde_json::json!({"mode": "verify"})).is_err(), "no findings");
        assert!(build_spec(&serde_json::json!({"mode": "dag"})).is_err(), "unknown mode");
        let six: Vec<_> = (0..6).map(|i| serde_json::json!({"prompt": format!("t{i}"), "role": "reviewer"})).collect();
        assert!(build_spec(&serde_json::json!({"mode": "fanout", "tasks": six})).is_err(), "cap 5");
    }

    #[test]
    fn task_is_writer_classifies_by_capability() {
        // Bare roles: coder and tester write (file_edit / shell_run); planner and reviewer read.
        assert!(task_is_writer("coder", None));
        assert!(task_is_writer("tester", None));
        assert!(!task_is_writer("reviewer", None));
        assert!(!task_is_writer("planner", None));
        // An unresolvable agent slug falls back to coder (write) scope at run time → count as writer.
        assert!(task_is_writer("reviewer", Some("__no_such_agent__")));
    }

    #[test]
    fn fanout_allows_two_readers() {
        // Two read-only tasks are NOT writers → they fan out freely (the invariant is one WRITER).
        let (spec, _) = build_spec(&serde_json::json!({
            "mode": "fanout",
            "tasks": [
                {"prompt": "review a", "role": "reviewer"},
                {"prompt": "plan b", "role": "planner"}
            ]
        }))
        .unwrap();
        assert_eq!(spec.tasks.len(), 2);
    }

    #[test]
    fn depth_guard_refuses_nested_fanout() {
        let t = WorkflowTool::new(reqwest::Client::new(), "http://x".into(), "k".into(), "m".into(), false, 1);
        let err = t.execute(&serde_json::json!({"mode": "verify", "findings": ["x"]})).unwrap_err();
        assert!(err.to_string().contains("depth-capped"), "{err}");
    }
}
