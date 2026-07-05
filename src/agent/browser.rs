//! Browser automation over the Chrome DevTools Protocol — OPT-IN (`--features browser`, default OFF
//! like `dense`). Drives an EXISTING local Chrome/Edge/Brave; it NEVER bundles a browser engine.
//!
//! Why this stays a single static binary: CDP's local endpoint is a plain `ws://127.0.0.1:<port>`
//! WebSocket (never TLS), so `tokio-tungstenite` is taken with NO tls feature → pure-Rust, no
//! `*-sys`/openssl, no C toolchain. Target discovery reuses the in-tree `reqwest`. Rejected (per the
//! roadmap): embedded engines (servo/CEF → C++), Playwright/Puppeteer (Node), the cloud/stealth half.
//!
//! Use it: launch a browser with remote debugging, then ask the agent to drive it —
//!   `chrome --remote-debugging-port=9222`   (or `msedge` / `brave`), override host via `NG_BROWSER_CDP`.
//!
//! Tools (top-level only): `browser_navigate`, `browser_snapshot` (a11y tree with `@ref` ids),
//! `browser_click`/`browser_type` (by `@ref`), `browser_eval` (run JS — the "debug localhost:3000"
//! power tool). All share ONE process-global connection so navigate→snapshot→click keep page state;
//! all run serially (`is_concurrency_safe()=false`) so the shared stream is never interleaved.

use crate::agent::tools::{Tool, ToolRegistry};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use once_cell::sync::Lazy;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Default CDP host:port; override with `NG_BROWSER_CDP` (e.g. `127.0.0.1:9333`).
const CDP_DEFAULT: &str = "127.0.0.1:9222";
/// Per-op wall-clock cap so a wedged page can't freeze the agent loop.
const OP_TIMEOUT: Duration = Duration::from_secs(30);
/// Cap on snapshot nodes — the a11y tree of a big SPA is huge; keep the injected slice bounded.
const SNAPSHOT_MAX_NODES: usize = 200;

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

/// The one shared CDP connection (lazy; reused across calls so the `@ref` map + page survive between
/// snapshot and click). Browser tools are serial, so the send-then-read-until-id loop is race-free.
static CLIENT: Lazy<Mutex<Option<CdpClient>>> = Lazy::new(|| Mutex::new(None));

struct CdpClient {
    ws: WsStream,
    next_id: u64,
    /// `@ref` number → DOM backendNodeId, rebuilt by each `browser_snapshot`.
    refs: HashMap<u32, i64>,
}

impl CdpClient {
    /// Send one CDP command and read until its matching response id (events / other ids skipped).
    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let msg = json!({"id": id, "method": method, "params": params});
        self.ws.send(WsMessage::Text(msg.to_string())).await.context("CDP send")?;
        let read = async {
            loop {
                match self.ws.next().await {
                    Some(Ok(WsMessage::Text(txt))) => {
                        let v: Value = match serde_json::from_str(&txt) {
                            Ok(v) => v,
                            Err(_) => continue,
                        };
                        if v.get("id").and_then(|i| i.as_u64()) == Some(id) {
                            if let Some(err) = v.get("error") {
                                let m = err.get("message").and_then(|m| m.as_str()).unwrap_or("CDP error");
                                bail!("CDP {method} error: {m}");
                            }
                            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                        }
                        // else: a protocol event or a stale response → ignore, keep reading
                    }
                    Some(Ok(WsMessage::Close(_))) | None => bail!("CDP connection closed"),
                    Some(Ok(_)) => continue, // ping/pong/binary
                    Some(Err(e)) => bail!("CDP read error: {e}"),
                }
            }
        };
        tokio::time::timeout(OP_TIMEOUT, read)
            .await
            .map_err(|_| anyhow!("CDP {method} timed out after {}s", OP_TIMEOUT.as_secs()))?
    }

    /// Resolve a snapshot `@ref` to a live JS object id (for click/type via `Runtime.callFunctionOn`).
    async fn object_for_ref(&mut self, r: u32) -> Result<String> {
        let backend = *self.refs.get(&r).ok_or_else(|| {
            anyhow!("unknown @ref @{r} — call browser_snapshot first (refs reset every snapshot)")
        })?;
        let resolved = self.call("DOM.resolveNode", json!({"backendNodeId": backend})).await?;
        resolved
            .get("object")
            .and_then(|o| o.get("objectId"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| anyhow!("@{r} is no longer in the DOM — re-snapshot the page"))
    }
}

/// Discover a page target's websocket URL from the local CDP HTTP endpoint.
async fn discover_ws_url() -> Result<String> {
    let host = std::env::var("NG_BROWSER_CDP").unwrap_or_else(|_| CDP_DEFAULT.to_string());
    let url = format!("http://{host}/json");
    let resp = reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .with_context(|| {
            format!(
                "no Chrome DevTools endpoint at {host} — launch a browser with remote debugging, e.g. \
                 `chrome --remote-debugging-port=9222` (or set NG_BROWSER_CDP)"
            )
        })?;
    let targets: Value = resp.json().await.context("parsing the CDP /json target list")?;
    let arr = targets.as_array().context("CDP /json did not return a list")?;
    let page = arr
        .iter()
        .find(|t| t.get("type").and_then(|v| v.as_str()) == Some("page"))
        .or_else(|| arr.first())
        .context("no open page/tab in the browser to attach to")?;
    page.get("webSocketDebuggerUrl")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .context("the target has no webSocketDebuggerUrl")
}

/// Open a fresh CDP connection and enable the domains we use.
async fn connect() -> Result<CdpClient> {
    let ws_url = discover_ws_url().await?;
    let (ws, _) = tokio_tungstenite::connect_async(&ws_url)
        .await
        .with_context(|| format!("connecting to the CDP websocket {ws_url}"))?;
    let mut c = CdpClient { ws, next_id: 1, refs: HashMap::new() };
    // Best-effort domain enables (a browser that doesn't support one shouldn't abort the session).
    let _ = c.call("Page.enable", json!({})).await;
    let _ = c.call("DOM.enable", json!({})).await;
    let _ = c.call("Runtime.enable", json!({})).await;
    let _ = c.call("Accessibility.enable", json!({})).await;
    Ok(c)
}

/// Run an op against the shared connection, connecting on first use. A connection-level failure
/// drops the client so the next call reconnects (a logical CDP error keeps it — preserving the
/// `@ref` map). The closure returns a boxed future (the canonical pattern for a closure whose future
/// borrows its `&mut` argument). The sync `Tool::execute` reaches this via `block`.
async fn with_cdp<T>(
    f: impl for<'a> FnOnce(&'a mut CdpClient) -> Pin<Box<dyn Future<Output = Result<T>> + 'a>>,
) -> Result<T> {
    let mut guard = CLIENT.lock().await;
    if guard.is_none() {
        *guard = Some(connect().await?);
    }
    let res = f(guard.as_mut().unwrap()).await;
    if let Err(e) = &res {
        let m = e.to_string();
        if m.contains("closed") || m.contains("read error") || m.contains("CDP send") || m.contains("timed out") {
            *guard = None; // drop a broken connection; the next call reconnects fresh
        }
    }
    res
}

/// Bridge the async CDP op to the sync `Tool::execute` — the shared cancel-aware bridge (valid on
/// workers AND spawn_blocking threads; Esc aborts a hung CDP round-trip).
fn block<T>(f: impl Future<Output = Result<T>>) -> Result<T> {
    crate::agent::tools::block_for_tool(f)
}

/// Parse a `@ref` argument: accepts `3`, `"3"`, or `"@3"`.
fn parse_ref(v: &Value) -> Result<u32> {
    if let Some(n) = v.as_u64() {
        return u32::try_from(n).map_err(|_| anyhow!("ref out of range"));
    }
    let s = v.as_str().context("'ref' must be a number or \"@N\" string")?;
    s.trim().trim_start_matches('@').parse::<u32>().with_context(|| format!("bad ref '{s}'"))
}

/// Roles worth surfacing even without an accessible name (interactive controls).
fn is_interactive_role(role: &str) -> bool {
    matches!(
        role,
        "button" | "link" | "textbox" | "searchbox" | "checkbox" | "radio" | "combobox" | "listbox"
            | "option" | "menuitem" | "menuitemcheckbox" | "menuitemradio" | "tab" | "switch" | "slider"
            | "spinbutton" | "textfield"
    )
}

/// Render an `Accessibility.getFullAXTree` node list into a compact, ref-tagged outline, populating
/// `refs` (ref number → backendNodeId). Pure (no IO) so it is unit-testable. Caps at `max` nodes.
fn render_ax_tree(nodes: &[Value], refs: &mut HashMap<u32, i64>, max: usize) -> String {
    refs.clear();
    let mut out = String::new();
    let mut n: u32 = 0;
    for node in nodes {
        if node.get("ignored").and_then(|v| v.as_bool()).unwrap_or(false) {
            continue;
        }
        let role = node.get("role").and_then(|r| r.get("value")).and_then(|v| v.as_str()).unwrap_or("");
        let name = node.get("name").and_then(|r| r.get("value")).and_then(|v| v.as_str()).unwrap_or("").trim();
        if role.is_empty() {
            continue;
        }
        // surface named nodes + interactive controls; skip pure structural noise
        if name.is_empty() && !is_interactive_role(role) {
            continue;
        }
        let backend = match node.get("backendDOMNodeId").and_then(|v| v.as_i64()) {
            Some(b) => b,
            None => continue,
        };
        n += 1;
        refs.insert(n, backend);
        let name_disp: String = name.chars().take(80).collect();
        if name_disp.is_empty() {
            out.push_str(&format!("[@{n}] {role}\n"));
        } else {
            out.push_str(&format!("[@{n}] {role} \"{name_disp}\"\n"));
        }
        if n as usize >= max {
            out.push_str(&format!("… (truncated at {max} nodes — interact via @ref, or browser_eval for more)\n"));
            break;
        }
    }
    if n == 0 {
        return "(no accessible elements found — the page may be blank or still loading)".to_string();
    }
    out.trim_end().to_string()
}

// ── tools ────────────────────────────────────────────────────────────────────────

pub struct BrowserNavigate;
impl Tool for BrowserNavigate {
    fn name(&self) -> &str {
        "browser_navigate"
    }
    fn description(&self) -> &str {
        "Open a URL in the connected local browser (Chrome/Edge/Brave launched with \
         --remote-debugging-port). Waits for the page to load, returns its title. Use to start a \
         browsing/debug session (e.g. your localhost dev server); then browser_snapshot to see the page."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"url":{"type":"string","description":"absolute URL (http/https/file)"}},"required":["url"],"additionalProperties":false})
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let url = args.get("url").and_then(|v| v.as_str()).context("missing 'url'")?.to_string();
        block(with_cdp(|c| Box::pin(async move {
            c.call("Page.navigate", json!({"url": url})).await?;
            // Poll readyState rather than racing a load event (simpler + robust across CDP versions).
            let deadline = OP_TIMEOUT;
            let start = tokio::time::Instant::now();
            loop {
                let r = c
                    .call("Runtime.evaluate", json!({"expression":"document.readyState","returnByValue":true}))
                    .await?;
                let state = r.get("result").and_then(|x| x.get("value")).and_then(|v| v.as_str()).unwrap_or("");
                if state == "complete" || state == "interactive" || start.elapsed() > deadline {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
            let t = c
                .call("Runtime.evaluate", json!({"expression":"document.title","returnByValue":true}))
                .await?;
            let title = t.get("result").and_then(|x| x.get("value")).and_then(|v| v.as_str()).unwrap_or("");
            Ok(format!("navigated — title: \"{title}\" (call browser_snapshot to see elements)"))
        })))
    }
}

pub struct BrowserSnapshot;
impl Tool for BrowserSnapshot {
    fn name(&self) -> &str {
        "browser_snapshot"
    }
    fn description(&self) -> &str {
        "Read the current page's accessibility tree as a compact outline of `[@ref] role \"name\"` \
         lines. The @ref ids are how you act on elements (browser_click/browser_type). Read-only. \
         Re-run after the page changes — refs are reassigned every snapshot."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{},"additionalProperties":false})
    }
    fn is_destructive(&self) -> bool {
        false
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, _args: &Value) -> Result<String> {
        block(with_cdp(|c| Box::pin(async move {
            let tree = c.call("Accessibility.getFullAXTree", json!({})).await?;
            let nodes = tree.get("nodes").and_then(|n| n.as_array()).cloned().unwrap_or_default();
            Ok(render_ax_tree(&nodes, &mut c.refs, SNAPSHOT_MAX_NODES))
        })))
    }
}

pub struct BrowserClick;
impl Tool for BrowserClick {
    fn name(&self) -> &str {
        "browser_click"
    }
    fn description(&self) -> &str {
        "Click an element by its @ref from the latest browser_snapshot. Scrolls it into view first. \
         Re-snapshot afterwards to see the result."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"ref":{"type":["integer","string"],"description":"the @ref number from browser_snapshot"}},"required":["ref"],"additionalProperties":false})
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let r = parse_ref(args.get("ref").context("missing 'ref'")?)?;
        block(with_cdp(|c| Box::pin(async move {
            let object_id = c.object_for_ref(r).await?;
            c.call(
                "Runtime.callFunctionOn",
                json!({"objectId": object_id, "functionDeclaration": "function(){ this.scrollIntoView({block:'center'}); this.click(); }"}),
            )
            .await?;
            Ok(format!("clicked @{r} (browser_snapshot to see the result)"))
        })))
    }
}

pub struct BrowserType;
impl Tool for BrowserType {
    fn name(&self) -> &str {
        "browser_type"
    }
    fn description(&self) -> &str {
        "Type text into an input/textarea by its @ref from the latest browser_snapshot (focuses, \
         sets the value, and fires input+change events). For buttons/links use browser_click."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"ref":{"type":["integer","string"]},"text":{"type":"string"}},"required":["ref","text"],"additionalProperties":false})
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let r = parse_ref(args.get("ref").context("missing 'ref'")?)?;
        let text = args.get("text").and_then(|v| v.as_str()).context("missing 'text'")?.to_string();
        block(with_cdp(|c| Box::pin(async move {
            let object_id = c.object_for_ref(r).await?;
            c.call(
                "Runtime.callFunctionOn",
                json!({
                    "objectId": object_id,
                    "functionDeclaration": "function(t){ this.focus(); if('value' in this){ this.value=t; this.dispatchEvent(new Event('input',{bubbles:true})); this.dispatchEvent(new Event('change',{bubbles:true})); } else { this.textContent=t; } }",
                    "arguments": [{"value": text}]
                }),
            )
            .await?;
            Ok(format!("typed into @{r}"))
        })))
    }
}

pub struct BrowserEval;
impl Tool for BrowserEval {
    fn name(&self) -> &str {
        "browser_eval"
    }
    fn description(&self) -> &str {
        "Run a JavaScript expression in the current page and return its result (JSON when possible). \
         The debug power tool: read DOM/localStorage/app state, await fetches, inspect errors. Prefer \
         browser_click/browser_type for normal interaction; use this when a snapshot can't express it."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","properties":{"expression":{"type":"string","description":"a JS expression; may be an async expression / Promise"}},"required":["expression"],"additionalProperties":false})
    }
    fn is_destructive(&self) -> bool {
        true
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let expr = args.get("expression").and_then(|v| v.as_str()).context("missing 'expression'")?.to_string();
        block(with_cdp(|c| Box::pin(async move {
            let r = c
                .call(
                    "Runtime.evaluate",
                    json!({"expression": expr, "returnByValue": true, "awaitPromise": true}),
                )
                .await?;
            if let Some(ex) = r.get("exceptionDetails") {
                let txt = ex.get("exception").and_then(|e| e.get("description")).and_then(|v| v.as_str())
                    .or_else(|| ex.get("text").and_then(|v| v.as_str()))
                    .unwrap_or("JS exception");
                return Ok(format!("error: {txt}"));
            }
            let result = r.get("result").cloned().unwrap_or(Value::Null);
            // returnByValue → a `value`; else fall back to the type/description.
            if let Some(v) = result.get("value") {
                Ok(serde_json::to_string(v).unwrap_or_else(|_| v.to_string()))
            } else {
                let desc = result.get("description").and_then(|v| v.as_str())
                    .or_else(|| result.get("type").and_then(|v| v.as_str()))
                    .unwrap_or("undefined");
                Ok(desc.to_string())
            }
        })))
    }
}

/// Register the browser tools into a registry (top-level only). Called from `default_registry_in`
/// under `#[cfg(feature = "browser")]`.
pub fn register_browser_tools(r: &mut ToolRegistry) {
    r.register(Box::new(BrowserNavigate));
    r.register(Box::new(BrowserSnapshot));
    r.register(Box::new(BrowserClick));
    r.register(Box::new(BrowserType));
    r.register(Box::new(BrowserEval));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ref_accepts_number_and_at_string() {
        assert_eq!(parse_ref(&json!(3)).unwrap(), 3);
        assert_eq!(parse_ref(&json!("3")).unwrap(), 3);
        assert_eq!(parse_ref(&json!("@7")).unwrap(), 7);
        assert!(parse_ref(&json!("nope")).is_err());
        assert!(parse_ref(&json!(true)).is_err());
    }

    #[test]
    fn render_ax_tree_tags_refs_and_skips_noise() {
        let nodes = vec![
            json!({"role":{"value":"button"},"name":{"value":"Submit"},"backendDOMNodeId":11}),
            json!({"role":{"value":"generic"},"name":{"value":""},"backendDOMNodeId":12}), // unnamed non-interactive → skipped
            json!({"ignored":true,"role":{"value":"link"},"name":{"value":"x"},"backendDOMNodeId":13}), // ignored → skipped
            json!({"role":{"value":"textbox"},"name":{"value":""},"backendDOMNodeId":14}), // interactive, unnamed → kept
            json!({"role":{"value":"heading"},"name":{"value":"Welcome"},"backendDOMNodeId":15}),
        ];
        let mut refs = HashMap::new();
        let out = render_ax_tree(&nodes, &mut refs, 200);
        assert!(out.contains("[@1] button \"Submit\""));
        assert!(out.contains("[@2] textbox"), "interactive unnamed control is kept: {out}");
        assert!(out.contains("[@3] heading \"Welcome\""));
        assert!(!out.contains("generic"), "unnamed non-interactive node is skipped");
        // refs map the rendered order to the right backend node ids
        assert_eq!(refs.get(&1), Some(&11));
        assert_eq!(refs.get(&2), Some(&14));
        assert_eq!(refs.get(&3), Some(&15));
    }

    #[test]
    fn render_ax_tree_caps_node_count() {
        let nodes: Vec<Value> = (0..50)
            .map(|i| json!({"role":{"value":"button"},"name":{"value":format!("b{i}")},"backendDOMNodeId":i}))
            .collect();
        let mut refs = HashMap::new();
        let out = render_ax_tree(&nodes, &mut refs, 5);
        assert!(out.contains("truncated at 5 nodes"));
        assert_eq!(refs.len(), 5, "refs bounded to the cap");
    }

    #[test]
    fn render_ax_tree_empty_is_explained() {
        let mut refs = HashMap::new();
        let out = render_ax_tree(&[], &mut refs, 200);
        assert!(out.contains("no accessible elements"));
    }
}
