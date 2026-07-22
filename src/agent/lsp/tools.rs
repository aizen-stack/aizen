//! Model-facing LSP tools (v1: navigation + diagnostics). Each is synchronous (`Tool::execute`)
//! and bridges to the async [`LspManager`](super::LspManager) via its blocking query API. Any
//! failure (LSP off, server not installed, no project, timeout) returns a clean message string so
//! the agent degrades to text search rather than aborting the turn.

use crate::agent::lsp::{InsertWhere, LSP};
use crate::agent::tools::{Tool, WorkspaceEffect};
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

/// `symbol_replace` — rewrite an entire named symbol body via the language-server outline range.
/// Serena-style: one call replaces a function/type/method without dumping the file or thrashing
/// `old_string`. Destructive (writes disk); arms the post-edit verify gate + LSP fold.
pub struct SymbolReplace {
    root: PathBuf,
}

impl SymbolReplace {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for SymbolReplace {
    fn name(&self) -> &str {
        "symbol_replace"
    }

    fn description(&self) -> &str {
        "Replace the FULL body of a named symbol (function / type / method / const / …) using the \
         language server's outline range — no old_string, no whole-file rewrite. Prefer this over \
         file_edit when changing an entire item. Pass the complete new body (signature + body). \
         Optional `file` disambiguates same-named symbols. Destructive; returns a before→after \
         preview plus any new LSP diagnostics."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "symbol": {
                    "type": "string",
                    "description": "exact symbol name whose full body to replace"
                },
                "new_body": {
                    "type": "string",
                    "description": "complete new source for the symbol (including signature/header)"
                },
                "file": {
                    "type": "string",
                    "description": "optional: file containing the symbol (disambiguates same names)"
                }
            },
            "required": ["symbol", "new_body"]
        })
    }

    fn is_destructive(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn workspace_effect(&self, _args: &Value) -> WorkspaceEffect {
        WorkspaceEffect::Paths
    }

    fn execute(&self, args: &Value) -> Result<String> {
        use crate::agent::builtin::NOOP_WRITE_PREFIX;
        let symbol = req_str(args, "symbol")?;
        let new_body = req_str(args, "new_body")?;
        let anchor = anchor_from(&self.root, args)?;
        let plan = LSP.replace_symbol(&anchor, symbol, new_body)?;
        let old = std::fs::read_to_string(&plan.path)
            .map_err(|e| anyhow!("reading {}: {e}", plan.path.display()))?;
        if plan.new_content == old {
            let rel = plan.path.strip_prefix(&self.root).unwrap_or(&plan.path);
            return Ok(format!(
                "{NOOP_WRITE_PREFIX}: {} unchanged (symbol body identical)",
                rel.display()
            ));
        }
        crate::core::persist::compare_and_atomic_write(
            &plan.path,
            &plan.base_fingerprint,
            plan.new_content.as_bytes(),
        )
        .map_err(|e| anyhow!("stale symbolic edit: {e}"))?;
        let rel = plan.path.strip_prefix(&self.root).unwrap_or(&plan.path);
        let preview_old: String = plan.old_body.chars().take(400).collect();
        let preview_new: String = new_body.chars().take(400).collect();
        let mut out = format!(
            "replaced symbol '{}' in {} (lines {}-{})\n--- before ---\n{}\n--- after ---\n{}",
            plan.symbol,
            rel.display(),
            plan.start_line + 1,
            plan.end_line + 1,
            preview_old,
            preview_new,
        );
        if let Some(fb) = LSP.edit_feedback(&plan.path) {
            out.push('\n');
            out.push_str(&fb);
        }
        Ok(out)
    }
}

/// `symbol_insert` — insert text immediately before or after a named symbol's full range.
pub struct SymbolInsert {
    root: PathBuf,
}

impl SymbolInsert {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }
}

impl Tool for SymbolInsert {
    fn name(&self) -> &str {
        "symbol_insert"
    }

    fn description(&self) -> &str {
        "Insert source text immediately before or after a named symbol (function / type / method …) \
         using the language-server outline range. Use for adding a helper next to an existing item, \
         a method after another method, or a use/import near a type — without reading the whole file \
         or hunting line numbers. `where` is `before` or `after` (default `after`). Destructive."
    }

    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "symbol": {
                    "type": "string",
                    "description": "anchor symbol name (insert relative to its full body range)"
                },
                "text": {
                    "type": "string",
                    "description": "source text to insert (one or more lines)"
                },
                "where": {
                    "type": "string",
                    "description": "\"before\" or \"after\" the symbol (default \"after\")"
                },
                "file": {
                    "type": "string",
                    "description": "optional: file containing the symbol (disambiguates same names)"
                }
            },
            "required": ["symbol", "text"]
        })
    }

    fn is_destructive(&self) -> bool {
        true
    }

    fn is_concurrency_safe(&self) -> bool {
        false
    }

    fn workspace_effect(&self, _args: &Value) -> WorkspaceEffect {
        WorkspaceEffect::Paths
    }

    fn execute(&self, args: &Value) -> Result<String> {
        let symbol = req_str(args, "symbol")?;
        let text = req_str(args, "text")?;
        let where_raw = args
            .get("where")
            .and_then(|v| v.as_str())
            .unwrap_or("after")
            .trim()
            .to_ascii_lowercase();
        let where_ = match where_raw.as_str() {
            "before" | "pre" | "above" => InsertWhere::Before,
            "after" | "post" | "below" | "" => InsertWhere::After,
            other => {
                return Err(anyhow!(
                    "invalid `where` '{other}' — use \"before\" or \"after\""
                ))
            }
        };
        let anchor = anchor_from(&self.root, args)?;
        let plan = LSP.insert_at_symbol(&anchor, symbol, where_, text)?;
        crate::core::persist::compare_and_atomic_write(
            &plan.path,
            &plan.base_fingerprint,
            plan.new_content.as_bytes(),
        )
        .map_err(|e| anyhow!("stale symbolic edit: {e}"))?;
        let rel = plan.path.strip_prefix(&self.root).unwrap_or(&plan.path);
        let side = match where_ {
            InsertWhere::Before => "before",
            InsertWhere::After => "after",
        };
        let preview: String = text.chars().take(400).collect();
        let mut out = format!(
            "inserted {side} '{}' in {} (at line {})\n{}",
            plan.symbol,
            rel.display(),
            plan.start_line + 1,
            preview
        );
        if let Some(fb) = LSP.edit_feedback(&plan.path) {
            out.push('\n');
            out.push_str(&fb);
        }
        Ok(out)
    }
}
