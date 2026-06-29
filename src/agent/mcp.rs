//! Pure-Rust MCP (Model Context Protocol) **client** — turns ng's closed compile-time tool
//! surface into a user-configurable ecosystem. Each MCP server's tools are discovered at
//! registry-build time and wrapped as `dyn Tool` named `mcp_<server>_<tool>`.
//!
//! Scope (v1, deliberately the 80%-value core — see `.claude/plans/260625-hermes-feature-adoption/`):
//! - Two transports: **stdio** (a child process, newline-delimited JSON-RPC — the dominant local
//!   transport) and **Streamable HTTP** (POST + optional `text/event-stream` reply — the modern
//!   remote transport). The legacy 2024-11-05 HTTP+SSE two-endpoint dance is NOT implemented.
//! - **OAuth 2.1 (PKCE)** for remote servers that require sign-in (Linear/Notion/Slack/Gmail/…): the
//!   token lives in `~/.aizen/mcp-tokens/<key>.json`, is attached as a `Bearer` header, and is
//!   refreshed transparently (see `mcp_oauth`). `"auth": "oauth"` in a server's entry turns it on.
//! - Handshake: `initialize` → store serverInfo + negotiated protocolVersion → `notifications/
//!   initialized`. Then `tools/list` (cursor-paged) → wrap each tool.
//! - `include`/`exclude` per-server tool filtering (Hermes-style).
//! - External tools are **destructive-by-default** (approval-gated) UNLESS the server advertises
//!   `annotations.readOnlyHint == true` — then we trust it as read-only.
//!
//! Static-binary posture: hand-rolled JSON-RPC over `serde_json` + the existing
//! `reqwest`(rustls) / `tokio::process` — NO `*-sys` crate, NEVER the Python/TS MCP SDK.
//!
//! Async-from-sync: the `Tool` trait is sync; MCP I/O is async. We bridge with
//! `block_in_place` + the current runtime's `block_on` (same invariant as `web_tools`), so every
//! `McpTool` declares `is_concurrency_safe() = false` (it must stay on the serial runtime-worker
//! path where the bridge is valid). Connections are process-global and reused across calls.

use crate::agent::cmd_guard;
use crate::agent::tools::Tool;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout};
use tokio::sync::Mutex;

/// Protocol version we advertise on `initialize` (the latest we target). Servers echo the version
/// they'll use; we accept whatever they return rather than hard-failing on a mismatch.
const PROTOCOL_VERSION: &str = "2025-06-18";
/// Handshake + list must not hang the CLI startup.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// A single `tools/call` ceiling (mirrors `shell_run`'s wall-clock cap).
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

// ───────────────────────────── config ─────────────────────────────

/// `~/.nextgen/mcp.json`. The `mcpServers` map mirrors the de-facto format used by Claude Desktop
/// & friends, so an existing config drops in. JSON (not TOML) → reuses `serde_json`, no new dep.
#[derive(Debug, Deserialize, Default)]
pub struct McpConfig {
    #[serde(default, rename = "mcpServers", alias = "servers")]
    pub servers: BTreeMap<String, ServerConfig>,
}

#[derive(Debug, Deserialize, Clone)]
pub struct ServerConfig {
    /// stdio transport: the program to spawn (e.g. `npx`). Mutually exclusive with `url`.
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// HTTP (Streamable) transport: the single MCP endpoint. Mutually exclusive with `command`.
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Default ON; set false to keep a server in the file but skip connecting.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// If non-empty, ONLY these tool names are exposed. Applied before `exclude`.
    #[serde(default)]
    pub include: Vec<String>,
    /// Tool names to drop (after `include`).
    #[serde(default)]
    pub exclude: Vec<String>,
    /// `"oauth"` → this remote needs OAuth 2.1 sign-in; the token is cached under
    /// `~/.aizen/mcp-tokens/<key>.json` and attached as a `Bearer` header (refreshed transparently).
    #[serde(default)]
    pub auth: Option<String>,
    /// Optional OAuth overrides (pinned client id / scopes) for servers without dynamic registration.
    #[serde(default)]
    pub oauth: Option<crate::agent::mcp_oauth::OAuthConfig>,
}
fn default_true() -> bool {
    true
}

/// HOME MCP config (`~/.nextgen/mcp.json`) — the personal servers.
pub fn config_path() -> PathBuf {
    crate::core::config::nextgen_home().join("mcp.json")
}

/// PROJECT-local MCP config (`<repo-root>/.nextgen/mcp.json`) — servers a cloned repo ships. Loaded
/// (merged OVER HOME, project wins by key) ONLY when the repo is TRUSTED — auto-loading a cloned
/// repo's tool servers is a supply-chain exec surface, so it's gated behind a first-run trust prompt.
pub fn project_config_path() -> PathBuf {
    crate::core::config::project_nextgen_dir().join("mcp.json")
}

/// Read + parse one mcp.json. `Ok(None)` when absent/empty (the common case — MCP is opt-in).
fn read_one(path: &Path) -> Result<Option<McpConfig>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    let cfg: McpConfig = serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(cfg))
}

/// Load the effective MCP config: HOME servers, with the project's (`./.nextgen/mcp.json`) merged
/// over them when the repo is trusted (project server-defs win by key). Untrusted/absent project →
/// HOME only (non-blocking — the trust prompt lives in the interactive entry, never here).
pub fn load_config() -> Result<Option<McpConfig>> {
    let home = read_one(&config_path())?;
    let proj = if project_trusted() { read_one(&project_config_path())? } else { None };
    Ok(match (home, proj) {
        (None, None) => None,
        (Some(h), None) => Some(h),
        (None, Some(p)) => Some(p),
        (Some(mut h), Some(p)) => {
            for (k, v) in p.servers {
                h.servers.insert(k, v); // project wins on a same-name server
            }
            Some(h)
        }
    })
}

// ── project MCP trust (supply-chain gate) ─────────────────────────────────────────

/// Persisted decision about which project roots may auto-load their `./.nextgen/mcp.json`.
#[derive(Debug, Default, Serialize, Deserialize)]
struct TrustStore {
    #[serde(default)]
    trusted: Vec<String>,
    /// Roots the user declined — so we don't re-prompt every launch (they can `ng mcp trust` later).
    #[serde(default)]
    dismissed: Vec<String>,
}

fn trust_path() -> PathBuf {
    crate::core::config::nextgen_home().join("mcp_trust.json")
}
fn load_trust() -> TrustStore {
    std::fs::read_to_string(trust_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}
fn save_trust(t: &TrustStore) -> Result<()> {
    let p = trust_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    std::fs::write(&p, serde_json::to_string_pretty(t)? + "\n").with_context(|| format!("writing {}", p.display()))
}

/// Canonical string key for the current project root (best-effort canonicalization).
fn project_key() -> String {
    let root = crate::core::config::project_root();
    std::fs::canonicalize(&root).unwrap_or(root).to_string_lossy().to_string()
}

/// Whether the current repo is trusted to load its project-local MCP servers.
pub fn project_trusted() -> bool {
    let key = project_key();
    load_trust().trusted.iter().any(|t| *t == key)
}

/// Trust the current repo's project MCP servers (idempotent; clears any prior dismissal).
pub fn trust_project() -> Result<()> {
    let key = project_key();
    let mut t = load_trust();
    t.dismissed.retain(|d| *d != key);
    if !t.trusted.iter().any(|x| *x == key) {
        t.trusted.push(key);
    }
    save_trust(&t)
}

/// Stop trusting the current repo (and forget any dismissal so it can be re-decided).
pub fn untrust_project() -> Result<()> {
    let key = project_key();
    let mut t = load_trust();
    t.trusted.retain(|x| *x != key);
    t.dismissed.retain(|d| *d != key);
    save_trust(&t)
}

/// Record that the user declined to trust this repo (so we don't nag again this/next launch).
pub fn dismiss_project() -> Result<()> {
    let key = project_key();
    let mut t = load_trust();
    if !t.dismissed.iter().any(|x| *x == key) {
        t.dismissed.push(key);
    }
    save_trust(&t)
}

/// For the interactive entry: how many project MCP servers await a trust decision. `Some(n)` only
/// when a non-empty `./.nextgen/mcp.json` exists, the repo is NOT yet trusted, and NOT dismissed.
pub fn project_trust_prompt() -> Option<usize> {
    let cfg = read_one(&project_config_path()).ok().flatten()?;
    if cfg.servers.is_empty() {
        return None;
    }
    let key = project_key();
    let t = load_trust();
    if t.trusted.iter().any(|x| *x == key) || t.dismissed.iter().any(|x| *x == key) {
        return None;
    }
    Some(cfg.servers.len())
}

impl ServerConfig {
    /// Whether a discovered tool name survives this server's include/exclude filter.
    fn allows(&self, tool: &str) -> bool {
        if !self.include.is_empty() && !self.include.iter().any(|t| t == tool) {
            return false;
        }
        !self.exclude.iter().any(|t| t == tool)
    }
}

// ───────────────────────────── JSON-RPC (pure helpers) ─────────────────────────────

/// Build a JSON-RPC 2.0 request object.
fn rpc_request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}
/// Build a JSON-RPC 2.0 notification (no `id` → no response expected).
fn rpc_notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// Extract the `result` from a parsed JSON-RPC response, surfacing a server `error` as `Err`.
fn rpc_result(msg: Value) -> Result<Value> {
    if let Some(err) = msg.get("error") {
        let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
        let m = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown error");
        bail!("MCP error {code}: {m}");
    }
    msg.get("result").cloned().context("JSON-RPC response missing `result`")
}

/// Is this parsed message the response to request `id`? (Notifications/logs have no matching id.)
fn is_response_to(msg: &Value, id: u64) -> bool {
    msg.get("id").and_then(|v| v.as_u64()) == Some(id)
}

/// A human name for a JSON value's kind (for argument-type error messages).
fn short_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// Flatten an MCP `tools/call` result's `content[]` into plain text for the model. Text blocks are
/// concatenated; non-text blocks (image/audio/resource) become a short typed placeholder.
fn render_content(result: &Value) -> String {
    let Some(items) = result.get("content").and_then(|c| c.as_array()) else {
        // No `content[]`: prefer the typed `structuredContent` (what the model actually wants) over a
        // raw dump of the whole envelope; only fall back to the full result when neither is present.
        if let Some(sc) = result.get("structuredContent") {
            return serde_json::to_string_pretty(sc).unwrap_or_default();
        }
        return serde_json::to_string(result).unwrap_or_default();
    };
    let mut out = String::new();
    for item in items {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(t);
                }
            }
            Some(other) => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(&format!("[{other} content omitted]"));
            }
            None => {}
        }
    }
    out
}

// ───────────────────────────── transport ─────────────────────────────

enum Transport {
    Stdio(StdioTransport),
    Http(HttpTransport),
}

struct StdioTransport {
    // Kept so the child is killed on drop (tokio Command sets `kill_on_drop`).
    _child: tokio::process::Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    /// Tail of the child's stderr (bounded), drained by a background task — so a failed handshake or
    /// a dead child can report WHY (bad token / missing dep / crash) instead of a bare errno/EOF.
    stderr: Arc<std::sync::Mutex<String>>,
}

impl StdioTransport {
    fn stderr_tail(&self) -> String {
        self.stderr.lock().ok().map(|b| b.trim().to_string()).unwrap_or_default()
    }
    /// Send a request line, then read newline-delimited messages until the one answering `id`,
    /// skipping interleaved notifications / log lines.
    async fn request(&mut self, msg: &Value) -> Result<Value> {
        let id = msg.get("id").and_then(|v| v.as_u64()).context("stdio request needs an id")?;
        let line = serde_json::to_string(msg)? + "\n";
        self.stdin.write_all(line.as_bytes()).await.context("writing to MCP server stdin")?;
        self.stdin.flush().await.ok();
        loop {
            let mut buf = String::new();
            let n = self.stdout.read_line(&mut buf).await.context("reading MCP server stdout")?;
            if n == 0 {
                let tail = self.stderr_tail();
                if tail.is_empty() {
                    bail!("MCP server closed stdout before answering");
                }
                bail!("MCP server exited before answering — stderr:\n{tail}");
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(trimmed) else { continue };
            if is_response_to(&v, id) {
                return Ok(v);
            }
            // else: a notification or a response to another id — ignore and keep reading.
        }
    }
    async fn notify(&mut self, msg: &Value) -> Result<()> {
        let line = serde_json::to_string(msg)? + "\n";
        self.stdin.write_all(line.as_bytes()).await.context("writing notification to MCP server")?;
        self.stdin.flush().await.ok();
        Ok(())
    }
}

struct HttpTransport {
    client: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    /// Returned by the server on `initialize`; echoed on every later request.
    session_id: Option<String>,
    /// Negotiated on `initialize`; sent as `MCP-Protocol-Version` afterwards (spec recommendation).
    protocol_version: Option<String>,
    /// The mcp.json key, set ONLY for OAuth-enabled remotes (so we can find/refresh the cached token).
    oauth_key: Option<String>,
    /// The cached OAuth token (when `oauth_key` is set + a sign-in has happened). Attached as `Bearer`
    /// on every request and refreshed transparently on expiry / 401.
    token: Option<crate::agent::mcp_oauth::TokenSet>,
}

impl HttpTransport {
    fn apply_headers(&self, mut rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb = rb
            .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        for (k, v) in &self.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        // OAuth bearer (only set for `auth: oauth` remotes that have signed in). These servers carry
        // no static `Authorization` header, so there's no conflict with the loop above.
        if let Some(t) = &self.token {
            rb = rb.header(reqwest::header::AUTHORIZATION, format!("Bearer {}", t.access_token));
        }
        if let Some(sid) = &self.session_id {
            rb = rb.header("Mcp-Session-Id", sid.as_str());
        }
        if let Some(pv) = &self.protocol_version {
            rb = rb.header("MCP-Protocol-Version", pv.as_str());
        }
        rb
    }

    /// Proactively refresh a near-expired OAuth token so we don't waste a guaranteed-401 round-trip.
    async fn maybe_refresh_expired(&mut self) {
        let pair = match (self.oauth_key.clone(), self.token.clone()) {
            (Some(k), Some(t)) => Some((k, t)),
            _ => None,
        };
        if let Some((key, t)) = pair {
            if t.is_expired() && t.refresh_token.is_some() {
                if let Ok(nt) = crate::agent::mcp_oauth::refresh(&key, &t).await {
                    self.token = Some(nt);
                }
            }
        }
    }

    /// Force a refresh using the cached refresh token (called after a 401).
    async fn refresh_now(&mut self) -> Result<()> {
        let (key, t) = match (self.oauth_key.clone(), self.token.clone()) {
            (Some(k), Some(t)) => (k, t),
            _ => bail!("no OAuth token to refresh"),
        };
        let nt = crate::agent::mcp_oauth::refresh(&key, &t).await?;
        self.token = Some(nt);
        Ok(())
    }

    async fn request(&mut self, msg: &Value) -> Result<Value> {
        let id = msg.get("id").and_then(|v| v.as_u64()).context("http request needs an id")?;
        if self.oauth_key.is_some() {
            self.maybe_refresh_expired().await;
        }
        match self.send_and_read(msg, id).await {
            // 401 on an OAuth remote → refresh the token and replay once; if that still 401s (or we
            // have no refresh token), surface a typed `NeedsAuth` so the user is told to sign in.
            Err(e) if e.downcast_ref::<Unauthorized>().is_some() && self.oauth_key.is_some() => {
                if self.refresh_now().await.is_ok() {
                    self.send_and_read(msg, id).await.map_err(|e2| {
                        if e2.downcast_ref::<Unauthorized>().is_some() {
                            anyhow::Error::new(NeedsAuth { key: self.oauth_key.clone().unwrap_or_default() })
                        } else {
                            e2
                        }
                    })
                } else {
                    Err(anyhow::Error::new(NeedsAuth { key: self.oauth_key.clone().unwrap_or_default() }))
                }
            }
            other => other,
        }
    }

    /// One send + read. Returns typed markers for the two recoverable failures (`SessionExpired` on a
    /// 404 with a session id, `Unauthorized` on a 401) so the wrappers can retry; everything else is a
    /// plain error.
    async fn send_and_read(&mut self, msg: &Value, id: u64) -> Result<Value> {
        let rb = self.apply_headers(self.client.post(&self.url)).json(msg);
        let mut resp = rb.send().await.with_context(|| format!("POST {}", self.url))?;
        // Capture/refresh the session id from the initialize response.
        if let Some(sid) = resp.headers().get("Mcp-Session-Id").and_then(|h| h.to_str().ok()) {
            self.session_id = Some(sid.to_string());
        }
        let status = resp.status();
        let ctype =
            resp.headers().get(reqwest::header::CONTENT_TYPE).and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
        if !status.is_success() {
            // 401 → the bearer is missing/expired; let `request` try a refresh + replay.
            if status.as_u16() == 401 {
                return Err(anyhow::Error::new(Unauthorized));
            }
            // Spec: a 404 to a request carrying an Mcp-Session-Id means the session expired → the
            // client MUST start a new session. Signal that distinctly so `Connection::call` can
            // re-`initialize` and replay once, rather than the session going permanently dead.
            if status.as_u16() == 404 && self.session_id.is_some() {
                self.session_id = None;
                self.protocol_version = None;
                return Err(anyhow::Error::new(SessionExpired));
            }
            let body = resp.text().await.unwrap_or_default();
            bail!("MCP HTTP {} from {}: {}", status.as_u16(), self.url, body.chars().take(300).collect::<String>());
        }
        if ctype.contains("text/event-stream") {
            // Stream the SSE response and return as soon as the frame answering `id` arrives — a
            // buffered `.text()` would block until the server CLOSES the stream (compliant servers
            // may hold it open for progress/keep-alive), hanging the whole call until the timeout.
            return read_sse_response(&mut resp, id).await;
        }
        let body = resp.text().await.context("reading MCP HTTP response body")?;
        serde_json::from_str::<Value>(&body).context("parsing MCP JSON response")
    }

    async fn notify(&mut self, msg: &Value) -> Result<()> {
        // Notifications expect 202 Accepted with no body. We DO check the status: a 404 means the
        // session expired (surface it so the next request re-inits); other 4xx/5xx shouldn't pass
        // silently.
        let rb = self.apply_headers(self.client.post(&self.url)).json(msg);
        let resp = rb.send().await.with_context(|| format!("POST notification {}", self.url))?;
        let status = resp.status();
        if status.as_u16() == 404 && self.session_id.is_some() {
            self.session_id = None;
            self.protocol_version = None;
            return Err(anyhow!(SessionExpired));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            bail!("MCP notify HTTP {} from {}: {}", status.as_u16(), self.url, body.chars().take(200).collect::<String>());
        }
        Ok(())
    }
}

/// Distinct error marker: an HTTP session expired (404 with a session id). `Connection::call`
/// downcasts this to re-run the `initialize` handshake and replay the request once.
#[derive(Debug)]
struct SessionExpired;
impl std::fmt::Display for SessionExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "MCP HTTP session expired (404)")
    }
}
impl std::error::Error for SessionExpired {}

/// Marker for a 401 — `HttpTransport::request` catches it to refresh the OAuth token and replay.
#[derive(Debug)]
struct Unauthorized;
impl std::fmt::Display for Unauthorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unauthorized (401)")
    }
}
impl std::error::Error for Unauthorized {}

/// Surfaced when an OAuth remote has no usable token (none cached, or refresh failed) — its `Display`
/// tells the user exactly how to fix it. Shown verbatim by `build_manager` / `apps info`.
#[derive(Debug)]
struct NeedsAuth {
    key: String,
}
impl std::fmt::Display for NeedsAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "needs sign-in — run `aizen apps login {}`", self.key)
    }
}
impl std::error::Error for NeedsAuth {}

/// Pure incremental SSE parser: push raw chunks, pull complete JSON `data:` events. Accumulates
/// `data:` lines per event (blank line = event boundary, per the SSE spec) so multi-line data and
/// chunk boundaries mid-line are handled. Kept pure (no I/O) so it is unit-testable.
#[derive(Default)]
struct SseParser {
    pending: String, // raw bytes, split into lines as they arrive
    data: String,    // accumulated `data:` payload of the current event
}
impl SseParser {
    /// Feed a chunk; return every complete event payload that parsed to JSON.
    fn push(&mut self, chunk: &str) -> Vec<Value> {
        self.pending.push_str(chunk);
        let mut out = Vec::new();
        while let Some(nl) = self.pending.find('\n') {
            let mut line: String = self.pending.drain(..=nl).collect();
            while line.ends_with('\n') || line.ends_with('\r') {
                line.pop();
            }
            if line.is_empty() {
                if let Some(v) = self.flush() {
                    out.push(v);
                }
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
            // other SSE fields (event:/id:/retry:/`:` comments) are ignored
        }
        out
    }
    /// Parse + clear the current event's accumulated data (the JSON-RPC message), if any.
    fn flush(&mut self) -> Option<Value> {
        if self.data.is_empty() {
            return None;
        }
        let v = serde_json::from_str::<Value>(self.data.trim()).ok();
        self.data.clear();
        v
    }
}

/// Incrementally parse a `text/event-stream` body, returning the first JSON-RPC message that answers
/// `id` — returns as soon as that frame arrives, so a stream the server holds open (progress/keep
/// -alive) doesn't block the call to completion.
async fn read_sse_response(resp: &mut reqwest::Response, id: u64) -> Result<Value> {
    let mut parser = SseParser::default();
    while let Some(bytes) = resp.chunk().await.context("reading MCP SSE chunk")? {
        for v in parser.push(&String::from_utf8_lossy(&bytes)) {
            if is_response_to(&v, id) {
                return Ok(v);
            }
        }
    }
    if let Some(v) = parser.flush() {
        if is_response_to(&v, id) {
            return Ok(v);
        }
    }
    bail!("MCP SSE stream ended without a response to id {id}")
}

impl Transport {
    async fn request(&mut self, msg: &Value) -> Result<Value> {
        match self {
            Transport::Stdio(t) => t.request(msg).await,
            Transport::Http(t) => t.request(msg).await,
        }
    }
    async fn notify(&mut self, msg: &Value) -> Result<()> {
        match self {
            Transport::Stdio(t) => t.notify(msg).await,
            Transport::Http(t) => t.notify(msg).await,
        }
    }
    /// The captured stderr tail (stdio only) — appended to connect errors so failures are diagnosable.
    fn stderr_tail(&self) -> String {
        match self {
            Transport::Stdio(t) => t.stderr_tail(),
            Transport::Http(_) => String::new(),
        }
    }
}

// ───────────────────────────── connection ─────────────────────────────

struct Connection {
    transport: Transport,
    next_id: u64,
}

impl Connection {
    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let first = self.raw_call(method, params.clone()).await;
        match first {
            // HTTP session expired (404) → re-run the handshake and replay the request exactly once.
            // `initialize` itself is never retried (it can't expire a session it hasn't made yet).
            Err(e) if method != "initialize" && e.downcast_ref::<SessionExpired>().is_some() => {
                self.initialize().await.context("re-initializing after MCP HTTP session expiry")?;
                self.raw_call(method, params).await
            }
            other => other,
        }
    }

    async fn raw_call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id();
        let msg = rpc_request(id, method, params);
        let resp = self.transport.request(&msg).await?;
        rpc_result(resp)
    }

    /// `initialize` handshake + the `notifications/initialized` follow-up. Returns serverInfo.
    async fn initialize(&mut self) -> Result<Value> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": "aizen", "version": env!("CARGO_PKG_VERSION")},
        });
        // `raw_call` (not `call`) so the session-expiry retry in `call` can't recurse back into us.
        let result = self.raw_call("initialize", params).await?;
        // Pin the negotiated protocol version on the HTTP transport for subsequent requests.
        if let Transport::Http(h) = &mut self.transport {
            if let Some(pv) = result.get("protocolVersion").and_then(|v| v.as_str()) {
                h.protocol_version = Some(pv.to_string());
            }
        }
        self.transport.notify(&rpc_notification("notifications/initialized", json!({}))).await?;
        Ok(result.get("serverInfo").cloned().unwrap_or(Value::Null))
    }

    /// `tools/list`, following `nextCursor` pagination to completion. Guarded against a server that
    /// returns the SAME cursor forever (infinite loop) and capped at a sane page count.
    async fn list_tools(&mut self) -> Result<Vec<ToolMeta>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        let mut prev: Option<String> = None;
        for _page in 0..50 {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let result = self.call("tools/list", params).await?;
            if let Some(arr) = result.get("tools").and_then(|t| t.as_array()) {
                for t in arr {
                    if let Some(meta) = ToolMeta::from_value(t) {
                        out.push(meta);
                    }
                }
            }
            match result.get("nextCursor").and_then(|c| c.as_str()) {
                // advance only on a NEW non-empty cursor (a repeated cursor = buggy server → stop)
                Some(c) if !c.is_empty() && Some(c) != prev.as_deref() => {
                    prev = Some(c.to_string());
                    cursor = Some(c.to_string());
                }
                _ => break,
            }
        }
        Ok(out)
    }

    async fn call_tool(&mut self, name: &str, args: &Value) -> Result<String> {
        let params = json!({"name": name, "arguments": args});
        let result = self.call("tools/call", params).await?;
        // A tool that errored at the application level sets `isError: true` (distinct from a
        // JSON-RPC protocol error). Feed that back to the model as an `Err` so it can recover.
        if result.get("isError").and_then(|b| b.as_bool()).unwrap_or(false) {
            let msg = render_content(&result);
            // An isError whose content is empty / image-only would otherwise hand the model a blank
            // error — fall back to the full result. Detect "no usable text" STRUCTURALLY (is there a
            // non-empty text block?), NOT by sniffing a leading '[' — many real errors legitimately
            // start with one (e.g. "[Errno 2] No such file"), and discarding those degraded the error.
            let has_text = result
                .get("content")
                .and_then(|c| c.as_array())
                .map(|items| {
                    items.iter().any(|i| {
                        i.get("type").and_then(|t| t.as_str()) == Some("text")
                            && i.get("text").and_then(|t| t.as_str()).is_some_and(|s| !s.trim().is_empty())
                    })
                })
                .unwrap_or(false);
            let msg = if has_text { msg } else { serde_json::to_string(&result).unwrap_or(msg) };
            bail!("{msg}");
        }
        Ok(render_content(&result))
    }
}

/// One tool as advertised by a server.
#[derive(Clone)]
struct ToolMeta {
    name: String,
    description: String,
    input_schema: Value,
    read_only: bool,
}

impl ToolMeta {
    fn from_value(v: &Value) -> Option<Self> {
        let name = v.get("name").and_then(|n| n.as_str())?.to_string();
        let description = v
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("(no description provided by the MCP server)")
            .to_string();
        // Pass the server's JSON Schema straight through; default to an open object if absent.
        let input_schema = v
            .get("inputSchema")
            .cloned()
            .unwrap_or_else(|| json!({"type": "object", "additionalProperties": true}));
        let read_only = v
            .get("annotations")
            .and_then(|a| a.get("readOnlyHint"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        Some(Self { name, description, input_schema, read_only })
    }
}

// ───────────────────────────── manager (process-global) ─────────────────────────────

/// A connected server: a shared connection + the tools we'll expose from it.
struct ServerHandle {
    name: String,
    conn: Arc<Mutex<Connection>>,
    tools: Vec<ToolMeta>,
    server_info: Value,
}

/// All connected MCP servers, established once per process.
pub struct Manager {
    servers: Vec<ServerHandle>,
}

static MANAGER: std::sync::RwLock<Option<Manager>> = std::sync::RwLock::new(None);

/// Spawn a stdio child server and return its transport. On **Windows** the launch goes through
/// `cmd /C` because the common runners (`npx`, `uvx`, `dnx`, `bunx`) are `.cmd`/`.bat` shims that
/// `CreateProcessW` — what `Command::new` uses — cannot execute directly (this was the #1 reason
/// EVERY local app failed to connect on Windows). Child **stderr** is drained into a bounded buffer
/// so a failed handshake reports the real cause (bad token / missing dep / crash), not a bare EOF.
async fn connect_stdio(cfg: &ServerConfig) -> Result<Transport> {
    let command = cfg.command.as_ref().context("stdio server needs `command`")?;
    let mut cmd = if cfg!(windows) {
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(command).args(&cfg.args);
        c
    } else {
        let mut c = tokio::process::Command::new(command);
        c.args(&cfg.args);
        c
    };
    cmd.envs(&cfg.env)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let mut child = cmd.spawn().with_context(|| format!("spawning MCP server `{command}`"))?;
    let stdin = child.stdin.take().context("child stdin unavailable")?;
    let stdout = child.stdout.take().context("child stdout unavailable")?;
    let stderr_buf: Arc<std::sync::Mutex<String>> = Arc::new(std::sync::Mutex::new(String::new()));
    if let Some(err) = child.stderr.take() {
        let sink = Arc::clone(&stderr_buf);
        tokio::spawn(async move {
            let mut reader = BufReader::new(err);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if let Ok(mut b) = sink.lock() {
                            b.push_str(&line);
                            if b.len() > 4096 {
                                let cut = b.len() - 4096;
                                *b = b[cut..].to_string();
                            }
                        }
                    }
                }
            }
        });
    }
    Ok(Transport::Stdio(StdioTransport { _child: child, stdin, stdout: BufReader::new(stdout), stderr: stderr_buf }))
}

fn connect_http(name: &str, cfg: &ServerConfig) -> Result<Transport> {
    let url = cfg.url.as_ref().context("http server needs `url`")?.clone();
    let client = reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION")))
        .timeout(CALL_TIMEOUT)
        .build()
        .context("building MCP HTTP client")?;
    // For an OAuth remote, load the cached token (if signed in) so requests carry a Bearer. With no
    // token the first request 401s → `NeedsAuth` → the connect fails with a "run `ng apps login`" hint.
    let oauth_enabled = cfg.auth.as_deref() == Some("oauth");
    let (oauth_key, token) = if oauth_enabled {
        (Some(name.to_string()), crate::agent::mcp_oauth::load_token(name))
    } else {
        (None, None)
    };
    Ok(Transport::Http(HttpTransport {
        client,
        url,
        headers: cfg.headers.clone(),
        session_id: None,
        protocol_version: None,
        oauth_key,
        token,
    }))
}

/// Connect one server: build transport → handshake → list tools (all under a timeout).
async fn connect_one(name: &str, cfg: &ServerConfig) -> Result<ServerHandle> {
    let transport = if cfg.command.is_some() {
        // Hard floor: the same `cmd_guard` that protects `shell_run` gates an MCP child too, so a
        // hostile registry entry or a (trusted) project mcp.json can't smuggle a catastrophic
        // command (`rm -rf /`, `mkfs`, …) past the spawn. Unconditional — like the shell floor.
        let cmdline = std::iter::once(cfg.command.clone().unwrap_or_default())
            .chain(cfg.args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");
        if let cmd_guard::Verdict::Blocked(reason) = cmd_guard::classify(&cmdline) {
            bail!("refusing to start MCP server `{name}`: command blocked ({reason})");
        }
        connect_stdio(cfg).await?
    } else if cfg.url.is_some() {
        connect_http(name, cfg)?
    } else {
        bail!("server `{name}` has neither `command` (stdio) nor `url` (http)");
    };
    let mut conn = Connection { transport, next_id: 0 };
    let server_info = match tokio::time::timeout(CONNECT_TIMEOUT, conn.initialize()).await {
        Ok(Ok(si)) => si,
        Ok(Err(e)) => {
            let tail = conn.transport.stderr_tail();
            return Err(if tail.is_empty() { e } else { e.context(format!("server stderr:\n{tail}")) });
        }
        Err(_) => {
            let tail = conn.transport.stderr_tail();
            let suffix = if tail.is_empty() { String::new() } else { format!(" — stderr:\n{tail}") };
            bail!("`{name}` initialize timed out after {}s{suffix}", CONNECT_TIMEOUT.as_secs());
        }
    };
    let tools = match tokio::time::timeout(CONNECT_TIMEOUT, conn.list_tools()).await {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => return Err(e),
        Err(_) => bail!("`{name}` tools/list timed out after {}s", CONNECT_TIMEOUT.as_secs()),
    };
    let tools: Vec<ToolMeta> = tools.into_iter().filter(|t| cfg.allows(&t.name)).collect();
    Ok(ServerHandle { name: name.to_string(), conn: Arc::new(Mutex::new(conn)), tools, server_info })
}

/// A live capability probe of ONE configured server (for `ng apps info` / the apps detail view):
/// build transport → handshake → `tools/list`, then drop the connection (the stdio child is
/// `kill_on_drop`). Distinct from the process-global `Manager` (which connects every server once at
/// startup) — this targets a single key on demand and tears down immediately.
pub struct ProbeReport {
    /// The full `initialize` result (holds `serverInfo` + `capabilities` + `protocolVersion`).
    pub server_info: Value,
    pub tools: Vec<ProbeTool>,
}
pub struct ProbeTool {
    pub name: String,
    pub description: String,
    pub read_only: bool,
}

/// Probe a single server by its mcp.json key. Errors if the key is absent or the connect fails
/// (missing runtime, bad token, timeout) — the caller surfaces that verbatim as the "status".
pub async fn probe(name: &str) -> Result<ProbeReport> {
    let cfg = load_config()?.context("no mcp.json — nothing connected")?;
    let sc = cfg.servers.get(name).with_context(|| format!("no connected app keyed '{name}' in mcp.json"))?;
    let handle = connect_one(name, sc).await?;
    Ok(ProbeReport {
        server_info: handle.server_info,
        tools: handle
            .tools
            .into_iter()
            .map(|t| ProbeTool { name: t.name, description: t.description, read_only: t.read_only })
            .collect(),
    })
}

/// Hit a remote MCP endpoint UNAUTHENTICATED once to capture its `401 WWW-Authenticate` challenge —
/// that header points at the protected-resource metadata the OAuth discovery needs (RFC 9728). `None`
/// when the server doesn't 401 (no auth required) or sends no challenge (discovery falls back to the
/// well-known location at the server origin).
async fn probe_www_authenticate(url: &str) -> Option<String> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION")))
        .timeout(CONNECT_TIMEOUT)
        .build()
        .ok()?;
    let init = rpc_request(
        1,
        "initialize",
        json!({"protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "aizen", "version": env!("CARGO_PKG_VERSION")}}),
    );
    let resp = client
        .post(url)
        .header(reqwest::header::ACCEPT, "application/json, text/event-stream")
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&init)
        .send()
        .await
        .ok()?;
    if resp.status().as_u16() == 401 {
        return resp.headers().get("WWW-Authenticate").and_then(|h| h.to_str().ok()).map(|s| s.to_string());
    }
    None
}

/// Interactive OAuth sign-in for one configured remote server key: probe its `401` challenge → run
/// the full PKCE flow (browser + loopback) → cache the token → invalidate the manager so the next
/// message reconnects WITH the bearer. Used by `ng apps login` / `ng mcp login`.
pub async fn login(key: &str) -> Result<()> {
    let cfg = load_config()?.context("no mcp.json — add an app first (`aizen apps add <name>`)")?;
    let sc = cfg.servers.get(key).with_context(|| format!("no app keyed '{key}' in mcp.json"))?;
    let url = sc.url.clone().context("`login` applies only to remote (url) servers")?;
    if sc.auth.as_deref() != Some("oauth") {
        bail!("server '{key}' isn't configured for OAuth (no \"auth\": \"oauth\" in its mcp.json entry)");
    }
    let oauth_cfg = sc.oauth.clone().unwrap_or_default();
    let www = probe_www_authenticate(&url).await;
    crate::agent::mcp_oauth::authorize(key, &url, &oauth_cfg, www).await?;
    invalidate();
    Ok(())
}

/// Connect every enabled server in the config — **concurrently** (each `connect_one` is independent
/// and individually timed), so first-turn latency is max(server), not the sum. Previously this looped
/// serially on the hot path, so N servers (or one slow one) froze the first agent turn for up to
/// N×30s with no output. A server that fails to connect is logged and skipped — one broken entry
/// never takes down the CLI or the others.
async fn build_manager(cfg: &McpConfig) -> Manager {
    let enabled: Vec<(String, ServerConfig)> =
        cfg.servers.iter().filter(|(_, sc)| sc.enabled).map(|(n, sc)| (n.clone(), sc.clone())).collect();
    if enabled.is_empty() {
        return Manager { servers: Vec::new() };
    }
    eprintln!("{}", console::style(format!("mcp: connecting {} server(s)…", enabled.len())).dim());

    // An overall ceiling per server so even a hang at spawn / first-run package download (which is
    // OUTSIDE the inner handshake timeouts) can't wedge the connect. JoinSet → run all at once.
    let overall = CONNECT_TIMEOUT * 3;
    let mut set: tokio::task::JoinSet<(String, Result<ServerHandle>)> = tokio::task::JoinSet::new();
    for (name, sc) in enabled {
        set.spawn(async move {
            let r = match tokio::time::timeout(overall, connect_one(&name, &sc)).await {
                Ok(r) => r,
                Err(_) => Err(anyhow!("`{name}` connect timed out after {}s", overall.as_secs())),
            };
            (name, r)
        });
    }

    let mut servers = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((_, Ok(h))) => servers.push(h),
            Ok((name, Err(e))) => {
                eprintln!("{}", console::style(format!("mcp: server '{name}' skipped — {e:#}")).yellow())
            }
            Err(je) => eprintln!("{}", console::style(format!("mcp: a connect task failed — {je}")).yellow()),
        }
    }
    // JoinSet completes out of order; sort for a deterministic tool listing across runs.
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    Manager { servers }
}

/// Drive an async future from the sync `Tool::execute` path (see the module note on the invariant).
fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

/// Build the global manager if it isn't built yet (connecting all enabled servers once). A built-but
/// -empty `Manager` is cached for the "no config / nothing enabled" case so we don't reconnect every
/// turn; `invalidate()` clears it so a freshly-added app is picked up on the next registry build (no
/// process relaunch). Leaves it `None` (unbuilt) when there's no Tokio runtime (e.g. unit tests).
fn ensure_manager() {
    let mut guard = MANAGER.write().unwrap();
    if guard.is_some() {
        return;
    }
    let cfg = match load_config() {
        Ok(Some(c)) if c.servers.values().any(|s| s.enabled) => c,
        Ok(_) => {
            *guard = Some(Manager { servers: Vec::new() }); // no file / nothing enabled → cache empty
            return;
        }
        Err(e) => {
            eprintln!("{}", console::style(format!("mcp: {e}")).yellow());
            *guard = Some(Manager { servers: Vec::new() });
            return;
        }
    };
    // The async connect needs a runtime; the real registry is always built inside `#[tokio::main]`.
    if tokio::runtime::Handle::try_current().is_err() {
        return; // leave None (unbuilt) — unit tests have no runtime
    }
    // Hold the write lock across the connect: callers only ever take the lock to read, and the
    // connect tasks (JoinSet) never touch MANAGER, so there's no deadlock — and no double-build race.
    let m = block(build_manager(&cfg));
    *guard = Some(m);
}

/// Forget the connected servers so the NEXT registry build reconnects from the current mcp.json —
/// the hot-reload hook after `ng apps add/remove` (no process relaunch needed). Dropping the old
/// `Manager` kills its child processes; the next `discovered_tools()` reconnects everything.
pub fn invalidate() {
    *MANAGER.write().unwrap() = None;
}

/// FNV-1a 64-bit hash — a small stable hash used for de-collision suffixes (no crypto needs).
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 1469598103934665603;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(1099511628211);
    }
    h
}

/// Sanitize a server/tool name into the `[a-z0-9_]` charset the tool-name grammar expects.
fn slug(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('_');
        }
    }
    let out = out.trim_matches('_').to_string();
    if out.is_empty() {
        // All-non-ASCII (CJK like "検索") or punctuation-only ("--") names slug to "" and would all
        // collide on `mcp__` / `mcp_<server>_`. Fall back to a short stable hash of the ORIGINAL so
        // distinct names stay distinguishable. (Execution still routes via the unmodified remote name.)
        return format!("x{:x}", fnv1a(s) & 0xffffff);
    }
    out
}

/// Tool names must satisfy the provider grammar `^[a-zA-Z0-9_-]{1,64}$`. A long server+tool pair can
/// blow the 64-char cap — and an over-long or duplicate name in the `tools[]` array makes some
/// providers 400 the WHOLE request (every tool, not just the offender). So we cap the length.
const MAX_TOOL_NAME: usize = 64;

/// The fully-qualified tool name the model sees: `mcp_<server>_<tool>`, capped to 64 chars (the
/// provider limit) by truncating with a short stable hash suffix so distinct long names stay distinct.
fn qualified_name(server: &str, tool: &str) -> String {
    let full = format!("mcp_{}_{}", slug(server), slug(tool));
    if full.len() <= MAX_TOOL_NAME {
        return full;
    }
    // Deterministic short suffix from the full name, then truncate the head to fit.
    let suffix = format!("_{:x}", fnv1a(&full) & 0xffffff); // 6 hex + '_'
    let head_len = MAX_TOOL_NAME - suffix.len();
    let head: String = full.chars().take(head_len).collect();
    format!("{head}{suffix}")
}

/// All MCP tools, ready to register into a `ToolRegistry`. Empty (no cost) when MCP is off.
/// Called from `default_registry_in` so the top-level agent surface gains them automatically.
/// Qualified names are de-duplicated (across servers exposing the same tool name) so one collision
/// can't silently shadow a tool or make the provider reject the whole tool array.
pub fn discovered_tools() -> Vec<Box<dyn Tool>> {
    ensure_manager();
    let guard = MANAGER.read().unwrap();
    let Some(mgr) = guard.as_ref() else { return Vec::new() };
    let mut out: Vec<Box<dyn Tool>> = Vec::new();
    let mut used: std::collections::HashSet<String> = std::collections::HashSet::new();
    for srv in &mgr.servers {
        for t in &srv.tools {
            let mut qn = qualified_name(&srv.name, &t.name);
            if used.contains(&qn) {
                // Disambiguate a collision (rare): append _2, _3, … keeping within the length cap.
                for n in 2..1000 {
                    let suffix = format!("_{n}");
                    let head: String = qn.chars().take(MAX_TOOL_NAME - suffix.len()).collect();
                    let cand = format!("{head}{suffix}");
                    if !used.contains(&cand) {
                        eprintln!("{}", console::style(format!("mcp: tool name '{qn}' collides — exposing as '{cand}'")).dim());
                        qn = cand;
                        break;
                    }
                }
            }
            used.insert(qn.clone());
            out.push(Box::new(McpTool {
                qualified: qn,
                server: srv.name.clone(),
                remote_name: t.name.clone(),
                description: format!("[MCP {}] {}", srv.name, t.description),
                schema: t.input_schema.clone(),
                destructive: !t.read_only,
                conn: srv.conn.clone(),
            }));
        }
    }
    out
}

/// A human-readable summary of connected servers + their tools, for `ng mcp` / `/mcp`.
pub fn summary() -> String {
    // Surface an untrusted project mcp.json up front (it's deliberately NOT loaded yet).
    let trust_note = match project_trust_prompt() {
        Some(n) => format!(
            "⚠ this repo ships {n} project MCP server(s) at {} — not loaded.\n  Run `aizen mcp trust` to enable them (they can run commands).\n\n",
            project_config_path().display()
        ),
        None => String::new(),
    };
    match load_config() {
        Ok(None) => {
            return format!(
                "{trust_note}No MCP servers configured.\nCreate {} with an \"mcpServers\" map to add some.",
                config_path().display()
            )
        }
        Ok(Some(c)) if c.servers.is_empty() => return format!("{trust_note}mcp.json has no servers."),
        Err(e) => return format!("mcp config error: {e}"),
        _ => {}
    }
    ensure_manager();
    let guard = MANAGER.read().unwrap();
    let Some(mgr) = guard.as_ref() else {
        return format!("{trust_note}MCP configured but not connected (no servers enabled, or not on a runtime).");
    };
    if mgr.servers.is_empty() {
        return format!("{trust_note}No MCP servers connected (all disabled or failed to connect).");
    }
    let mut s = trust_note;
    for srv in &mgr.servers {
        let info = srv
            .server_info
            .get("name")
            .and_then(|n| n.as_str())
            .map(|n| format!(" ({n})"))
            .unwrap_or_default();
        s.push_str(&format!("● {}{} — {} tool(s)\n", srv.name, info, srv.tools.len()));
        for t in &srv.tools {
            let ro = if t.read_only { " [read-only]" } else { "" };
            s.push_str(&format!("    {}{}\n", qualified_name(&srv.name, &t.name), ro));
        }
    }
    s.trim_end().to_string()
}

// ───────────────────────────── the dyn Tool wrapper ─────────────────────────────

struct McpTool {
    qualified: String,
    server: String,
    remote_name: String,
    description: String,
    schema: Value,
    destructive: bool,
    conn: Arc<Mutex<Connection>>,
}

impl Tool for McpTool {
    fn name(&self) -> &str {
        &self.qualified
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn parameters(&self) -> Value {
        // Guarantee an object schema (some servers omit `type`); the model needs a valid shape.
        match &self.schema {
            Value::Object(m) if m.contains_key("type") => self.schema.clone(),
            Value::Object(m) => {
                let mut m = m.clone();
                m.insert("type".to_string(), json!("object"));
                Value::Object(m)
            }
            _ => json!({"type": "object", "additionalProperties": true}),
        }
    }
    fn is_destructive(&self) -> bool {
        self.destructive
    }
    fn is_concurrency_safe(&self) -> bool {
        false // block_in_place would panic on the parallel scoped-thread path (no runtime)
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let conn = self.conn.clone();
        let name = self.remote_name.clone();
        let server = self.server.clone();
        // Coerce null/absent → {}, but DON'T silently discard a real non-object value (a bare
        // string/array/number means the model called the tool wrong — tell it, don't drop the arg).
        let args = match args {
            Value::Object(_) => args.clone(),
            Value::Null => json!({}),
            _ => bail!("MCP tool '{server}/{name}' expects a JSON object for arguments, got {}", short_kind(args)),
        };
        block(async move {
            let mut c = conn.lock().await;
            match tokio::time::timeout(CALL_TIMEOUT, c.call_tool(&name, &args)).await {
                Ok(r) => r,
                Err(_) => {
                    // The dropped read can leave the stdio BufReader desynced from message boundaries.
                    // invalidate() nulls the cached Manager so the NEXT turn's registry build reconnects
                    // from scratch. NOTE it does NOT repair the CURRENT turn: the live McpTools still
                    // hold Arc<Mutex<Connection>> clones of this (now desynced) connection, so a repeat
                    // call to the same server this turn keeps re-timing-out until the next message
                    // rebuilds the registry. (A same-turn in-place reconnect would need a poison flag on
                    // Connection; deferred — CALL_TIMEOUT is rare and the next turn self-heals.)
                    drop(c);
                    invalidate();
                    Err(anyhow!(
                        "MCP tool '{server}/{name}' timed out after {}s (connection reset)",
                        CALL_TIMEOUT.as_secs()
                    ))
                }
            }
        })
    }
}

// ───────────────────────────── tests (pure, offline) ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_normalizes_to_safe_charset() {
        assert_eq!(slug("File System"), "file_system");
        assert_eq!(slug("git-tools"), "git_tools");
        assert_eq!(slug("Weird@@Name!!"), "weird__name"); // inner non-alnum → _, trimmed ends
        assert_eq!(qualified_name("My Server", "read-file"), "mcp_my_server_read_file");
        // Non-ASCII / punctuation-only names must NOT collapse to "" (the collision bug): each gets a
        // short stable hash so distinct names stay distinguishable.
        assert!(slug("検索").starts_with('x') && slug("検索").len() > 1);
        assert_ne!(slug("検索"), slug("コード"), "distinct non-ASCII names stay distinct");
        assert!(slug("--").starts_with('x'));
    }

    #[test]
    fn config_parses_both_transports_and_filters() {
        let raw = r#"{
          "mcpServers": {
            "fs":   {"command": "npx", "args": ["-y", "server-fs"], "include": ["read_file"]},
            "rem":  {"url": "https://x/mcp", "headers": {"Authorization": "Bearer t"}, "exclude": ["danger"]},
            "off":  {"command": "x", "enabled": false}
          }
        }"#;
        let cfg: McpConfig = serde_json::from_str(raw).unwrap();
        assert_eq!(cfg.servers.len(), 3);
        let fs = &cfg.servers["fs"];
        assert_eq!(fs.command.as_deref(), Some("npx"));
        assert!(fs.enabled, "enabled defaults true");
        assert!(fs.allows("read_file"));
        assert!(!fs.allows("write_file"), "include-list excludes everything else");
        let rem = &cfg.servers["rem"];
        assert_eq!(rem.url.as_deref(), Some("https://x/mcp"));
        assert!(rem.allows("anything"));
        assert!(!rem.allows("danger"), "exclude drops it");
        assert!(!cfg.servers["off"].enabled);
    }

    #[test]
    fn project_mcp_merges_only_when_trusted() {
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("ng-mcp-home-{}", std::process::id()));
        let proj = std::env::temp_dir().join(format!("ng-mcp-proj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&proj);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(proj.join(".nextgen")).unwrap();
        std::env::set_var("NEXTGEN_HOME", &home);
        std::env::set_var("NG_PROJECT_ROOT", &proj);

        std::fs::write(home.join("mcp.json"), r#"{"mcpServers":{"h":{"command":"home"}}}"#).unwrap();
        std::fs::write(
            proj.join(".nextgen").join("mcp.json"),
            r#"{"mcpServers":{"p":{"command":"proj"},"h":{"command":"proj-override"}}}"#,
        )
        .unwrap();

        // Untrusted: HOME only; the project servers await a decision.
        let c = load_config().unwrap().unwrap();
        assert_eq!(c.servers.len(), 1, "untrusted → project servers not loaded");
        assert_eq!(c.servers["h"].command.as_deref(), Some("home"));
        assert_eq!(project_trust_prompt(), Some(2), "untrusted, non-dismissed → prompt for 2 servers");

        // Trust: merged, project wins on the colliding key.
        trust_project().unwrap();
        let c = load_config().unwrap().unwrap();
        assert_eq!(c.servers.len(), 2, "trusted → project merged");
        assert_eq!(c.servers["h"].command.as_deref(), Some("proj-override"), "project wins on collision");
        assert!(c.servers.contains_key("p"));
        assert_eq!(project_trust_prompt(), None, "trusted → no prompt");

        let _ = untrust_project();
        std::env::remove_var("NEXTGEN_HOME");
        std::env::remove_var("NG_PROJECT_ROOT");
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&proj);
    }

    #[test]
    fn rpc_result_surfaces_error() {
        let ok = json!({"jsonrpc":"2.0","id":1,"result":{"a":1}});
        assert_eq!(rpc_result(ok).unwrap(), json!({"a":1}));
        let err = json!({"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"no method"}});
        let e = rpc_result(err).unwrap_err().to_string();
        assert!(e.contains("-32601") && e.contains("no method"), "got: {e}");
    }

    #[test]
    fn is_response_to_matches_id_only() {
        assert!(is_response_to(&json!({"id": 7, "result": {}}), 7));
        assert!(!is_response_to(&json!({"id": 8, "result": {}}), 7));
        assert!(!is_response_to(&json!({"method": "notifications/x"}), 7)); // notification, no id
    }

    #[test]
    fn sse_parser_handles_events_and_split_chunks() {
        // Two events; the second answers id=3. Also fed split across a chunk boundary mid-line to
        // prove the parser reassembles partial lines (the streaming-transport invariant).
        let mut p = SseParser::default();
        let part1 = "event: message\n\
                     data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\
                     \n\
                     event: message\n\
                     data: {\"jsonrpc\":\"2.0\",\"id\"";
        let part2 = ":3,\"result\":{\"ok\":true}}\n\n";
        let mut found = Vec::new();
        found.extend(p.push(part1));
        found.extend(p.push(part2));
        let hit = found.iter().find(|v| is_response_to(v, 3)).expect("should find id=3");
        assert_eq!(hit["result"]["ok"], json!(true));
        assert!(!found.iter().any(|v| is_response_to(v, 99)));
    }

    #[test]
    fn sse_parser_accumulates_multiline_data() {
        // A single event whose `data:` payload spans multiple lines (SSE concatenates with \n).
        let mut p = SseParser::default();
        let body = "data: {\"jsonrpc\":\"2.0\",\n\
                    data: \"id\":7,\"result\":{}}\n\n";
        let evs = p.push(body);
        assert!(evs.iter().any(|v| is_response_to(v, 7)), "multi-line data event must parse");
    }

    #[test]
    fn qualified_name_caps_at_provider_limit() {
        // A normal name passes through.
        assert_eq!(qualified_name("fs", "read_file"), "mcp_fs_read_file");
        // An over-long server+tool is capped to 64 chars (provider grammar) with a hash suffix, and
        // two distinct long names stay distinct.
        let a = qualified_name(&"x".repeat(60), &"read".repeat(20));
        let b = qualified_name(&"x".repeat(60), &"write".repeat(20));
        assert!(a.len() <= 64 && b.len() <= 64, "must fit the 64-char cap");
        assert!(a.starts_with("mcp_"));
        assert_ne!(a, b, "distinct long names must not collapse to the same id");
    }

    #[test]
    fn render_content_prefers_structured_when_no_content() {
        let r = json!({"structuredContent": {"answer": 42}});
        let out = render_content(&r);
        assert!(out.contains("\"answer\""), "structuredContent should be surfaced: {out}");
    }

    #[test]
    fn render_content_concatenates_text_and_marks_other() {
        let r = json!({"content": [
            {"type": "text", "text": "line one"},
            {"type": "text", "text": "line two"},
            {"type": "image", "data": "..."}
        ]});
        let out = render_content(&r);
        assert_eq!(out, "line one\nline two\n[image content omitted]");
    }

    #[test]
    fn tool_meta_reads_readonly_hint_and_schema_passthrough() {
        let t = json!({
            "name": "list_dir",
            "description": "list a directory",
            "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}},
            "annotations": {"readOnlyHint": true}
        });
        let m = ToolMeta::from_value(&t).unwrap();
        assert_eq!(m.name, "list_dir");
        assert!(m.read_only);
        assert_eq!(m.input_schema["properties"]["path"]["type"], json!("string"));

        // No annotations → destructive-by-default (read_only=false).
        let t2 = json!({"name": "rm", "description": "delete"});
        let m2 = ToolMeta::from_value(&t2).unwrap();
        assert!(!m2.read_only);
        assert!(m2.input_schema["type"] == json!("object"), "defaults to object schema");
    }

    #[test]
    fn load_config_absent_is_none() {
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap();
        let tmp = std::env::temp_dir().join(format!("ng-mcp-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("NEXTGEN_HOME", &tmp);
        // No mcp.json in this fresh home.
        let _ = std::fs::remove_file(config_path());
        assert!(load_config().unwrap().is_none());
        std::env::remove_var("NEXTGEN_HOME");
    }
}
