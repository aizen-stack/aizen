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

    /// Every registered tool. Lets a test sweep the WHOLE surface (rather than a hand-listed
    /// sample that silently stops covering tools added later).
    #[cfg(test)]
    pub fn tools(&self) -> impl Iterator<Item = &std::sync::Arc<dyn Tool>> {
        self.tools.iter()
    }

    /// Keep only tools for which `keep(name)` is true (Hermes `disabled_toolsets` filter).
    pub fn retain<F>(&mut self, mut keep: F)
    where
        F: FnMut(&str) -> bool,
    {
        self.tools.retain(|t| keep(t.name()));
    }
}

// ── schema-driven argument repair ───────────────────────────────────────────────
//
// A model that names an argument `q` instead of `query`, or wraps the whole object in `{"input": …}`,
// costs a full round trip: the call fails with `missing required string arg`, the model reads the
// error, and calls again. That is one wasted tool call plus one wasted assistant turn, per slip.
//
// These helpers close that loop by reading each tool's OWN `parameters()` schema, so every tool —
// builtin, LSP, skills, MCP — is covered by one implementation, and a tool added later is covered
// without touching this file. The alternative (per-tool alias lists, as `web_search::extract_queries`
// does) cannot be replicated across ~40 tools and drifts from the schema the moment either side
// changes.
//
// The rules only ever RESHAPE what the model sent; they never invent a value. Anything ambiguous is
// left alone so the model gets a real error instead of a silently wrong call.

/// A schema's `required` keys that are declared `type: "string"`.
fn required_string_keys(schema: &Value) -> Vec<String> {
    let props = schema.get("properties").and_then(Value::as_object);
    schema
        .get("required")
        .and_then(Value::as_array)
        .map(|req| {
            req.iter()
                .filter_map(Value::as_str)
                .filter(|k| {
                    // Absent `properties` entry ⇒ assume string: the key is required either way, and
                    // a repair that hands it a string is still better than a missing-arg failure.
                    props
                        .and_then(|p| p.get(*k))
                        .and_then(|s| s.get("type"))
                        .and_then(Value::as_str)
                        .map(|t| t == "string")
                        .unwrap_or(true)
                })
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Is this arg present AND usable as a non-empty string?
fn has_usable_string(args: &Value, key: &str) -> bool {
    args.get(key)
        .and_then(Value::as_str)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// The `required` string keys `schema` demands that `args` does not usably supply.
///
/// Reads the schema directly, so it applies to every tool in the registry (and to MCP tools, whose
/// server-provided schema passes through verbatim). A schema with no `required` array — an open MCP
/// schema, or `web_search`, which enforces "query or queries" at runtime instead — yields an empty
/// list, making every caller a no-op.
pub fn missing_required_strings(schema: &Value, args: &Value) -> Vec<String> {
    required_string_keys(schema)
        .into_iter()
        .filter(|k| !has_usable_string(args, k))
        .collect()
}

/// Keys the schema declares in `properties`.
fn declared_keys(schema: &Value) -> Vec<String> {
    schema
        .get("properties")
        .and_then(Value::as_object)
        .map(|p| p.keys().cloned().collect())
        .unwrap_or_default()
}

/// The single-key wrapper names models most often wrap a whole argument object in.
const WRAPPER_KEYS: &[&str] = &["input", "args", "arguments", "parameters", "params", "kwargs"];

/// Repair argument-shape slips that can be inferred WITH CERTAINTY. Returns the corrected arguments
/// plus a short human description of what changed (for the trace line), or `None` when there is
/// nothing to fix or the fix would be a guess.
///
/// Three rules, applied in order:
///
/// 1. **UNWRAP** — the whole object is nested under one wrapper key (`{"input": {"path": …}}`).
///    Accepted only when the inner object satisfies the schema's required keys and the outer object
///    has nothing else, so it cannot swallow a real single-argument call.
/// 2. **ALIAS** — exactly ONE required string key is missing and the arguments carry exactly ONE
///    undeclared string key. The one-to-one condition is what makes this safe: with two candidates on
///    either side the mapping is a coin flip, so it is refused.
/// 3. **COERCE** — a required string key is present but as a number/bool (stringified) or as a
///    single-element string array (unwrapped). No information is created.
///
/// Deliberately NOT done: filling a missing key from a default, dropping undeclared keys (a strict
/// gateway already rejected those upstream, and dropping could discard something meaningful), or
/// renaming when the schema declares no `required`.
pub fn repair_args(schema: &Value, args: &Value) -> Option<(Value, String)> {
    let obj = args.as_object()?;
    let required = required_string_keys(schema);
    if required.is_empty() {
        return None; // nothing to satisfy ⇒ nothing to repair
    }
    if required.iter().all(|k| has_usable_string(args, k)) {
        return None; // already valid: the overwhelmingly common path, allocation-free
    }

    // ── rule 1: UNWRAP ────────────────────────────────────────────────────────
    if obj.len() == 1 {
        let (k, v) = obj.iter().next()?;
        if WRAPPER_KEYS.contains(&k.as_str()) && v.is_object() {
            let inner_ok = required.iter().all(|r| has_usable_string(v, r));
            if inner_ok {
                return Some((v.clone(), format!("unwrapped args from '{k}'")));
            }
        }
    }

    let declared = declared_keys(schema);
    let mut fixed = obj.clone();
    let mut notes: Vec<String> = Vec::new();

    // ── rule 2: ALIAS ─────────────────────────────────────────────────────────
    let missing: Vec<String> = required
        .iter()
        .filter(|k| !has_usable_string(args, k))
        .cloned()
        .collect();
    if missing.len() == 1 {
        let strays: Vec<String> = obj
            .iter()
            .filter(|(k, v)| {
                !declared.contains(*k)
                    && v.as_str().map(|s| !s.trim().is_empty()).unwrap_or(false)
            })
            .map(|(k, _)| k.clone())
            .collect();
        if strays.len() == 1 {
            let from = &strays[0];
            let to = &missing[0];
            if let Some(v) = fixed.remove(from) {
                fixed.insert(to.clone(), v);
                notes.push(format!("arg '{from}' → '{to}'"));
            }
        }
    }

    // ── rule 3: COERCE ────────────────────────────────────────────────────────
    for key in &required {
        if has_usable_string(&Value::Object(fixed.clone()), key) {
            continue;
        }
        let coerced = match fixed.get(key) {
            Some(Value::Number(n)) => Some(n.to_string()),
            Some(Value::Bool(b)) => Some(b.to_string()),
            Some(Value::Array(a)) if a.len() == 1 => a[0]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
            _ => None,
        };
        if let Some(s) = coerced {
            fixed.insert(key.clone(), Value::String(s));
            notes.push(format!("arg '{key}' coerced to string"));
        }
    }

    if notes.is_empty() {
        return None;
    }
    Some((Value::Object(fixed), notes.join(", ")))
}

/// The model-facing error for a call whose missing arguments could not be repaired. Carries the tool
/// name, what the schema actually requires, and what the model sent — so the retry is informed
/// rather than a second guess. Shared by every tool via the executor, which is why the per-tool
/// messages (`missing required string arg 'x'`) rarely surface any more.
pub fn missing_args_error(tool: &str, schema: &Value, args: &Value, missing: &[String]) -> String {
    let props = schema.get("properties").and_then(Value::as_object);
    let spec = required_string_keys(schema)
        .iter()
        .map(|k| {
            let ty = props
                .and_then(|p| p.get(k))
                .and_then(|s| s.get("type"))
                .and_then(Value::as_str)
                .unwrap_or("string");
            format!("{k} ({ty})")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let sent = serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string());
    let sent = if sent.chars().count() > 200 {
        let mut t: String = sent.chars().take(197).collect();
        t.push_str("...");
        t
    } else {
        sent
    };
    format!(
        "error: missing required arg{} {} for {tool} — required: {spec}. You sent: {sent}. \
         Call again using those exact key names.",
        if missing.len() == 1 { "" } else { "s" },
        missing
            .iter()
            .map(|m| format!("'{m}'"))
            .collect::<Vec<_>>()
            .join(", "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// `file_read`-shaped schema: one required string plus optional extras.
    fn path_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "start": {"type": "integer"},
                "number": {"type": "boolean"}
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    #[test]
    fn repair_unwraps_a_wrapped_args_object() {
        for wrapper in ["input", "args", "arguments", "parameters", "params", "kwargs"] {
            let args = json!({wrapper: {"path": "a.rs"}});
            let (fixed, what) = repair_args(&path_schema(), &args)
                .unwrap_or_else(|| panic!("wrapper '{wrapper}' must unwrap"));
            assert_eq!(fixed, json!({"path": "a.rs"}));
            assert!(what.contains(wrapper), "note names the wrapper: {what}");
        }
    }

    #[test]
    fn repair_does_not_unwrap_when_the_inner_object_still_lacks_the_key() {
        // `{"input": {...}}` that doesn't satisfy the schema is not a wrapper slip — leave it alone
        // so the model gets a real error instead of a differently-broken call.
        let args = json!({"input": {"nope": "a.rs"}});
        assert!(repair_args(&path_schema(), &args).is_none());
    }

    #[test]
    fn repair_maps_a_single_unknown_alias() {
        let (fixed, what) = repair_args(&path_schema(), &json!({"file": "a.rs"})).unwrap();
        assert_eq!(fixed, json!({"path": "a.rs"}));
        assert!(what.contains("'file'") && what.contains("'path'"), "{what}");

        // A different tool shape, same rule — this is why it isn't a per-tool alias table.
        let query_schema = json!({
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"]
        });
        let (fixed, _) = repair_args(&query_schema, &json!({"q": "hermes"})).unwrap();
        assert_eq!(fixed, json!({"query": "hermes"}));

        // Declared optional args ride along untouched.
        let (fixed, _) = repair_args(&path_schema(), &json!({"file": "a.rs", "start": 3})).unwrap();
        assert_eq!(fixed, json!({"path": "a.rs", "start": 3}));
    }

    #[test]
    fn repair_refuses_when_ambiguous() {
        // TWO stray keys → which one is the path? Refuse rather than guess.
        assert!(
            repair_args(&path_schema(), &json!({"file": "a.rs", "dest": "b.rs"})).is_none(),
            "two candidate strays must not be guessed"
        );
        // TWO missing required keys → a single stray cannot fill both.
        let two = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}, "body": {"type": "string"}},
            "required": ["name", "body"]
        });
        assert!(
            repair_args(&two, &json!({"title": "x"})).is_none(),
            "two missing keys must not be guessed"
        );
    }

    #[test]
    fn repair_coerces_scalar_and_single_element_array() {
        let (fixed, what) = repair_args(&path_schema(), &json!({"path": 123})).unwrap();
        assert_eq!(fixed, json!({"path": "123"}));
        assert!(what.contains("coerced"), "{what}");

        let (fixed, _) = repair_args(&path_schema(), &json!({"path": ["a.rs"]})).unwrap();
        assert_eq!(fixed, json!({"path": "a.rs"}));

        // A multi-element array is a real disagreement about arity — not ours to resolve.
        assert!(repair_args(&path_schema(), &json!({"path": ["a.rs", "b.rs"]})).is_none());
    }

    #[test]
    fn repair_is_a_noop_for_valid_args() {
        assert!(repair_args(&path_schema(), &json!({"path": "a.rs"})).is_none());
        // No `required` (an open MCP schema, or web_search's runtime-enforced pair) ⇒ never repaired.
        let open = json!({"type": "object", "additionalProperties": true});
        assert!(repair_args(&open, &json!({"anything": 1})).is_none());
        // Non-object arguments are not repairable.
        assert!(repair_args(&path_schema(), &json!("just a string")).is_none());
    }

    #[test]
    fn missing_required_strings_reports_only_unusable_keys() {
        assert!(missing_required_strings(&path_schema(), &json!({"path": "a.rs"})).is_empty());
        assert_eq!(
            missing_required_strings(&path_schema(), &json!({})),
            vec!["path".to_string()]
        );
        // Present but blank / wrong type still counts as missing.
        assert_eq!(
            missing_required_strings(&path_schema(), &json!({"path": "   "})),
            vec!["path".to_string()]
        );
        assert_eq!(
            missing_required_strings(&path_schema(), &json!({"path": 7})),
            vec!["path".to_string()]
        );
    }

    #[test]
    fn missing_args_error_carries_the_schema_and_what_was_sent() {
        let msg = missing_args_error(
            "file_glob",
            &json!({
                "type": "object",
                "properties": {"pattern": {"type": "string"}},
                "required": ["pattern"]
            }),
            &json!({"glb": "**/*.rs"}),
            &["pattern".to_string()],
        );
        assert!(msg.starts_with("error: "), "{msg}");
        assert!(msg.contains("file_glob"), "names the tool: {msg}");
        assert!(msg.contains("pattern (string)"), "states the schema: {msg}");
        assert!(msg.contains(r#"{"glb":"**/*.rs"}"#), "echoes the args: {msg}");
    }

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
