//! The tool surface: a `Tool` trait + a registry the agent loop advertises to the model.
//!
//! Discipline (the repo's hard-won lesson — semantic OVERLAP between tools is the killer,
//! not the count): names are `category_action`, one canonical tool per capability, and each
//! description says when to use it AND when NOT to (point at the sibling tool instead).
//! Execution is synchronous — tools are local (memory / fs / subprocess).

use crate::core::types::ToolDef;
use anyhow::Result;
use serde_json::Value;

/// A capability the model may invoke.
///
/// `Send + Sync` is REQUIRED: the agent loop runs a batch of concurrency-safe tool calls in
/// parallel via `std::thread::scope` (see `agent::execute_parallel`), which shares `&dyn Tool`
/// across threads — that needs `Sync`, and moving the borrow into a scoped thread needs `Send`.
/// All built-in tools hold only `PathBuf`/unit, so they satisfy this trivially. A future tool
/// holding a non-`Sync` field (`Rc`, `RefCell`, …) will fail to compile here — which is the
/// correct guardrail, not a false alarm: such a tool could not be run concurrently anyway.
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
    /// Read-only & side-effect-free → eligible for concurrent execution in a parallel batch
    /// (`agent::execute_parallel`). Destructive tools also set this `false`.
    fn is_concurrency_safe(&self) -> bool {
        true
    }
    /// Run the tool. `args` is the parsed (object) arguments. Return the result text; return
    /// an `Err` for a real failure — the loop feeds the error back to the model to recover.
    fn execute(&self, args: &Value) -> Result<String>;
}

/// The set of tools advertised to the model. Insertion order is the advertised order.
#[derive(Default)]
pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, tool: Box<dyn Tool>) {
        self.tools.push(tool);
    }

    /// Look up a tool by exact name.
    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools.iter().find(|t| t.name() == name).map(|b| b.as_ref())
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
}
