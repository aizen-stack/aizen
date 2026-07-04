//! Model-facing LSP tools (v1: navigation + diagnostics). Each is synchronous (`Tool::execute`)
//! and bridges to the async [`LspManager`](super::LspManager) via its blocking query API. Any
//! failure (LSP off, server not installed, no project, timeout) returns a clean message string so
//! the agent degrades to text search rather than aborting the turn.

use crate::agent::lsp::LSP;
use crate::agent::tools::Tool;
use anyhow::{anyhow, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Shared arg plumbing: required non-empty string.
fn req_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("missing `{key}`"))
}

/// Shared arg plumbing: optional `file` confined to the project root → anchor path (project root
/// when absent).
fn anchor_from(root: &Path, args: &Value) -> Result<PathBuf> {
    match args.get("file").and_then(|v| v.as_str()) {
        Some(f) if !f.trim().is_empty() => crate::agent::builtin::confine(root, f.trim(), true),
        _ => Ok(root.to_path_buf()),
    }
}

/// `lsp_references` — find every reference / call-site of a symbol BY NAME (type-aware, cross-file,
/// no comment/string false positives), via the language server. Read-only.
pub struct LspReferences {
    root: PathBuf,
}

impl LspReferences {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for LspReferences {
    fn name(&self) -> &str {
        "lsp_references"
    }

    fn description(&self) -> &str {
        "Find every reference / call-site of a symbol across the project, BY NAME, using the \
         language server. Type-aware and exact: unlike text search it skips comments, strings, and \
         unrelated same-named symbols, and spans all files. Use it for impact analysis before \
         changing a function/type, or to find all call sites to update. Returns `path:line:col  \
         snippet` per hit. Read-only."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "symbol": {
                    "type": "string",
                    "description": "the symbol name to find references to (function / type / method / variable name)"
                },
                "file": {
                    "type": "string",
                    "description": "optional: a file in the relevant project — helps pick the language and disambiguate same-named symbols; defaults to the working directory"
                },
                "include_declaration": {
                    "type": "boolean",
                    "description": "include the definition itself in the results (default true)"
                }
            },
            "required": ["symbol"]
        })
    }

    // Touches the shared LSP manager state → serial path (like memory_search / todo_write).
    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn execute(&self, args: &Value) -> Result<String> {
        let symbol = req_str(args, "symbol")?;
        let include_decl = args.get("include_declaration").and_then(|v| v.as_bool()).unwrap_or(true);
        let anchor = anchor_from(&self.root, args)?;
        LSP.references(&anchor, symbol, include_decl)
    }
}

/// `lsp_definition` — jump to a symbol's definition BY NAME and return the definition's source text
/// inline (no navigate-then-read round-trip). Read-only.
pub struct LspDefinition {
    root: PathBuf,
}

impl LspDefinition {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for LspDefinition {
    fn name(&self) -> &str {
        "lsp_definition"
    }

    fn description(&self) -> &str {
        "Go to the definition of a symbol BY NAME via the language server and return the \
         definition's source code inline (file:line plus the item's text), so you see the signature \
         and body without a separate file read. Works for named items (functions, types, methods, \
         constants) — not local variables. Read-only."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "symbol": {
                    "type": "string",
                    "description": "the symbol name to resolve (function / type / method / constant name)"
                },
                "file": {
                    "type": "string",
                    "description": "optional: a file in the relevant project — helps pick the language and disambiguate same-named symbols; defaults to the working directory"
                }
            },
            "required": ["symbol"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn execute(&self, args: &Value) -> Result<String> {
        let symbol = req_str(args, "symbol")?;
        let anchor = anchor_from(&self.root, args)?;
        LSP.definition(&anchor, symbol)
    }
}

/// `lsp_document_symbols` — structural outline of one file (symbols + lines, no bodies). Read-only.
pub struct LspDocumentSymbols {
    root: PathBuf,
}

impl LspDocumentSymbols {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for LspDocumentSymbols {
    fn name(&self) -> &str {
        "lsp_document_symbols"
    }

    fn description(&self) -> &str {
        "Outline one file via the language server: every symbol (functions, types, methods, fields) \
         with its kind, nesting, and line number — no bodies. Use it to orient in an unfamiliar file \
         and then read only the symbol you need, instead of dumping the whole file. Read-only."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "file": {
                    "type": "string",
                    "description": "the file to outline (path relative to the project root)"
                }
            },
            "required": ["file"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn execute(&self, args: &Value) -> Result<String> {
        let file = req_str(args, "file")?;
        let path = crate::agent::builtin::confine(&self.root, file, true)?;
        LSP.document_symbols(&path)
    }
}

/// `lsp_workspace_symbol` — project-wide fuzzy symbol search by name. Read-only.
pub struct LspWorkspaceSymbol {
    root: PathBuf,
}

impl LspWorkspaceSymbol {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for LspWorkspaceSymbol {
    fn name(&self) -> &str {
        "lsp_workspace_symbol"
    }

    fn description(&self) -> &str {
        "Search the whole project for symbols by (fuzzy) name via the language server — \"where is \
         X defined?\" across every file, returning name, kind, and location. Type-aware: matches \
         declared symbols only, never comments/strings. Read-only."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "query": {
                    "type": "string",
                    "description": "the symbol name (or name fragment) to search for"
                },
                "file": {
                    "type": "string",
                    "description": "optional: a file in the relevant project — picks the language in a mixed repo; defaults to the working directory"
                },
                "max": {
                    "type": "integer",
                    "description": "max hits to return (default 30, cap 100)"
                }
            },
            "required": ["query"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn execute(&self, args: &Value) -> Result<String> {
        let query = req_str(args, "query")?;
        let max = args.get("max").and_then(|v| v.as_u64()).unwrap_or(30) as usize;
        let anchor = anchor_from(&self.root, args)?;
        LSP.workspace_symbols(&anchor, query, max.clamp(1, 100))
    }
}

/// `lsp_diagnostics` — current compiler/linter diagnostics for one file, on demand. Read-only.
pub struct LspDiagnostics {
    root: PathBuf,
}

impl LspDiagnostics {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for LspDiagnostics {
    fn name(&self) -> &str {
        "lsp_diagnostics"
    }

    fn description(&self) -> &str {
        "Get the language server's current diagnostics (errors / warnings / lints) for one file — \
         fast, incremental feedback after an edit without running a full build. Complements (does \
         not replace) the project's real build/test commands. Read-only."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "file": {
                    "type": "string",
                    "description": "the file to check (path relative to the project root)"
                }
            },
            "required": ["file"]
        })
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn execute(&self, args: &Value) -> Result<String> {
        let file = req_str(args, "file")?;
        let path = crate::agent::builtin::confine(&self.root, file, true)?;
        LSP.diagnostics(&path)
    }
}
