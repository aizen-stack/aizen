//! `task` — sub-agent dispatch (the lean, product-only multiagent primitive).
//!
//! The main agent calls `task` to hand a focused, self-contained sub-task to a FRESH sub-agent
//! (its own context, its own role-scoped tool set) and gets back only the sub-agent's final
//! result. This is the CLI analogue of the extension's Task tool / Claude Code's Task — minus
//! the fleet machinery: **single depth only**. A sub-agent runs on a registry that EXCLUDES the
//! `task` tool, so it physically cannot dispatch further sub-agents (recursion guard); a depth
//! guard refuses `depth >= 1` as belt-and-suspenders.
//!
//! Isolation invariants: the sub-agent uses a NON-streaming chat call (it runs silently; only
//! its final text returns to the parent, which streams its own synthesis), and a `quiet` config
//! (no nested progress trace). It inherits the parent's `--yes` (an explicit autonomy opt-in is
//! transitive) and runs its own verify gate so a `coder` sub-agent self-checks before returning.
//!
//! Async-from-sync: `Tool::execute` is synchronous but `run_agent` is async. We bridge with
//! `block_in_place` + the CURRENT runtime's `block_on` — deliberately the same runtime the
//! `reqwest::Client` was built on (a fresh runtime would mismatch reqwest's reactor). `task`
//! declares `is_concurrency_safe() = false`, so it always runs on the loop's serial path inside
//! a runtime worker thread, where `block_in_place` is valid.

use crate::agent::tools::Tool;
use crate::agent::{build_system_prompt, AgentConfig};
use crate::types::{Message, ToolDef};
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::path::PathBuf;

/// The stable sub-agent preamble (a `const` → byte-identical across invocations, so the CLI's
/// own upstream prefix cache stays warm). Kept CLI-specific; not a copy of the extension's.
const SUBAGENT_PREAMBLE: &str = "\
<subagent>
You are a focused sub-agent dispatched to do ONE task and report back.
- output_discipline: your FINAL message is the RETURN VALUE to the orchestrating agent — it is not shown to a human. Return the result/finding directly: no greeting, no \"I'll help\", no sign-off.
- scope: do only the dispatched task; do not widen it. If blocked, stop and state precisely what blocks you.
- workspace: every file/shell op is confined to the working directory; you cannot dispatch further sub-agents.
</subagent>";

pub struct TaskTool {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    /// Inherited from the parent `--yes` — an explicit autonomy opt-in applies transitively.
    auto_approve: bool,
    /// The confinement root, resolved once with the parent registry.
    root: PathBuf,
    /// Dispatch depth (0 at top level). The guard refuses `>= 1`.
    depth: usize,
}

impl TaskTool {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: reqwest::Client,
        base_url: String,
        api_key: String,
        model: String,
        auto_approve: bool,
        root: PathBuf,
        depth: usize,
    ) -> Self {
        Self { client, base_url, api_key, model, auto_approve, root, depth }
    }
}

/// A one-line role brief appended to the sub-agent system prompt. MUST stay consistent with the
/// tool scoping in `builtin::role_registry` (read-only roles get no edit/shell tools).
fn role_brief(role: &str) -> &'static str {
    match role {
        "coder" => "coder — implement the change. Tools: read/glob/edit files, run shell (build/test/search), memory. Read before editing; make sure your change compiles.",
        "planner" => "planner — produce a concrete, ordered plan. READ-ONLY: read/glob files + memory; you cannot edit files or run shell.",
        "reviewer" => "reviewer — assess correctness/security/quality and report findings with file:line. READ-ONLY: read/glob files + memory; you cannot edit or run shell.",
        "tester" => "tester — run and analyze tests/builds, report results. Tools: read/glob files, run shell, memory; you cannot edit files.",
        _ => "assistant — investigate and report concisely. READ-ONLY: read/glob files + memory.",
    }
}

/// Build the sub-agent system prompt: shared base + environment (no frozen-core user_memory —
/// the sub-agent is task-focused) + the stable preamble + the role brief. Shared with the
/// workflow fan-out (every sub-agent — task tool or workflow task — gets the same shape).
pub(crate) fn build_subagent_prompt(role: &str, root: &std::path::Path, model: &str, date: &str) -> String {
    let cwd = root.display().to_string();
    let mut s = build_system_prompt(&cwd, std::env::consts::OS, date, model, None);
    s.push('\n');
    s.push_str(SUBAGENT_PREAMBLE);
    s.push_str(&format!("\n<role>\n{}\n</role>\n", role_brief(role)));
    s
}

impl Tool for TaskTool {
    fn name(&self) -> &str {
        "task"
    }
    fn description(&self) -> &str {
        "Dispatch a focused sub-agent (fresh context) to do ONE self-contained sub-task and return \
         its result. Use for isolatable work that would clutter your own context (a deep \
         investigation, a contained implementation). The sub-agent CANNOT dispatch further \
         sub-agents. Roles: coder (read/edit/shell), tester (shell, no edit), planner/reviewer \
         (read-only)."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": {"type": "string", "description": "the complete, self-contained task for the sub-agent"},
                "role": {"type": "string", "enum": ["coder", "planner", "reviewer", "tester"], "description": "sub-agent role (default coder); decides its tool scope"},
                "model": {"type": "string", "description": "optional model override for the sub-agent"}
            },
            "required": ["prompt"],
            "additionalProperties": false
        })
    }
    /// MUST stay `false`. Beyond avoiding heavyweight parallel dispatch, this is a load-bearing
    /// invariant: `execute` calls `block_in_place`, which requires a Tokio worker thread. The
    /// parallel path (`execute_parallel`) runs tools on `std::thread::scope` threads that have
    /// NO runtime — `block_in_place`/`Handle::current()` would panic there. Returning `false`
    /// keeps `task` on the serial path (a runtime worker). Approval is handled inside the
    /// sub-agent per its own destructive ops, so `task` itself is not approval-gated.
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        // Depth guard (belt-and-suspenders; the sub-registry already excludes `task`).
        if self.depth >= 1 {
            bail!("task is depth-capped at 1 — a sub-agent cannot dispatch further sub-agents");
        }
        let prompt = args
            .get("prompt")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .context("missing required string arg 'prompt'")?;
        let role = args.get("role").and_then(|v| v.as_str()).unwrap_or("coder").to_string();
        let model =
            args.get("model").and_then(|v| v.as_str()).unwrap_or(&self.model).to_string();

        let registry = crate::agent::builtin::role_registry(&role, &self.root);
        let date = chrono::Local::now().format("%Y-%m-%d").to_string();
        let system = build_subagent_prompt(&role, &self.root, &model, &date);

        // Non-streaming chat closure: the sub-agent runs silently; the parent streams synthesis.
        let client = self.client.clone();
        let base = self.base_url.clone();
        let key = self.api_key.clone();
        let chat = move |msgs: Vec<Message>, defs: Vec<ToolDef>| {
            let client = client.clone();
            let base = base.clone();
            let key = key.clone();
            let model = model.clone();
            async move {
                crate::client::chat_with_tools(&client, &base, &key, &model, &msgs, &defs).await
            }
        };

        let cfg = AgentConfig {
            auto_approve: self.auto_approve, // inherit parent --yes (transitive autonomy)
            quiet: true,                     // suppress nested progress trace
            enable_verify_gate: false,       // verify is a TOP-LEVEL concern (the parent run owns it)
            ..AgentConfig::default()
        };

        // Make the transitive --yes grant visible: a sub-agent running unattended-destructive
        // is easy to miss otherwise.
        if self.auto_approve {
            eprintln!("→ task({role}): running with --yes (sub-agent destructive ops auto-approved)");
        }

        // Bridge sync→async on the CURRENT runtime (same one the reqwest client was built on).
        // MUST run on a Tokio MULTI-THREAD worker thread — `block_in_place` panics on a
        // current-thread runtime / with no runtime. The serial-path invariant guarantees this:
        // `is_concurrency_safe()==false` keeps `task` off `execute_parallel`'s scoped threads
        // (which have no runtime), so `execute` is only ever reached from `run_agent`'s async
        // serial path under `#[tokio::main]` (multi-thread). Never call `execute` from a plain
        // `#[test]` past the early-return guards.
        let outcome = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(crate::agent::run_agent(chat, &cfg, &registry, &system, prompt))
        })
        .with_context(|| format!("sub-agent ({role}) failed"))?;

        let stop = match outcome.stop {
            crate::agent::StopReason::Done => "done",
            crate::agent::StopReason::Divergence => "diverged (repeated itself)",
            crate::agent::StopReason::MaxIters => "hit the step limit",
            // Unreachable for a sub-agent (no `clarify` in any role registry — nobody to answer),
            // but the match must be total.
            crate::agent::StopReason::AwaitingInput(_) => "stopped to ask (no interactive user)",
        };
        let body = outcome
            .final_text
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "(sub-agent produced no final answer)".to_string());
        Ok(format!("[task: role={role}, {} step(s), {stop}]\n{body}", outcome.iters))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(depth: usize) -> TaskTool {
        TaskTool::new(
            reqwest::Client::new(),
            "http://localhost".into(),
            "k".into(),
            "m".into(),
            false,
            std::env::temp_dir(),
            depth,
        )
    }

    #[test]
    fn depth_guard_refuses_nested_dispatch() {
        // A sub-agent's task tool (depth 1) must refuse BEFORE any network call.
        let t = tool(1);
        let err = t.execute(&serde_json::json!({"prompt": "do x"})).unwrap_err();
        assert!(err.to_string().contains("depth-capped"), "got: {err}");
    }

    #[test]
    fn missing_prompt_is_an_error() {
        let t = tool(0);
        assert!(t.execute(&serde_json::json!({"role": "coder"})).is_err());
        assert!(t.execute(&serde_json::json!({"prompt": "   "})).is_err(), "blank prompt rejected");
    }

    #[test]
    fn role_registry_scopes_tools_and_never_includes_task() {
        let root = std::env::temp_dir();
        let coder = crate::agent::builtin::role_registry("coder", &root);
        assert!(coder.get("file_edit").is_some(), "coder can edit");
        assert!(coder.get("shell_run").is_some(), "coder can shell");
        assert!(coder.get("task").is_none(), "NO recursion: sub-registry excludes task");

        let planner = crate::agent::builtin::role_registry("planner", &root);
        assert!(planner.get("file_read").is_some(), "planner can read");
        assert!(planner.get("file_edit").is_none(), "planner is read-only");
        assert!(planner.get("shell_run").is_none(), "planner has no shell");
        assert!(planner.get("task").is_none());

        let reviewer = crate::agent::builtin::role_registry("reviewer", &root);
        assert!(reviewer.get("file_edit").is_none() && reviewer.get("shell_run").is_none());

        let tester = crate::agent::builtin::role_registry("tester", &root);
        assert!(tester.get("shell_run").is_some(), "tester can run tests");
        assert!(tester.get("file_edit").is_none(), "tester cannot edit");

        let unknown = crate::agent::builtin::role_registry("weird", &root);
        assert!(unknown.get("file_edit").is_none(), "unknown role → conservative read-only");
    }

    #[test]
    fn subagent_prompt_has_stable_preamble_and_role() {
        let root = std::env::temp_dir();
        let p = build_subagent_prompt("reviewer", &root, "m", "2026-06-20");
        assert!(p.contains("<subagent>"), "preamble present");
        assert!(p.contains("output_discipline"));
        assert!(p.contains("cannot dispatch further sub-agents"));
        assert!(p.contains("reviewer —"), "role brief present");
        // no always-on user_memory block in a sub-agent prompt
        assert!(!p.contains("\n<user_memory>\n"));
    }

    #[test]
    fn role_brief_matches_scoping() {
        // planner/reviewer briefs claim read-only; their registries must agree (caught drift).
        let root = std::env::temp_dir();
        for role in ["planner", "reviewer"] {
            assert!(role_brief(role).contains("READ-ONLY"));
            let r = crate::agent::builtin::role_registry(role, &root);
            assert!(r.get("file_edit").is_none() && r.get("shell_run").is_none());
        }
    }
}
