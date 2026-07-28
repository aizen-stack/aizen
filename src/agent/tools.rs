//! The tool surface: a `Tool` trait + a registry the agent loop advertises to the model.
//!
//! Discipline (the repo's hard-won lesson — semantic OVERLAP between tools is the killer,
//! not the count): names are `category_action`, one canonical tool per capability, and each
//! description says when to use it AND when NOT to (point at the sibling tool instead).
//! Execution is synchronous — tools are local (memory / fs / subprocess).

use crate::core::types::ToolDef;
use anyhow::Result;
use serde_json::Value;

/// What a tool call may mutate in the local workspace. This is deliberately separate from
/// `is_destructive`: an outbound notification needs approval but no code checkpoint, while a file
/// edit needs both. Args-aware classification keeps read-only process actions from creating Git
/// snapshots merely because their tool shares one broad capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkspaceEffect {
    None,
    Paths,
    OpaqueWorkspace,
    RepoMetadata,
    External,
}

impl WorkspaceEffect {
    pub fn needs_checkpoint(self) -> bool {
        matches!(self, Self::Paths | Self::OpaqueWorkspace)
    }
}

/// A capability the model may invoke.
///
/// `Send + Sync` is REQUIRED: the agent loop runs each call of a concurrency-safe batch on a
/// `tokio::task::spawn_blocking` thread (see `agent::execute_calls`), which needs the shared
/// `Arc<dyn Tool>` to cross threads. All built-in tools hold only `PathBuf`/unit, so they satisfy
/// this trivially. A future tool holding a non-`Sync` field (`Rc`, `RefCell`, …) will fail to
/// compile here — which is the correct guardrail, not a false alarm: such a tool could not be run
/// concurrently anyway.
pub trait Tool: Send + Sync {
    /// `category_action`, unique in the registry (e.g. `memory_search`, `file_read`).
    fn name(&self) -> &str;
    /// What it does · when to use · "Not for X → use Y" · caveat (see system-prompt.md).
    fn description(&self) -> &str;
    /// JSON-Schema for the arguments object (use `additionalProperties:false` for strict gateways).
    fn parameters(&self) -> Value;
    /// Destructive / outward-facing → the loop gates it behind approval before executing.
    fn is_destructive(&self) -> bool {
        false
    }
    /// Read-only & side-effect-free → eligible for concurrent execution in a parallel batch.
    /// Destructive tools also set this `false`.
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    /// Args-aware refinement of [`Tool::is_concurrency_safe`]: a tool whose safety depends on WHAT
    /// it was asked to do overrides this (e.g. a `task` dispatch is safe iff the resolved sub-agent
    /// registry is read-only). Default: the static flag.
    fn is_concurrency_safe_for(&self, _args: &Value) -> bool {
        self.is_concurrency_safe()
    }
    /// Workspace mutation classification used by automatic checkpoints and verification. Override
    /// this for local writers; approval-only/network/process-control tools keep the default `None`.
    fn workspace_effect(&self, _args: &Value) -> WorkspaceEffect {
        WorkspaceEffect::None
    }
    /// WHERE this call will write — an absolute directory to look for a repository in. The
    /// checkpoint gate uses it to discover the work tree that actually owns the change, instead of
    /// assuming the process's cwd does. Those differ constantly in practice: a session launched from
    /// the home directory editing `Desktop/proj/src/x.js` found no repo at cwd and reported "not a
    /// git repository (run `git init`)" — advice that, followed literally, would have turned the
    /// user's ENTIRE HOME DIRECTORY into a repository, while the real project sat one level down
    /// with a perfectly good `.git`. Override in every tool that names a path; the default `None`
    /// means "no particular path" and leaves the gate on cwd.
    fn workspace_target(&self, _args: &Value) -> Option<std::path::PathBuf> {
        None
    }
    /// Whether beginning this call may create a durable workspace, process, network, external-service,
    /// or delegated-agent effect whose completion would be ambiguous after a crash. Defaults to the
    /// existing safety classifications; pure reads override naturally by being non-destructive with
    /// no workspace effect.
    fn recovery_effect(&self, args: &Value) -> bool {
        self.is_destructive() || self.workspace_effect(args) != WorkspaceEffect::None
    }
    /// Run the tool. `args` is the parsed (object) arguments. Return the result text; return
    /// an `Err` for a real failure — the loop feeds the error back to the model to recover.
    fn execute(&self, args: &Value) -> Result<String>;
    /// Does THIS tool consider `result` a failure? A tool that returns `Ok(...)` even on a logical
    /// failure (so the model sees the detail) — e.g. `shell_run`'s `exit N`, or an MCP tool that
    /// encodes `{"isError":true}` in its Ok body — overrides this so the loop's progress/thrash
    /// guard doesn't count that "success" as real progress (W12). `None` (the default) ⇒ defer to
    /// the loop's generic heuristic (`error:` prefix / non-zero `exit N`); `Some(b)` is definitive.
    fn result_is_error(&self, _result: &str) -> Option<bool> {
        None
    }
}

/// Drive an async future from the sync `Tool::execute` path, RACED against user cancellation
/// (Esc while working) so a long network call aborts promptly instead of blocking the turn.
///
/// Validity invariant (the async execution core rests on this): the body is safe BOTH on a tokio
/// multi-thread WORKER (where `block_in_place` parks the worker's queue — the classic bridge) AND
/// on a `spawn_blocking` thread — verified against the vendored tokio source: `block_in_place`
/// with a runtime context but off a worker is a no-op pass-through, and `Handle::block_on` off a
/// worker is the documented sync bridge. Pinned by `bridge_works_inside_spawn_blocking` below so
/// a future tokio upgrade that changes this fails loudly.
pub(crate) fn block_for_tool<T>(f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    let cancel = crate::core::cancel::current();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            match cancel {
                Some(token) => tokio::select! {
                    r = f => r,
                    _ = token.cancelled() => Err(anyhow::anyhow!("cancelled by user")),
                },
                None => f.await,
            }
        })
    })
}

/// The set of tools advertised to the model. Insertion order is the advertised order.
/// Stored as `Arc<dyn Tool>` so a call can be moved onto a `spawn_blocking` thread (`'static`
/// requirement) without cloning the tool itself; `register` keeps the `Box` signature so the ~40
/// existing call sites compile unchanged.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<std::sync::Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(std::sync::Arc::from(tool));
    }

    /// Look up a tool by exact name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|a| a.as_ref())
    }

    /// Look up a tool by exact name as an owned handle — for moving into a `spawn_blocking`
    /// closure (which needs `'static`).
    pub fn get_arc(&self, name: &str) -> Option<std::sync::Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.name() == name).cloned()
    }

    /// The `tools` array for the chat request.
    pub fn defs(&self) -> Vec<ToolDef> {
        self.tools
            .iter()
            .map(|t| ToolDef::function(t.name(), t.description(), t.parameters()))
            .collect()
    }

    /// The registered tool names (advertised order) — used to publish the live tool surface so the
    /// skills index can hide skills whose `requires:` tool isn't present in this build/session.
    pub fn names(&self) -> Vec<String> {
        self.tools.iter().map(|t| t.name().to_string()).collect()
    }

    /// Keep only tools for which `keep(name)` is true (Hermes `disabled_toolsets` filter).
    pub fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&str) -> bool,
    {
        self.tools.retain(|t| keep(t.name()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PINS the tokio behavior `block_for_tool` depends on: `block_in_place` + `Handle::block_on`
    /// must work unchanged INSIDE a `spawn_blocking` thread (context propagates; block_in_place is
    /// a pass-through off a worker). If a tokio upgrade breaks this, the async execution core's
    /// parallel path breaks with it — this test makes that loud and local.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_works_inside_spawn_blocking() {
        // On a spawn_blocking thread…
        let via_blocking = tokio::task::spawn_blocking(|| {
            block_for_tool(async { Ok::<_, anyhow::Error>(41 + 1) })
        })
        .await
        .expect("spawn_blocking join");
        assert_eq!(via_blocking.unwrap(), 42);
        // …and on a worker thread (the classic serial path).
        let on_worker = block_for_tool(async { Ok::<_, anyhow::Error>("ok") });
        assert_eq!(on_worker.unwrap(), "ok");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn bridge_observes_the_scoped_turn_token() {
        let token = crate::core::cancel::TurnCancel::new();
        token.cancel();
        let err = tokio::task::spawn_blocking(move || {
            crate::core::cancel::with_current(token, || {
                block_for_tool(async {
                    std::future::pending::<()>().await;
                    Ok::<_, anyhow::Error>(())
                })
            })
        })
        .await
        .expect("spawn_blocking join")
        .unwrap_err();
        assert!(err.to_string().contains("cancelled by user"), "{err}");
    }
}
