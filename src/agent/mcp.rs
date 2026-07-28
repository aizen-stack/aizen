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
//! Async-from-sync: the `Tool` trait is sync; MCP I/O is async. Calls bridge through the shared
//! cancel-aware `tools::block_for_tool` (valid on runtime workers AND spawn_blocking threads), so
//! read-only `McpTool`s declare `is_concurrency_safe() = true` and may run concurrently in a
//! batch; same-server calls serialize on the connection's own `Arc<Mutex<Connection>>`.
//! Connections are process-global and reused across calls.

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
    let cfg: McpConfig =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(cfg))
}

/// Load the effective MCP config: HOME servers, with the project's (`./.nextgen/mcp.json`) merged
/// over them when the repo is trusted (project server-defs win by key). Untrusted/absent project →
/// HOME only (non-blocking — the trust prompt lives in the interactive entry, never here).
pub fn load_config() -> Result<Option<McpConfig>> {
    let home = read_one(&config_path())?;
    let proj = if project_trusted() {
        read_one(&project_config_path())?
    } else {
        None
    };
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
    std::fs::read_to_string(trust_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}
fn save_trust(t: &TrustStore) -> Result<()> {
    let p = trust_path();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        crate::core::config::harden_dir(parent);
    }
    let mut bytes = serde_json::to_vec_pretty(t)?;
    bytes.push(b'\n');
    crate::core::persist::atomic_write(&p, &bytes)
        .with_context(|| format!("writing {}", p.display()))?;
    crate::core::persist::harden_owner_only_checked(&p)?;
    Ok(())
}

/// Serialize a trust-store read-modify-write under the shared store lock, RELOADING the authoritative
/// bytes inside the lock so two windows toggling trust for different roots can't lose each other's
/// change (the old load→mutate→save had a cross-process lost-update window). The mutator runs against
/// the freshly reloaded store; its result is persisted while the lock is still held.
fn update_trust(mutate: impl FnOnce(&mut TrustStore)) -> Result<()> {
    let lock_path = crate::core::workspace_txn::store_lock("mcp-trust", "trust");
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        std::time::Duration::from_secs(5),
    )?;
    let mut t = load_trust();
    mutate(&mut t);
    save_trust(&t)
}

/// Canonical string key for the current project root (best-effort canonicalization).
fn project_key() -> String {
    let root = crate::core::config::project_root();
    std::fs::canonicalize(&root)
        .unwrap_or(root)
        .to_string_lossy()
        .to_string()
}

/// Whether the current repo is trusted to load its project-local MCP servers.
pub fn project_trusted() -> bool {
    let key = project_key();
    load_trust().trusted.iter().any(|t| *t == key)
}

/// Trust the current repo's project MCP servers (idempotent; clears any prior dismissal).
pub fn trust_project() -> Result<()> {
    let key = project_key();
    update_trust(|t| {
        t.dismissed.retain(|d| *d != key);
        if !t.trusted.iter().any(|x| *x == key) {
            t.trusted.push(key.clone());
        }
    })
}

/// Stop trusting the current repo (and forget any dismissal so it can be re-decided).
pub fn untrust_project() -> Result<()> {
    let key = project_key();
    update_trust(|t| {
        t.trusted.retain(|x| *x != key);
        t.dismissed.retain(|d| *d != key);
    })
}

/// Record that the user declined to trust this repo (so we don't nag again this/next launch).
pub fn dismiss_project() -> Result<()> {
    let key = project_key();
    update_trust(|t| {
        if !t.dismissed.iter().any(|x| *x == key) {
            t.dismissed.push(key.clone());
        }
    })
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
        let m = err
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error");
        bail!("MCP error {code}: {m}");
    }
    msg.get("result")
        .cloned()
        .context("JSON-RPC response missing `result`")
}

/// Is this parsed message the response to request `id`? (Notifications/logs have no matching id.)
fn is_response_to(msg: &Value, id: u64) -> bool {
    msg.get("id").and_then(|v| v.as_u64()) == Some(id)
}

/// Observe server-initiated lifecycle notifications without mutating the live registry. A
/// `tools/list_changed` event only latches a dirty bit; the actual reconnect/re-list happens at the
/// next fresh user-turn boundary via [`prepare_fresh_turn`].
fn observe_server_message(msg: &Value) {
    if msg.get("method").and_then(|m| m.as_str()) == Some("notifications/tools/list_changed") {
        mark_tools_dirty();
    }
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

// ───────────────────────────── secret redaction ─────────────────────────────

/// The placeholder every masked secret collapses to — one token so tests can assert a sentinel is
/// gone AND that redaction fired.
const REDACTED: &str = "«redacted»";

/// Mask credentials in any string bound for an error, diagnostic, `/mcp` output, or tool result.
/// Two layers:
///   1. `known` — exact literal values (env var values, header values, bearer/refresh tokens) the
///      caller pulled from the live config/token. Any occurrence is replaced wholesale. This is the
///      strong layer: it catches a secret no matter where it surfaces (a stderr line, a URL, a body).
///   2. Generic patterns — `Bearer <...>` / `Basic <...>` authorization values, and `?token=`/
///      `?key=`/`?access_token=`/`?api_key=` query parameters — masked structurally so an unknown
///      credential (one we didn't pass in `known`) still doesn't leak.
/// Short/empty `known` values are skipped so we never blanket-replace a 1-char string across output.
fn redact_secrets(input: &str, known: &[String]) -> String {
    let mut s = input.to_string();
    for secret in known {
        let secret = secret.trim();
        // Guard: only mask values long enough to plausibly BE a secret — masking "a" or "" would
        // shred unrelated text. Real tokens/keys clear this easily.
        if secret.len() >= 6 {
            s = s.replace(secret, REDACTED);
        }
    }
    redact_generic(&s)
}

/// The structural pass of [`redact_secrets`]: mask `Bearer`/`Basic` auth values and known secret
/// query parameters even when the literal value wasn't supplied. Kept pure + separate so it's unit
/// -testable and so [`McpTransportError::new`] can run just this layer with no `known` list.
fn redact_generic(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < input.len() {
        // Match `Bearer ` / `Basic ` (case-insensitive) then swallow the token that follows.
        let rest = &input[i..];
        let lower_rest = rest.to_ascii_lowercase();
        if lower_rest.starts_with("bearer ") || lower_rest.starts_with("basic ") {
            let kw_len = if lower_rest.starts_with("bearer ") {
                "bearer ".len()
            } else {
                "basic ".len()
            };
            out.push_str(&rest[..kw_len]);
            // The credential runs until whitespace, a quote, or end-of-string.
            let after = &rest[kw_len..];
            let tok_end = after
                .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',')
                .unwrap_or(after.len());
            if tok_end > 0 {
                out.push_str(REDACTED);
            }
            i += kw_len + tok_end;
            continue;
        }
        // Match a secret query parameter `token=` / `key=` / `access_token=` / `api_key=` / `apikey=`
        // preceded by `?` or `&`, then swallow the value up to the next `&`, `#`, whitespace, or quote.
        if bytes[i] == b'?' || bytes[i] == b'&' {
            let param_rest = &input[i + 1..];
            let lower = param_rest.to_ascii_lowercase();
            let names = [
                "access_token=",
                "refresh_token=",
                "api_key=",
                "apikey=",
                "token=",
                "key=",
                "secret=",
                "password=",
            ];
            if let Some(name) = names.iter().find(|n| lower.starts_with(**n)) {
                out.push(bytes[i] as char);
                out.push_str(&param_rest[..name.len()]);
                let val = &param_rest[name.len()..];
                let val_end = val
                    .find(|c: char| {
                        c == '&' || c == '#' || c.is_whitespace() || c == '"' || c == '\''
                    })
                    .unwrap_or(val.len());
                if val_end > 0 {
                    out.push_str(REDACTED);
                }
                i += 1 + name.len() + val_end;
                continue;
            }
        }
        // Default: copy this char through.
        let ch = input[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// The set of literal secret values for one server (env values, header values, OAuth bearer/refresh
/// tokens) — the `known` list handed to [`redact_secrets`] so they never appear in any diagnostic.
fn server_secrets(
    cfg: &ServerConfig,
    token: Option<&crate::agent::mcp_oauth::TokenSet>,
) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    // Header VALUES are credentials (Authorization, X-Api-Key, cookies…). Names are safe to show.
    for hv in cfg.headers.values() {
        v.push(hv.clone());
        // A header value may be `Bearer <tok>` / `Basic <tok>` — also mask the bare token part so a
        // reworded diagnostic that drops the scheme still can't leak it.
        for scheme in ["Bearer ", "Basic ", "bearer ", "basic "] {
            if let Some(bare) = hv.strip_prefix(scheme) {
                v.push(bare.trim().to_string());
            }
        }
    }
    // Env VALUES commonly carry API keys/tokens for a stdio server.
    for ev in cfg.env.values() {
        v.push(ev.clone());
    }
    if let Some(t) = token {
        v.push(t.access_token.clone());
        if let Some(r) = &t.refresh_token {
            v.push(r.clone());
        }
        if let Some(sec) = &t.client_secret {
            v.push(sec.clone());
        }
    }
    v
}

// ───────────────────────────── typed transport errors ─────────────────────────────

/// The kind of a transport-level failure. Health decisions (poison? replay?) branch on the VARIANT,
/// never on a substring of a human message — a string match is fragile against reworded errors and
/// localized OS text. Every variant except the auth markers means the connection framing is
/// desynchronized or the request outcome is unknown, so the connection must be poisoned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportErrorKind {
    /// The peer closed the stream (EOF) before answering — stdio stdout hit `n == 0`.
    ConnectionClosed,
    /// Writing the request out failed (stdin write / HTTP POST send) — the request may be partial.
    SendFailed,
    /// Reading the response failed mid-stream (stdout read / body read / bad JSON body).
    ReadFailed,
    /// An SSE `text/event-stream` ended without a frame answering the dispatched id.
    SseTruncated,
    /// A per-call or schema-probe timeout elapsed — the request was dispatched, outcome unknown.
    Timeout,
    /// The user cancelled (Esc) after the request was dispatched — outcome unknown.
    Cancelled,
    /// Authentication is required/expired and could not be refreshed. Clean (no framing desync).
    AuthExpired,
    /// A reconnect revealed the server's live tool schema no longer matches what the model was shown.
    SchemaMismatch,
    /// The request was dispatched but its result can't be determined (garbage/partial response).
    AmbiguousResult,
}

impl TransportErrorKind {
    /// Whether a failure of this kind leaves the connection unsafe to reuse without a reconnect.
    /// Only the clean auth marker is non-poisoning; every framing/ambiguity failure poisons.
    fn poisons(self) -> bool {
        !matches!(self, TransportErrorKind::AuthExpired)
    }
}

/// A typed transport failure carried through `anyhow` (so it interleaves with the existing
/// `SessionExpired` / `Unauthorized` / `NeedsAuth` markers). `detail` is ALREADY redacted at
/// construction — it may be surfaced to the model / logs verbatim.
#[derive(Debug)]
struct McpTransportError {
    kind: TransportErrorKind,
    detail: String,
}

impl McpTransportError {
    fn new(kind: TransportErrorKind, detail: impl Into<String>) -> Self {
        // Defense in depth: redact generic auth/query credentials even if a caller forgot to.
        Self {
            kind,
            detail: redact_secrets(&detail.into(), &[]),
        }
    }
}

impl std::fmt::Display for McpTransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}
impl std::error::Error for McpTransportError {}

/// Classify an `anyhow` transport error into a health decision. Branches on the TYPED variant
/// (`McpTransportError` / the auth markers), never on a substring of the human message. Anything
/// unrecognized is treated as `AmbiguousResult` (poisons) — the safe default when the request was
/// dispatched but its outcome can't be proven.
fn classify_transport_error(e: &anyhow::Error) -> TransportErrorKind {
    if let Some(te) = e.downcast_ref::<McpTransportError>() {
        return te.kind;
    }
    // The recoverable auth/session markers are clean — the framing is intact, no poison needed.
    if e.downcast_ref::<Unauthorized>().is_some()
        || e.downcast_ref::<NeedsAuth>().is_some()
        || e.downcast_ref::<SessionExpired>().is_some()
    {
        return TransportErrorKind::AuthExpired;
    }
    // Unknown error after a dispatched request → ambiguous, poison to be safe.
    TransportErrorKind::AmbiguousResult
}

/// Whether an `anyhow` transport error should poison the connection: yes for every framing/ambiguity
/// failure, no ONLY for the clean recoverable auth/session markers.
fn err_poisons(e: &anyhow::Error) -> bool {
    classify_transport_error(e).poisons()
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
    /// Literal secret values (env/header/token) to strip from the stderr tail before it's surfaced.
    secrets: Vec<String>,
}

impl StdioTransport {
    /// The bounded stderr tail with any known secret + generic auth material masked — a child that
    /// echoes its own env/token in a log line must NOT leak it through a diagnostic.
    fn stderr_tail(&self) -> String {
        let raw = self
            .stderr
            .lock()
            .ok()
            .map(|b| b.trim().to_string())
            .unwrap_or_default();
        redact_secrets(&raw, &self.secrets)
    }
    /// Send a request line, then read newline-delimited messages until the one answering `id`,
    /// skipping interleaved notifications / log lines. Framing failures return a typed
    /// [`McpTransportError`] so the health layer branches on the VARIANT, not a substring.
    async fn request(&mut self, msg: &Value) -> Result<Value> {
        let id = msg
            .get("id")
            .and_then(|v| v.as_u64())
            .context("stdio request needs an id")?;
        let line = serde_json::to_string(msg)? + "\n";
        if let Err(e) = self.stdin.write_all(line.as_bytes()).await {
            return Err(anyhow::Error::new(McpTransportError::new(
                TransportErrorKind::SendFailed,
                format!("writing to MCP server stdin: {e}"),
            )));
        }
        self.stdin.flush().await.ok();
        loop {
            let mut buf = String::new();
            let n = match self.stdout.read_line(&mut buf).await {
                Ok(n) => n,
                Err(e) => {
                    return Err(anyhow::Error::new(McpTransportError::new(
                        TransportErrorKind::ReadFailed,
                        format!("reading MCP server stdout: {e}"),
                    )))
                }
            };
            if n == 0 {
                let tail = self.stderr_tail();
                let detail = if tail.is_empty() {
                    "MCP server closed stdout before answering".to_string()
                } else {
                    format!("MCP server exited before answering — stderr:\n{tail}")
                };
                return Err(anyhow::Error::new(McpTransportError::new(
                    TransportErrorKind::ConnectionClosed,
                    detail,
                )));
            }
            let trimmed = buf.trim();
            if trimmed.is_empty() {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(trimmed) else {
                continue;
            };
            observe_server_message(&v);
            if is_response_to(&v, id) {
                return Ok(v);
            }
            // else: a notification or a response to another id — ignore and keep reading.
        }
    }
    async fn notify(&mut self, msg: &Value) -> Result<()> {
        let line = serde_json::to_string(msg)? + "\n";
        self.stdin
            .write_all(line.as_bytes())
            .await
            .context("writing notification to MCP server")?;
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
    /// Server identity/version returned by `initialize.serverInfo`, retained for lifecycle status.
    server_info: Value,
    /// The mcp.json key, set ONLY for OAuth-enabled remotes (so we can find/refresh the cached token).
    oauth_key: Option<String>,
    /// The cached OAuth token (when `oauth_key` is set + a sign-in has happened). Attached as `Bearer`
    /// on every request and refreshed transparently on expiry / 401.
    token: Option<crate::agent::mcp_oauth::TokenSet>,
    /// Literal secret values (static header values) to strip from any diagnostic (error body, status).
    /// The OAuth bearer/refresh token is redacted generically (it's not stable across refreshes).
    secrets: Vec<String>,
}

impl HttpTransport {
    fn apply_headers(&self, mut rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        rb = rb
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .header(reqwest::header::CONTENT_TYPE, "application/json");
        for (k, v) in &self.headers {
            rb = rb.header(k.as_str(), v.as_str());
        }
        // OAuth bearer (only set for `auth: oauth` remotes that have signed in). These servers carry
        // no static `Authorization` header, so there's no conflict with the loop above.
        if let Some(t) = &self.token {
            rb = rb.header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", t.access_token),
            );
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

    /// The live secret list for redaction: the static header secrets plus the CURRENT OAuth token
    /// material (which rotates on refresh, so it can't be baked into `self.secrets` at connect time).
    fn live_secrets(&self) -> Vec<String> {
        let mut v = self.secrets.clone();
        if let Some(t) = &self.token {
            v.push(t.access_token.clone());
            if let Some(r) = &t.refresh_token {
                v.push(r.clone());
            }
            if let Some(sec) = &t.client_secret {
                v.push(sec.clone());
            }
        }
        v
    }

    async fn request(&mut self, msg: &Value) -> Result<Value> {
        let id = msg
            .get("id")
            .and_then(|v| v.as_u64())
            .context("http request needs an id")?;
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
                            anyhow::Error::new(NeedsAuth {
                                key: self.oauth_key.clone().unwrap_or_default(),
                            })
                        } else {
                            e2
                        }
                    })
                } else {
                    Err(anyhow::Error::new(NeedsAuth {
                        key: self.oauth_key.clone().unwrap_or_default(),
                    }))
                }
            }
            other => other,
        }
    }

    /// One send + read. Returns typed markers for the two recoverable failures (`SessionExpired` on a
    /// 404 with a session id, `Unauthorized` on a 401) so the wrappers can retry; every genuine
    /// transport failure is a typed [`McpTransportError`] (so the health layer branches on the
    /// variant, not a substring) with its detail already run through redaction.
    async fn send_and_read(&mut self, msg: &Value, id: u64) -> Result<Value> {
        // Snapshot the live secrets + a redacted endpoint up front so diagnostics never echo the
        // raw URL (it may carry `?token=`) or a header/token value.
        let secrets = self.live_secrets();
        let safe_url = redact_secrets(&self.url, &secrets);
        let rb = self.apply_headers(self.client.post(&self.url)).json(msg);
        let mut resp = match rb.send().await {
            Ok(r) => r,
            Err(e) => {
                // A send failure means the request may not have reached the server — treat as
                // `SendFailed` (request outcome unknown → poison).
                return Err(anyhow::Error::new(McpTransportError::new(
                    TransportErrorKind::SendFailed,
                    format!(
                        "POST {safe_url}: {}",
                        redact_secrets(&e.to_string(), &secrets)
                    ),
                )));
            }
        };
        // Capture/refresh the session id from the initialize response.
        if let Some(sid) = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|h| h.to_str().ok())
        {
            self.session_id = Some(sid.to_string());
        }
        let status = resp.status();
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|h| h.to_str().ok())
            .unwrap_or("")
            .to_string();
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
                self.server_info = Value::Null;
                return Err(anyhow::Error::new(SessionExpired));
            }
            let body = resp.text().await.unwrap_or_default();
            let body = redact_secrets(&body.chars().take(300).collect::<String>(), &secrets);
            // A non-2xx after the request was accepted leaves the outcome ambiguous → poison.
            return Err(anyhow::Error::new(McpTransportError::new(
                TransportErrorKind::AmbiguousResult,
                format!("MCP HTTP {} from {}: {}", status.as_u16(), safe_url, body),
            )));
        }
        if ctype.contains("text/event-stream") {
            // Stream the SSE response and return as soon as the frame answering `id` arrives — a
            // buffered `.text()` would block until the server CLOSES the stream (compliant servers
            // may hold it open for progress/keep-alive), hanging the whole call until the timeout.
            return read_sse_response(&mut resp, id).await;
        }
        let body = match resp.text().await {
            Ok(b) => b,
            Err(e) => {
                return Err(anyhow::Error::new(McpTransportError::new(
                    TransportErrorKind::ReadFailed,
                    format!(
                        "reading MCP HTTP response body: {}",
                        redact_secrets(&e.to_string(), &secrets)
                    ),
                )))
            }
        };
        match serde_json::from_str::<Value>(&body) {
            Ok(v) => Ok(v),
            // A dispatched request answered with a non-JSON / partial body: the outcome is unknown.
            Err(e) => Err(anyhow::Error::new(McpTransportError::new(
                TransportErrorKind::AmbiguousResult,
                format!("parsing MCP JSON response: {e}"),
            ))),
        }
    }

    async fn notify(&mut self, msg: &Value) -> Result<()> {
        // Notifications expect 202 Accepted with no body. We DO check the status: a 404 means the
        // session expired (surface it so the next request re-inits); other 4xx/5xx shouldn't pass
        // silently.
        let secrets = self.live_secrets();
        let safe_url = redact_secrets(&self.url, &secrets);
        let rb = self.apply_headers(self.client.post(&self.url)).json(msg);
        let resp = match rb.send().await {
            Ok(r) => r,
            Err(e) => {
                return Err(anyhow::Error::new(McpTransportError::new(
                    TransportErrorKind::SendFailed,
                    format!(
                        "POST notification {safe_url}: {}",
                        redact_secrets(&e.to_string(), &secrets)
                    ),
                )))
            }
        };
        let status = resp.status();
        if status.as_u16() == 404 && self.session_id.is_some() {
            self.session_id = None;
            self.protocol_version = None;
            self.server_info = Value::Null;
            return Err(anyhow!(SessionExpired));
        }
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let body = redact_secrets(&body.chars().take(200).collect::<String>(), &secrets);
            bail!(
                "MCP notify HTTP {} from {}: {}",
                status.as_u16(),
                safe_url,
                body
            );
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
    loop {
        match resp.chunk().await {
            Ok(Some(bytes)) => {
                for v in parser.push(&String::from_utf8_lossy(&bytes)) {
                    observe_server_message(&v);
                    if is_response_to(&v, id) {
                        return Ok(v);
                    }
                }
            }
            Ok(None) => break, // stream ended
            Err(e) => {
                // A mid-stream read error after the request was already dispatched: the outcome is
                // unknown and the connection framing is desynced → typed ReadFailed (poisons).
                return Err(anyhow::Error::new(McpTransportError::new(
                    TransportErrorKind::ReadFailed,
                    format!("reading MCP SSE chunk: {e}"),
                )));
            }
        }
    }
    if let Some(v) = parser.flush() {
        observe_server_message(&v);
        if is_response_to(&v, id) {
            return Ok(v);
        }
    }
    // The stream closed before the frame answering our dispatched id arrived → the request was sent
    // but never answered: typed SseTruncated (poisons; outcome unknown).
    Err(anyhow::Error::new(McpTransportError::new(
        TransportErrorKind::SseTruncated,
        format!("MCP SSE stream ended without a response to id {id}"),
    )))
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
    /// The mcp.json key — carried so a poisoned connection can rebuild its own transport.
    name: String,
    /// The server's config — the recipe to respawn/reconnect the transport in place.
    cfg: ServerConfig,
    transport: Transport,
    next_id: u64,
    /// A connection-level transport failure (EOF, send/read error, timeout) desyncs stdio framing
    /// and leaves the HTTP session ambiguous. Once poisoned the connection must NOT be reused for a
    /// new call until a successful `reconnect()` — a read-only caller may reconnect+replay once; a
    /// destructive caller must not auto-replay (its side effect may already have happened).
    poisoned: bool,
    /// Bumped on every successful (re)connect. A stable generation across a turn means the tool
    /// schema the model was shown still matches the live server.
    generation: u64,
}

impl Connection {
    fn next_id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn is_poisoned(&self) -> bool {
        self.poisoned
    }

    /// Mark the connection unusable after a transport-level failure (timeout / EOF / mid-call cancel).
    fn mark_poisoned(&mut self) {
        self.poisoned = true;
    }

    /// The current connection generation (bumped on every successful (re)connect).
    fn generation(&self) -> u64 {
        self.generation
    }

    /// Re-list the live server's tools and hash them, to detect a schema change after a reconnect.
    /// `None` when the re-list itself fails/times out — AND in that case the connection is left
    /// POISONED so the caller can never fall back to a stale advertised schema: a failed verify is
    /// treated exactly like "the schema changed" (never replay, rebuild the registry next turn).
    async fn schema_hash_now(&mut self) -> Option<u64> {
        match tokio::time::timeout(CONNECT_TIMEOUT, self.list_tools()).await {
            Ok(Ok(tools)) => {
                let tools: Vec<ToolMeta> = tools
                    .into_iter()
                    .filter(|t| self.cfg.allows(&t.name))
                    .collect();
                Some(schema_hash(&tools))
            }
            // Timeout, or a `tools/list` error: we can NOT confirm the live schema. Poison so no caller
            // reuses this connection on a stale schema, and report "unknown" (None → caller won't replay).
            // A `raw_call` error already sets poison; a bare timeout (outer future) does not — pin it here.
            _ => {
                self.poisoned = true;
                None
            }
        }
    }

    /// Rebuild the transport from `self.cfg` and re-run the handshake. Clears poison + bumps the
    /// generation on success; leaves it poisoned on failure so the caller can surface a clean error.
    async fn reconnect(&mut self) -> Result<()> {
        let transport = if self.cfg.command.is_some() {
            connect_stdio(&self.cfg).await?
        } else if self.cfg.url.is_some() {
            connect_http(&self.name, &self.cfg)?
        } else {
            bail!(
                "server `{}` has neither `command` (stdio) nor `url` (http)",
                self.name
            );
        };
        self.transport = transport;
        self.next_id = 0;
        tokio::time::timeout(CONNECT_TIMEOUT, self.initialize())
            .await
            .map_err(|_| {
                anyhow!(
                    "`{}` re-initialize timed out after {}s",
                    self.name,
                    CONNECT_TIMEOUT.as_secs()
                )
            })??;
        self.poisoned = false;
        self.generation += 1;
        Ok(())
    }

    async fn call(&mut self, method: &str, params: Value) -> Result<Value> {
        let first = self.raw_call(method, params.clone()).await;
        match first {
            // HTTP session expired (404) → re-run the handshake and replay the request exactly once.
            // `initialize` itself is never retried (it can't expire a session it hasn't made yet).
            Err(e) if method != "initialize" && e.downcast_ref::<SessionExpired>().is_some() => {
                self.initialize()
                    .await
                    .context("re-initializing after MCP HTTP session expiry")?;
                self.poisoned = false; // a clean re-init after 404 restores the connection
                self.raw_call(method, params).await
            }
            other => other,
        }
    }

    async fn raw_call(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id();
        let msg = rpc_request(id, method, params);
        let resp = match self.transport.request(&msg).await {
            Ok(r) => {
                // A complete response re-synchronizes the connection, including JSON-RPC errors —
                // the request boundary is known. This also makes the caller's pre-request poison
                // marker cancellation-safe: if its future is dropped mid-read, this line is never
                // reached and the connection stays poisoned for the next same-turn caller.
                self.poisoned = false;
                r
            }
            Err(e) => {
                // A transport-level failure (EOF / send / read / SSE-truncation / timeout / cancel)
                // desyncs the connection. The decision is made on the TYPED variant, not a substring:
                // only the clean recoverable markers (session expiry, 401, needs-auth) leave framing
                // intact; every framing/ambiguity failure poisons so the next same-turn caller hits
                // the gate rather than reusing a desynced connection.
                if err_poisons(&e) {
                    self.poisoned = true;
                }
                return Err(e);
            }
        };
        rpc_result(resp) // an application/protocol error here means the connection is still alive
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
        // Pin negotiated lifecycle metadata on the HTTP transport for subsequent requests/status.
        if let Transport::Http(h) = &mut self.transport {
            if let Some(pv) = result.get("protocolVersion").and_then(|v| v.as_str()) {
                h.protocol_version = Some(pv.to_string());
            }
            h.server_info = result.get("serverInfo").cloned().unwrap_or(Value::Null);
        }
        self.transport
            .notify(&rpc_notification("notifications/initialized", json!({})))
            .await?;
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
        if result
            .get("isError")
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
        {
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
                            && i.get("text")
                                .and_then(|t| t.as_str())
                                .is_some_and(|s| !s.trim().is_empty())
                    })
                })
                .unwrap_or(false);
            let msg = if has_text {
                msg
            } else {
                serde_json::to_string(&result).unwrap_or(msg)
            };
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
        Some(Self {
            name,
            description,
            input_schema,
            read_only,
        })
    }
}

// ───────────────────────────── manager (process-global) ─────────────────────────────

/// A connected server: a shared connection + the tools we'll expose from it.
struct ServerHandle {
    name: String,
    conn: Arc<Mutex<Connection>>,
    tools: Vec<ToolMeta>,
    server_info: Value,
    /// Canonical hash of the exposed tool schemas at connect time. `notifications/tools/list_changed`
    /// marks the manager dirty, but the schema the model sees must NOT change mid-run — the next
    /// user-turn registry rebuild reconnects, recomputes this, and only then re-exposes changed tools.
    schema_hash: u64,
}

/// Canonical, order-independent hash of a tool set's (name, schema) pairs — the pin that lets a
/// reconnect detect "the server's tools changed underneath us" without trusting field ordering.
fn schema_hash(tools: &[ToolMeta]) -> u64 {
    let mut pairs: Vec<String> = tools
        .iter()
        .map(|t| {
            format!(
                "{}\u{1}{}\u{1}{}",
                t.name,
                serde_json::to_string(&t.input_schema).unwrap_or_default(),
                if t.read_only { "ro" } else { "rw" }
            )
        })
        .collect();
    pairs.sort();
    fnv1a(&pairs.join("\u{2}"))
}

/// All connected MCP servers, established once per process.
pub struct Manager {
    servers: Vec<ServerHandle>,
    /// Monotonic manager generation: increments each time the process rebuilds all MCP connections.
    /// Combined with each connection's local generation this makes lifecycle state observable without
    /// exposing credentials or endpoint headers.
    generation: u64,
}

static MANAGER: std::sync::RwLock<Option<Manager>> = std::sync::RwLock::new(None);
static NEXT_MANAGER_GENERATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// A server sent `notifications/tools/list_changed`: its tool surface changed. We do NOT swap the
/// registry mid-run (a schema change under an in-flight call would invalidate the arguments the
/// model already produced) — instead the flag is latched and `prepare_fresh_turn()` reconnects at
/// the next user-turn boundary, where the new surface can be re-listed and re-exposed safely.
static TOOLS_DIRTY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Latch a `tools/list_changed` observation (called from the transport read loop). Cheap + lock-free
/// so it's safe to call from inside a request/notification scan.
pub fn mark_tools_dirty() {
    TOOLS_DIRTY.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Fresh-user-turn boundary hook. When a server announced `tools/list_changed` since the last turn,
/// we do NOT blindly drop every connection. Instead — for servers that advertise `listChanged` — we
/// run a bounded, CONSERVATIVE schema probe on each server's CURRENT connection (never a second,
/// competing reader): a `tools/list` that drains the pending notification and yields a fresh hash.
///   • Every server still matches its pinned hash → the change was a no-op; KEEP the manager and its
///     generation (the schemas the model saw last turn are still live).
///   • Any server's schema moved, or the probe timed out / errored (ambiguous) → `invalidate()` so
///     the NEXT registry build reconnects, re-lists, and re-exposes the changed surface safely. A
///     server that then fails to rebuild is omitted by `build_manager` (never exposed on a stale
///     schema). A failed/timed-out probe also poisons that connection (see `schema_hash_now`).
/// Deliberately a no-op mid-run — only the top-level REPL calls this between turns, never inside
/// `run_agent_loop`, so advertised schemas stay pinned for the duration of a run. Idempotent.
pub fn prepare_fresh_turn() {
    if !TOOLS_DIRTY.swap(false, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    // No runtime (unit tests) → fall back to the safe blunt behavior (drop + rebuild next build).
    if tokio::runtime::Handle::try_current().is_err() {
        invalidate();
        return;
    }
    // Probe on the current connections; only invalidate if something actually changed / is unclear.
    if !block(all_schemas_unchanged()) {
        invalidate();
    }
}

/// Probe every connected server's live tool schema on its CURRENT connection (no competing reader)
/// and return whether ALL still match their pinned hash. `true` also when nothing is built (nothing
/// to invalidate). Snapshots the connection handles under a short read lock, then releases it before
/// awaiting so the probe never holds the manager lock across I/O.
async fn all_schemas_unchanged() -> bool {
    let probes: Vec<(Arc<Mutex<Connection>>, u64)> = {
        let guard = MANAGER.read().unwrap();
        let Some(mgr) = guard.as_ref() else {
            return true;
        };
        mgr.servers
            .iter()
            .map(|s| (s.conn.clone(), s.schema_hash))
            .collect()
    };
    for (conn, pinned) in probes {
        let mut c = conn.lock().await;
        // A poisoned connection can't be trusted for a probe → treat as changed (force a rebuild).
        if c.is_poisoned() {
            return false;
        }
        match c.schema_hash_now().await {
            Some(h) if h == pinned => {} // unchanged → keep this server
            _ => return false,           // changed / failed / timed out (poison held) → rebuild
        }
    }
    true
}

/// Spawn a stdio child server and return its transport. On **Windows** the launch goes through
/// `cmd /C` because the common runners (`npx`, `uvx`, `dnx`, `bunx`) are `.cmd`/`.bat` shims that
/// `CreateProcessW` — what `Command::new` uses — cannot execute directly (this was the #1 reason
/// EVERY local app failed to connect on Windows). Child **stderr** is drained into a bounded buffer
/// so a failed handshake reports the real cause (bad token / missing dep / crash), not a bare EOF.
async fn connect_stdio(cfg: &ServerConfig) -> Result<Transport> {
    let command = cfg
        .command
        .as_ref()
        .context("stdio server needs `command`")?;
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
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning MCP server `{command}`"))?;
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
    Ok(Transport::Stdio(StdioTransport {
        _child: child,
        stdin,
        stdout: BufReader::new(stdout),
        stderr: stderr_buf,
        // Env values commonly carry the child's API key/token — mask them out of the stderr tail.
        secrets: server_secrets(cfg, None),
    }))
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
        (
            Some(name.to_string()),
            crate::agent::mcp_oauth::load_token(name),
        )
    } else {
        (None, None)
    };
    let secrets = server_secrets(cfg, token.as_ref());
    Ok(Transport::Http(HttpTransport {
        client,
        url,
        headers: cfg.headers.clone(),
        session_id: None,
        protocol_version: None,
        server_info: Value::Null,
        oauth_key,
        token,
        secrets,
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
    let mut conn = Connection {
        name: name.to_string(),
        cfg: cfg.clone(),
        transport,
        next_id: 0,
        poisoned: false,
        generation: 0,
    };
    let server_info = match tokio::time::timeout(CONNECT_TIMEOUT, conn.initialize()).await {
        Ok(Ok(si)) => si,
        Ok(Err(e)) => {
            let tail = conn.transport.stderr_tail();
            return Err(if tail.is_empty() {
                e
            } else {
                e.context(format!("server stderr:\n{tail}"))
            });
        }
        Err(_) => {
            let tail = conn.transport.stderr_tail();
            let suffix = if tail.is_empty() {
                String::new()
            } else {
                format!(" — stderr:\n{tail}")
            };
            bail!(
                "`{name}` initialize timed out after {}s{suffix}",
                CONNECT_TIMEOUT.as_secs()
            );
        }
    };
    let tools = match tokio::time::timeout(CONNECT_TIMEOUT, conn.list_tools()).await {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => return Err(e),
        Err(_) => bail!(
            "`{name}` tools/list timed out after {}s",
            CONNECT_TIMEOUT.as_secs()
        ),
    };
    let tools: Vec<ToolMeta> = tools.into_iter().filter(|t| cfg.allows(&t.name)).collect();
    let schema_hash = schema_hash(&tools);
    Ok(ServerHandle {
        name: name.to_string(),
        conn: Arc::new(Mutex::new(conn)),
        tools,
        server_info,
        schema_hash,
    })
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
    let sc = cfg
        .servers
        .get(name)
        .with_context(|| format!("no connected app keyed '{name}' in mcp.json"))?;
    let handle = connect_one(name, sc).await?;
    Ok(ProbeReport {
        server_info: handle.server_info,
        tools: handle
            .tools
            .into_iter()
            .map(|t| ProbeTool {
                name: t.name,
                description: t.description,
                read_only: t.read_only,
            })
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
        .header(
            reqwest::header::ACCEPT,
            "application/json, text/event-stream",
        )
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(&init)
        .send()
        .await
        .ok()?;
    if resp.status().as_u16() == 401 {
        return resp
            .headers()
            .get("WWW-Authenticate")
            .and_then(|h| h.to_str().ok())
            .map(|s| s.to_string());
    }
    None
}

/// Interactive OAuth sign-in for one configured remote server key: probe its `401` challenge → run
/// the full PKCE flow (browser + loopback) → cache the token → invalidate the manager so the next
/// message reconnects WITH the bearer. Used by `ng apps login` / `ng mcp login`.
pub async fn login(key: &str) -> Result<()> {
    let cfg = load_config()?.context("no mcp.json — add an app first (`aizen apps add <name>`)")?;
    let sc = cfg
        .servers
        .get(key)
        .with_context(|| format!("no app keyed '{key}' in mcp.json"))?;
    let url = sc
        .url
        .clone()
        .context("`login` applies only to remote (url) servers")?;
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
    let generation = NEXT_MANAGER_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let enabled: Vec<(String, ServerConfig)> = cfg
        .servers
        .iter()
        .filter(|(_, sc)| sc.enabled)
        .map(|(n, sc)| (n.clone(), sc.clone()))
        .collect();
    if enabled.is_empty() {
        return Manager {
            servers: Vec::new(),
            generation,
        };
    }
    // Through the TUI funnel, never `eprintln!`: `discovered_tools()` runs from
    // `default_registry_in`, which rebuilds the tool surface on EVERY turn, so a lazy reconnect can
    // fire mid-turn. A raw print there lands in the terminal behind the retained render thread's
    // back and gets folded into later frames as stale cells.
    crate::ui::tui::note_line(
        &console::style(format!("mcp: connecting {} server(s)…", enabled.len()))
            .dim()
            .to_string(),
    );

    // An overall ceiling per server so even a hang at spawn / first-run package download (which is
    // OUTSIDE the inner handshake timeouts) can't wedge the connect. JoinSet → run all at once.
    let overall = CONNECT_TIMEOUT * 3;
    let mut set: tokio::task::JoinSet<(String, Result<ServerHandle>)> = tokio::task::JoinSet::new();
    for (name, sc) in enabled {
        set.spawn(async move {
            let r = match tokio::time::timeout(overall, connect_one(&name, &sc)).await {
                Ok(r) => r,
                Err(_) => Err(anyhow!(
                    "`{name}` connect timed out after {}s",
                    overall.as_secs()
                )),
            };
            (name, r)
        });
    }

    let mut servers = Vec::new();
    while let Some(joined) = set.join_next().await {
        match joined {
            Ok((_, Ok(h))) => servers.push(h),
            Ok((name, Err(e))) => crate::ui::tui::note_line(
                &console::style(format!("mcp: server '{name}' skipped — {e:#}"))
                    .yellow()
                    .to_string(),
            ),
            Err(je) => crate::ui::tui::note_line(
                &console::style(format!("mcp: a connect task failed — {je}"))
                    .yellow()
                    .to_string(),
            ),
        }
    }
    // JoinSet completes out of order; sort for a deterministic tool listing across runs.
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    Manager {
        servers,
        generation,
    }
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
            let generation =
                NEXT_MANAGER_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            *guard = Some(Manager {
                servers: Vec::new(),
                generation,
            }); // no file / nothing enabled → cache empty
            return;
        }
        Err(e) => {
            crate::ui::tui::note_line(&console::style(format!("mcp: {e}")).yellow().to_string());
            let generation =
                NEXT_MANAGER_GENERATION.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            *guard = Some(Manager {
                servers: Vec::new(),
                generation,
            });
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

// (schema_hash is defined once near ServerHandle — see above.)

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
    let Some(mgr) = guard.as_ref() else {
        return Vec::new();
    };
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
                        // `discovered_tools` runs from `default_registry_in`, i.e. once per turn —
                        // so this note must go through the funnel, not straight to the terminal.
                        crate::ui::tui::note_line(
                            &console::style(format!(
                                "mcp: tool name '{qn}' collides — exposing as '{cand}'"
                            ))
                            .dim()
                            .to_string(),
                        );
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
                // Pin the server's tool-schema hash + connect-time generation (0). A reconnect
                // mid-turn re-lists the tools and bumps the generation to 1; if the re-listed hash
                // differs the live surface no longer matches what the model was shown, so the call
                // isn't replayed and the registry rebuilds next turn.
                built_generation: 0,
                built_schema_hash: srv.schema_hash,
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
        return format!(
            "{trust_note}No MCP servers connected (all disabled or failed to connect)."
        );
    }
    let mut s = trust_note;
    s.push_str(&format!(
        "MCP generation {} · schema pinned per turn\n",
        mgr.generation
    ));
    for srv in &mgr.servers {
        let info = srv
            .server_info
            .get("name")
            .and_then(|n| n.as_str())
            .map(|n| format!(" ({n})"))
            .unwrap_or_default();
        let (conn_gen, health) = match srv.conn.try_lock() {
            Ok(c) => (
                c.generation(),
                if c.is_poisoned() {
                    "poisoned"
                } else {
                    "healthy"
                },
            ),
            Err(_) => (0, "busy"),
        };
        s.push_str(&format!(
            "● {}{} — {} tool(s) · conn gen {} · {} · schema {:016x}\n",
            srv.name,
            info,
            srv.tools.len(),
            conn_gen,
            health,
            srv.schema_hash,
        ));
        for t in &srv.tools {
            let ro = if t.read_only { " [read-only]" } else { "" };
            s.push_str(&format!(
                "    {}{}\n",
                qualified_name(&srv.name, &t.name),
                ro
            ));
        }
    }
    s.trim_end().to_string()
}

/// One `tools/call`, wall-clock-capped AND raced against the turn's cancel token, mapping the two
/// non-completion outcomes to TYPED transport errors so the health layer branches on the variant
/// (never a substring): a `Timeout` when the cap elapses, a `Cancelled` when Esc wins. Both leave the
/// caller's pre-set poison marker in place (the request was dispatched, outcome unknown) so the
/// deterministic tail invalidates the manager. `after_reconnect` only tunes the timeout wording.
async fn call_once(
    c: &mut Connection,
    name: &str,
    args: &Value,
    cancel: Option<&crate::core::cancel::TurnCancel>,
    server: &str,
    after_reconnect: bool,
) -> Result<String> {
    let timed = tokio::time::timeout(CALL_TIMEOUT, c.call_tool(name, args));
    let outcome = match cancel {
        Some(tok) => tokio::select! {
            biased;
            _ = tok.cancelled() => {
                return Err(anyhow::Error::new(McpTransportError::new(
                    TransportErrorKind::Cancelled,
                    format!("MCP tool '{server}/{name}' cancelled by user"),
                )));
            }
            r = timed => r,
        },
        None => timed.await,
    };
    match outcome {
        Ok(r) => r,
        Err(_) => {
            let where_ = if after_reconnect {
                " after reconnect"
            } else {
                ""
            };
            Err(anyhow::Error::new(McpTransportError::new(
                TransportErrorKind::Timeout,
                format!(
                    "MCP tool '{server}/{name}' timed out{where_} after {}s",
                    CALL_TIMEOUT.as_secs()
                ),
            )))
        }
    }
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
    /// The connection generation this tool's schema was built against. If a reconnect bumps the
    /// live generation AND the re-listed schema hash differs, the tool the model was shown no longer
    /// matches the server — fail this call and rebuild the registry on the next turn rather than
    /// calling a tool whose shape silently changed mid-run.
    built_generation: u64,
    /// The server's canonical tool-schema hash at build time (same value across every tool of a
    /// server). Compared after an in-place reconnect to detect a schema change.
    built_schema_hash: u64,
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
        // The shared bridge is valid on spawn_blocking threads, so read-only MCP calls may run
        // concurrently; same-server calls still serialize correctly on the connection's own
        // Arc<Mutex<Connection>>. Destructive-annotated MCP tools stay serial anyway via the
        // `!is_destructive` half of the executor's safety check.
        true
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let conn = self.conn.clone();
        let name = self.remote_name.clone();
        let server = self.server.clone();
        let destructive = self.destructive;
        let built_gen = self.built_generation;
        let built_hash = self.built_schema_hash;
        // Coerce null/absent → {}, but DON'T silently discard a real non-object value (a bare
        // string/array/number means the model called the tool wrong — tell it, don't drop the arg).
        let args = match args {
            Value::Object(_) => args.clone(),
            Value::Null => json!({}),
            _ => bail!(
                "MCP tool '{server}/{name}' expects a JSON object for arguments, got {}",
                short_kind(args)
            ),
        };
        // The shared cancel-aware bridge: Esc aborts a slow MCP call instead of blocking the turn.
        let out = crate::agent::tools::block_for_tool(async move {
            let mut c = conn.lock().await;
            // The turn's cancel token (set on this thread by the sync tool bridge). Racing it INSIDE
            // the body — not only in the outer bridge — lets an Esc mid-call resolve to a typed
            // `Cancelled` error whose poison flag is observed by the deterministic tail, so the cached
            // Manager is invalidated rather than left holding a half-used connection.
            let cancel = crate::core::cancel::current();

            // The call + at-most-one recovery, computed as its own value so the SINGLE tail below can
            // make the invalidation decision deterministically from the connection's final poison
            // state — never from string-matching an error message.
            let result: Result<String> = async {
                // POISON GATE: a prior transport failure this turn left the connection desynced. A
                // read-only tool may reconnect+replay once (its call has no side effect to double). A
                // destructive tool must NOT auto-reconnect-and-run: the earlier failure's side effect
                // may already have landed on the server, so silently re-running risks a double
                // mutation. Fail clean and let the next user turn rebuild against a fresh connection.
                if c.is_poisoned() {
                    if destructive {
                        bail!(
                            "MCP tool '{server}/{name}' not run: the connection was reset after an \
                             earlier error and this is a state-changing tool — its previous effect may \
                             already have happened, so it is not auto-retried. It will reconnect on \
                             your next message."
                        );
                    }
                    c.reconnect()
                        .await
                        .map_err(|e| e.context(format!("reconnecting to MCP server '{server}' after a transport error")))?;
                    // A failed/timed-out schema verify leaves the connection poisoned (see
                    // `schema_hash_now`), so a stale schema can never be replayed against.
                    let live_hash = c.schema_hash_now().await;
                    if c.generation() <= built_gen || live_hash != Some(built_hash) {
                        // Poison so the deterministic tail invalidates the (now stale) registry even
                        // when the reconnect itself succeeded — the advertised schema no longer matches.
                        c.mark_poisoned();
                        return Err(anyhow::Error::new(McpTransportError::new(
                            TransportErrorKind::SchemaMismatch,
                            format!(
                                "MCP server '{server}' changed its tool schema while reconnecting; \
                                 '{name}' was not replayed. The updated tools will be available on \
                                 your next message."
                            ),
                        )));
                    }
                }

                // Mark BEFORE awaiting transport I/O. If Esc drops this future mid-read, no cleanup
                // code runs; the marker survives and prevents another live McpTool clone from using a
                // desynchronized connection. `raw_call` clears it only after a complete response.
                c.mark_poisoned();
                let first = call_once(&mut c, &name, &args, cancel.as_ref(), &server, false).await;

                // A read-only call that failed on a freshly-poisoned connection (EOF/send/read
                // mid-call) gets ONE reconnect+replay — the read has no side effect to double. A
                // destructive call is NEVER auto-replayed (poison stays set → the next same-server
                // call hits the gate above). If a reconnect reveals a CHANGED schema, don't replay
                // against a tool the model never saw.
                if first.is_err() && !destructive && c.is_poisoned() {
                    if c.reconnect().await.is_err() {
                        return first; // still poisoned → tail invalidates
                    }
                    if c.generation() <= built_gen || c.schema_hash_now().await != Some(built_hash) {
                        // Schema moved / verify failed. `schema_hash_now` holds poison on a failed
                        // verify; force it on a clean-but-changed schema too so the tail invalidates.
                        c.mark_poisoned();
                        return first; // don't replay against a tool the model never saw
                    }
                    c.mark_poisoned(); // same cancellation-safety invariant for the replay await
                    return call_once(&mut c, &name, &args, cancel.as_ref(), &server, true).await;
                }
                first
            }
            .await;

            // DETERMINISTIC INVALIDATION: whenever the connection ends this call poisoned (broken
            // transport, timeout, cancel, or a destructive gate hit), the OTHER live McpTool clones
            // this turn can't safely reuse it — null the cached Manager BEFORE returning so the next
            // registry build reconnects from scratch. No string-matching involved.
            if c.is_poisoned() {
                invalidate();
            }
            result
        });
        out
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
        assert_eq!(
            qualified_name("My Server", "read-file"),
            "mcp_my_server_read_file"
        );
        // Non-ASCII / punctuation-only names must NOT collapse to "" (the collision bug): each gets a
        // short stable hash so distinct names stay distinguishable.
        assert!(slug("検索").starts_with('x') && slug("検索").len() > 1);
        assert_ne!(
            slug("検索"),
            slug("コード"),
            "distinct non-ASCII names stay distinct"
        );
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
        assert!(
            !fs.allows("write_file"),
            "include-list excludes everything else"
        );
        let rem = &cfg.servers["rem"];
        assert_eq!(rem.url.as_deref(), Some("https://x/mcp"));
        assert!(rem.allows("anything"));
        assert!(!rem.allows("danger"), "exclude drops it");
        assert!(!cfg.servers["off"].enabled);
    }

    #[test]
    fn project_mcp_merges_only_when_trusted() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let home = std::env::temp_dir().join(format!("ng-mcp-home-{}", std::process::id()));
        let proj = std::env::temp_dir().join(format!("ng-mcp-proj-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        let _ = std::fs::remove_dir_all(&proj);
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(proj.join(".nextgen")).unwrap();
        std::env::set_var("NEXTGEN_HOME", &home);
        std::env::set_var("NG_PROJECT_ROOT", &proj);

        std::fs::write(
            home.join("mcp.json"),
            r#"{"mcpServers":{"h":{"command":"home"}}}"#,
        )
        .unwrap();
        std::fs::write(
            proj.join(".nextgen").join("mcp.json"),
            r#"{"mcpServers":{"p":{"command":"proj"},"h":{"command":"proj-override"}}}"#,
        )
        .unwrap();

        // Untrusted: HOME only; the project servers await a decision.
        let c = load_config().unwrap().unwrap();
        assert_eq!(c.servers.len(), 1, "untrusted → project servers not loaded");
        assert_eq!(c.servers["h"].command.as_deref(), Some("home"));
        assert_eq!(
            project_trust_prompt(),
            Some(2),
            "untrusted, non-dismissed → prompt for 2 servers"
        );

        // Trust: merged, project wins on the colliding key.
        trust_project().unwrap();
        let c = load_config().unwrap().unwrap();
        assert_eq!(c.servers.len(), 2, "trusted → project merged");
        assert_eq!(
            c.servers["h"].command.as_deref(),
            Some("proj-override"),
            "project wins on collision"
        );
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
    fn tools_list_changed_is_deferred_until_fresh_turn() {
        TOOLS_DIRTY.store(false, std::sync::atomic::Ordering::Relaxed);
        observe_server_message(
            &json!({"jsonrpc":"2.0","method":"notifications/tools/list_changed"}),
        );
        assert!(TOOLS_DIRTY.load(std::sync::atomic::Ordering::Relaxed));
        // A plain notification cannot mutate the live manager/tool registry in place; only the
        // explicit fresh-turn hook consumes it.
        prepare_fresh_turn();
        assert!(!TOOLS_DIRTY.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn schema_hash_is_order_independent_and_tracks_safety() {
        let ro = ToolMeta {
            name: "x".into(),
            description: String::new(),
            input_schema: json!({"type":"object"}),
            read_only: true,
        };
        let rw = ToolMeta {
            name: "y".into(),
            read_only: false,
            ..ro.clone()
        };
        assert_eq!(
            schema_hash(&[ro.clone(), rw.clone()]),
            schema_hash(&[rw.clone(), ro.clone()])
        );
        let changed = ToolMeta {
            read_only: false,
            ..ro.clone()
        };
        assert_ne!(
            schema_hash(&[ro]),
            schema_hash(&[changed]),
            "read-only hint is part of the pin"
        );
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
        let hit = found
            .iter()
            .find(|v| is_response_to(v, 3))
            .expect("should find id=3");
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
        assert!(
            evs.iter().any(|v| is_response_to(v, 7)),
            "multi-line data event must parse"
        );
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
        assert!(
            out.contains("\"answer\""),
            "structuredContent should be surfaced: {out}"
        );
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
        assert_eq!(
            m.input_schema["properties"]["path"]["type"],
            json!("string")
        );

        // No annotations → destructive-by-default (read_only=false).
        let t2 = json!({"name": "rm", "description": "delete"});
        let m2 = ToolMeta::from_value(&t2).unwrap();
        assert!(!m2.read_only);
        assert!(
            m2.input_schema["type"] == json!("object"),
            "defaults to object schema"
        );
    }

    #[test]
    fn redact_masks_known_secrets_and_generic_credentials() {
        // Layer 1: a known literal value is masked wherever it appears (stderr line, URL, body).
        let secret = "sk-supersecrettoken123456";
        let line = format!("child crashed: API_KEY={secret} at startup");
        let out = redact_secrets(&line, &[secret.to_string()]);
        assert!(
            !out.contains(secret),
            "known secret must not survive: {out}"
        );
        assert!(
            out.contains(REDACTED),
            "redaction sentinel must fire: {out}"
        );

        // Layer 2 (generic, no `known` list): Bearer / Basic auth values.
        let bearer = redact_generic("Authorization: Bearer abc.def.ghij tail");
        assert!(
            !bearer.contains("abc.def.ghij"),
            "bearer token leaked: {bearer}"
        );
        assert!(
            bearer.contains("Bearer") && bearer.contains("tail"),
            "kept scheme + trailing: {bearer}"
        );
        let basic = redact_generic("hdr Basic dXNlcjpwYXNz,next");
        assert!(
            !basic.contains("dXNlcjpwYXNz"),
            "basic cred leaked: {basic}"
        );

        // Layer 2: secret query parameters, value swallowed to the next delimiter.
        let q = redact_generic("GET https://h/mcp?access_token=zzz999secret&page=2#frag");
        assert!(!q.contains("zzz999secret"), "query token leaked: {q}");
        assert!(
            q.contains("page=2") && q.contains("#frag"),
            "non-secret params kept: {q}"
        );

        // Guard: a short `known` value is NOT blanket-replaced (would shred unrelated text).
        let short = redact_secrets("the letter a appears here", &["a".to_string()]);
        assert_eq!(
            short, "the letter a appears here",
            "short secrets must not be masked"
        );
    }

    #[test]
    fn transport_error_kinds_drive_poison_decision() {
        // Every framing/ambiguity failure poisons; only the clean auth marker does not.
        for kind in [
            TransportErrorKind::ConnectionClosed,
            TransportErrorKind::SendFailed,
            TransportErrorKind::ReadFailed,
            TransportErrorKind::SseTruncated,
            TransportErrorKind::Timeout,
            TransportErrorKind::Cancelled,
            TransportErrorKind::SchemaMismatch,
            TransportErrorKind::AmbiguousResult,
        ] {
            assert!(kind.poisons(), "{kind:?} must poison the connection");
        }
        assert!(
            !TransportErrorKind::AuthExpired.poisons(),
            "clean auth marker must not poison"
        );

        // classify_transport_error branches on the TYPE, never a substring.
        let te = anyhow::Error::new(McpTransportError::new(
            TransportErrorKind::ConnectionClosed,
            "EOF",
        ));
        assert_eq!(
            classify_transport_error(&te),
            TransportErrorKind::ConnectionClosed
        );
        assert!(err_poisons(&te));

        // The recoverable auth/session markers classify as AuthExpired (clean, no poison).
        for e in [
            anyhow::Error::new(Unauthorized),
            anyhow::Error::new(SessionExpired),
            anyhow::Error::new(NeedsAuth { key: "x".into() }),
        ] {
            assert_eq!(
                classify_transport_error(&e),
                TransportErrorKind::AuthExpired
            );
            assert!(!err_poisons(&e), "auth marker must not poison");
        }

        // An UNKNOWN error after a dispatched request is treated as ambiguous → poison (safe default).
        let unknown = anyhow!("something odd happened");
        assert_eq!(
            classify_transport_error(&unknown),
            TransportErrorKind::AmbiguousResult
        );
        assert!(err_poisons(&unknown));
    }

    #[test]
    fn transport_error_detail_is_redacted_at_construction() {
        // A caller that forgets to redact still can't leak a Bearer through the typed detail.
        let e = McpTransportError::new(
            TransportErrorKind::ReadFailed,
            "POST failed: Bearer leaky.token.here",
        );
        assert!(
            !e.detail.contains("leaky.token.here"),
            "detail must be redacted: {}",
            e.detail
        );
    }

    #[test]
    fn server_secrets_collects_header_env_and_token_material() {
        let mut headers = BTreeMap::new();
        headers.insert(
            "Authorization".to_string(),
            "Bearer header-tok-abcdef".to_string(),
        );
        headers.insert("X-Api-Key".to_string(), "xapikey-value-123".to_string());
        let mut env = BTreeMap::new();
        env.insert("TOKEN".to_string(), "env-secret-value-9".to_string());
        let cfg = ServerConfig {
            command: Some("x".into()),
            args: vec![],
            env,
            url: None,
            headers,
            enabled: true,
            include: vec![],
            exclude: vec![],
            auth: None,
            oauth: None,
        };
        let tok = crate::agent::mcp_oauth::TokenSet {
            access_token: "access-tok-secret".into(),
            refresh_token: Some("refresh-tok-secret".into()),
            expires_at: None,
            token_endpoint: "https://h/token".into(),
            client_id: "cid".into(),
            client_secret: Some("client-sec-secret".into()),
            scope: None,
            resource: None,
        };
        let secrets = server_secrets(&cfg, Some(&tok));
        // Whole header value AND the bare token part (scheme stripped) are both listed.
        assert!(secrets.iter().any(|s| s == "Bearer header-tok-abcdef"));
        assert!(
            secrets.iter().any(|s| s == "header-tok-abcdef"),
            "bare bearer token must be masked too"
        );
        assert!(secrets.iter().any(|s| s == "xapikey-value-123"));
        assert!(secrets.iter().any(|s| s == "env-secret-value-9"));
        assert!(secrets.iter().any(|s| s == "access-tok-secret"));
        assert!(secrets.iter().any(|s| s == "refresh-tok-secret"));
        assert!(secrets.iter().any(|s| s == "client-sec-secret"));
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
