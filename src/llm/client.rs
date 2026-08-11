//! OpenAI-compatible streaming chat client — the "call API like hermes" layer.
//! POST {base_url}/chat/completions with Bearer auth, parse the SSE stream, emit content deltas.

use anyhow::{anyhow, bail, Context, Result};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::Deserialize;

use crate::core::types::{
    CacheControl, ChatChunk, ChatRequest, ChatResponse, FunctionCall, Message, StreamOptions,
    ToolCall, ToolCallDelta, ToolDef, Usage,
};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-global REAL token accounting. `client` is the single choke point every model call goes
/// through (REPL, sub-agents, compaction, skill/persona learning), so accumulating here gives an
/// honest session cost without threading `usage` through the agent loop. Only populated when the
/// provider actually reports `usage` (OpenAI-compatible endpoints with `include_usage`); when it
/// never does, `calls_with_usage` stays 0 and callers fall back to the chars/4 estimate.
#[derive(Default)]
pub struct CostMeter {
    prompt_tokens: AtomicU64,
    completion_tokens: AtomicU64,
    /// Model calls that returned a usage object (so the UI knows the numbers are real, not estimated).
    calls_with_usage: AtomicU64,
    /// Prompt-cache input tokens read back this session (the prompt_cache breakpoint payoff). 0 ⇒
    /// caching off / unsupported by the provider — the `/cost` cache line only shows when > 0.
    cache_read_tokens: AtomicU64,
    /// The MOST RECENT usage-carrying call (prompt / cached / completion) — the live cache-hit-rate
    /// signal for the status line. Session totals above answer "what did this cost"; these answer
    /// "is the prompt cache warm RIGHT NOW".
    last_prompt_tokens: AtomicU64,
    last_cached_tokens: AtomicU64,
    last_completion_tokens: AtomicU64,
}

static COST_METER: CostMeter = CostMeter {
    prompt_tokens: AtomicU64::new(0),
    completion_tokens: AtomicU64::new(0),
    calls_with_usage: AtomicU64::new(0),
    cache_read_tokens: AtomicU64::new(0),
    last_prompt_tokens: AtomicU64::new(0),
    last_cached_tokens: AtomicU64::new(0),
    last_completion_tokens: AtomicU64::new(0),
};

/// The process-global cost meter (real provider-reported tokens this session).
pub fn cost_meter() -> &'static CostMeter {
    &COST_METER
}

/// Does this model name route to an Anthropic model (which honors `cache_control` and rejects
/// `temperature`)? Drives AUTO prompt-cache so non-Anthropic providers get a field-free wire.
///
/// `claude`/`anthropic` match anywhere (always first-party). The bare gateway ids the product uses
/// (`opus-4-8`, `sonnet-4-6`, `haiku-4-5`, `fable-5` — all also exist with a `claude-` prefix) are
/// matched as WHOLE TOKENS so a community model that merely embeds the word (e.g. `fable13b`,
/// `haikuwriter`, the Llama `mythos` merge) is NOT misclassified into sending `cache_control` that
/// many OpenAI-compatible gateways reject with a 400.
pub fn is_anthropic_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    if m.contains("claude") || m.contains("anthropic") {
        return true;
    }
    ["opus", "sonnet", "haiku", "fable"]
        .iter()
        .any(|k| contains_word(&m, k))
}

/// `haystack` contains `needle` delimited by non-alphanumeric boundaries (so `-`, `_`, `.`, space
/// count as separators but an adjacent letter/digit does not). ASCII-only (model ids are ASCII).
/// Shared with `agent::prompt_tier_for` (the strict-tier model heuristic).
pub(crate) fn contains_word(haystack: &str, needle: &str) -> bool {
    let bytes = haystack.as_bytes();
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let i = start + pos;
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let after = i + needle.len();
        let after_ok = after >= bytes.len() || !bytes[after].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        start = i + 1;
    }
    false
}

/// Whether to send prompt-cache breakpoints: explicit config wins; `None` ⇒ AUTO (on only for
/// Anthropic-looking models). Off ⇒ the request is byte-identical to before (zero risk).
pub fn cache_enabled(flag: Option<bool>, model: &str) -> bool {
    flag.unwrap_or_else(|| is_anthropic_model(model))
}

/// Stamp ephemeral cache breakpoints (≤4 of Anthropic's 4): the LAST tool def (caches the whole
/// tool block), the first stable system message, an optional second dynamic system message, and the
/// last stable assistant/tool message before the newest turn. The volatile newest user turn stays
/// uncached. Computed per call, never stored, so mid-history `system` nudges don't disturb it.
fn apply_cache_breakpoints(messages: &mut [Message], tools: &mut [ToolDef]) {
    if let Some(last) = tools.last_mut() {
        last.cache_control = Some(CacheControl::ephemeral());
    }
    // First system message = stable lane.
    if let Some(first) = messages.first_mut() {
        if first.role == "system" {
            first.cache_control = Some(CacheControl::ephemeral());
        }
    }
    // Second leading system message = dynamic lane (persona/memory/skills/…); stamp when present.
    if messages.len() >= 2 && messages[0].role == "system" && messages[1].role == "system" {
        messages[1].cache_control = Some(CacheControl::ephemeral());
    }
    let n = messages.len();
    if n >= 3 {
        if let Some(idx) = (0..n)
            .rev()
            .find(|&i| matches!(messages[i].role.as_str(), "assistant" | "tool"))
        {
            if idx != 0 && !(idx == 1 && messages[0].role == "system") {
                messages[idx].cache_control = Some(CacheControl::ephemeral());
            }
        }
    }
}

impl CostMeter {
    fn record(&self, u: &Usage) {
        // Only count a call as "real" if it carried at least one token field.
        if u.prompt_tokens.is_none() && u.completion_tokens.is_none() && u.total_tokens.is_none() {
            return;
        }
        self.prompt_tokens
            .fetch_add(u.prompt_tokens.unwrap_or(0), Ordering::Relaxed);
        self.completion_tokens
            .fetch_add(u.completion_tokens.unwrap_or(0), Ordering::Relaxed);
        self.cache_read_tokens
            .fetch_add(u.cache_read(), Ordering::Relaxed);
        self.calls_with_usage.fetch_add(1, Ordering::Relaxed);
        self.last_prompt_tokens
            .store(u.prompt_tokens.unwrap_or(0), Ordering::Relaxed);
        self.last_cached_tokens
            .store(u.cache_read(), Ordering::Relaxed);
        self.last_completion_tokens
            .store(u.completion_tokens.unwrap_or(0), Ordering::Relaxed);
    }

    /// `(prompt, cached, completion)` of the most recent usage-carrying call; `None` before any.
    pub fn last_call(&self) -> Option<(u64, u64, u64)> {
        if self.calls_with_usage.load(Ordering::Relaxed) == 0 {
            return None;
        }
        Some((
            self.last_prompt_tokens.load(Ordering::Relaxed),
            self.last_cached_tokens.load(Ordering::Relaxed),
            self.last_completion_tokens.load(Ordering::Relaxed),
        ))
    }
    /// `(prompt, completion, calls_with_usage)`. `calls == 0` ⇒ no real usage seen (use the estimate).
    pub fn snapshot(&self) -> (u64, u64, u64) {
        (
            self.prompt_tokens.load(Ordering::Relaxed),
            self.completion_tokens.load(Ordering::Relaxed),
            self.calls_with_usage.load(Ordering::Relaxed),
        )
    }
    /// Prompt-cache input tokens read back this session (0 ⇒ caching off / unsupported).
    pub fn cache_read(&self) -> u64 {
        self.cache_read_tokens.load(Ordering::Relaxed)
    }
    /// Reset on `/clear` (a fresh conversation starts a fresh cost tally).
    pub fn reset(&self) {
        self.prompt_tokens.store(0, Ordering::Relaxed);
        self.completion_tokens.store(0, Ordering::Relaxed);
        self.calls_with_usage.store(0, Ordering::Relaxed);
        self.cache_read_tokens.store(0, Ordering::Relaxed);
        self.last_prompt_tokens.store(0, Ordering::Relaxed);
        self.last_cached_tokens.store(0, Ordering::Relaxed);
        self.last_completion_tokens.store(0, Ordering::Relaxed);
    }
}

/// Fetch the provider's advertised model ids (`GET {base}/models`). Used by `aizen models` and to
/// validate a freshly-set key. Returns the ids in the order the provider lists them.
/// One model row from `GET {base}/models`, plus the context window if the provider advertises it.
/// The bare OpenAI schema is just `id`; richer gateways (OpenRouter, LiteLLM) add a context length
/// under one of several field names — we read them all so the HUD can show a real number, not a guess.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub id: String,
    /// Context window in tokens, if the provider reported it. `None` ⇒ fall back to a name heuristic.
    pub context_length: Option<usize>,
}

/// Does `base_url` point at Anthropic's own API host?
///
/// Anthropic exposes an OpenAI-compatible surface at `https://api.anthropic.com/v1/`, so the chat
/// path needs nothing special — `authorization: Bearer` is supported there. But `GET /v1/models` is
/// the NATIVE endpoint and authenticates with `x-api-key` + `anthropic-version`; a Bearer-only
/// request gets a 401 that looks exactly like a wrong key. Since the model list is what config
/// validation uses to prove a key works, that false 401 would make a perfectly good Anthropic key
/// unusable in setup.
///
/// Matched on host, not on a substring of the whole URL, so a proxy whose PATH mentions anthropic
/// (`https://gw.example.com/anthropic/v1`) is not misread as first-party.
pub fn is_anthropic_endpoint(base_url: &str) -> bool {
    let after_scheme = base_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(base_url);
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split('@') // strip any userinfo
        .next_back()
        .unwrap_or("")
        .split(':') // strip the port
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    host == "api.anthropic.com" || host.ends_with(".api.anthropic.com")
}

/// Attach auth for `base_url`: always Bearer (what every OpenAI-compatible gateway wants), plus
/// Anthropic's native pair when the host is theirs. Sending both is safe — each side ignores the
/// header it doesn't use — and it means one code path serves both wire dialects.
fn with_provider_auth(
    rb: reqwest::RequestBuilder,
    base_url: &str,
    api_key: &str,
) -> reqwest::RequestBuilder {
    let rb = rb.bearer_auth(api_key);
    if is_anthropic_endpoint(base_url) {
        rb.header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
    } else {
        rb
    }
}

/// What a one-shot endpoint check found. The point of the split is that the three failures need
/// DIFFERENT fixes from the user, and a single "it didn't work" string can't say which: a bad host is
/// a base-URL problem, a 401 is a key problem, and a 404 usually means the `/v1` suffix is missing.
#[derive(Debug)]
pub enum EndpointCheck {
    /// Reachable and authenticated. Carries the model list so a caller can offer it immediately.
    Ok(Vec<ModelInfo>),
    /// Reachable, but the credential was rejected (401/403) — re-ask for the key, not the URL.
    Auth(String),
    /// Reachable, but no model list at this path (404/405) — almost always a missing `/v1`.
    NotFound(String),
    /// No HTTP response at all: DNS, TLS, connection refused, timeout. The URL itself is suspect.
    Unreachable(String),
    /// Any other HTTP status. Reported verbatim rather than guessed at.
    Http(u16, String),
}

/// One-shot `GET {base}/models`, classified — the validation behind interactive setup.
///
/// Deliberately NOT `send_with_retry`: this runs while a human waits, and retrying a 401 three times
/// with backoff just makes a wrong key take eight seconds to report. Pass a short-timeout client.
/// `api_key` is optional so a base URL can be checked before a key exists — an endpoint that answers
/// 401 without credentials has still proven it is reachable and speaks the right protocol.
///
/// When a key IS supplied, a 200 from `/models` is not accepted as proof on its own: many gateways
/// serve their model list unauthenticated, so the key is additionally checked against the endpoint
/// that actually matters — see [`chat_auth_rejection`].
pub async fn check_endpoint(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
) -> EndpointCheck {
    check_endpoint_with_deadline(client, base_url, api_key, CHAT_AUTH_PROBE_TIMEOUT).await
}

/// [`check_endpoint`] with the key-probe deadline injected, so a test can assert the timeout path
/// without holding a real connection open for the production 20s.
async fn check_endpoint_with_deadline(
    client: &reqwest::Client,
    base_url: &str,
    api_key: Option<&str>,
    probe_deadline: std::time::Duration,
) -> EndpointCheck {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let mut rb = client.get(&url);
    if let Some(k) = api_key {
        rb = with_provider_auth(rb, base_url, k);
    }
    let resp = match rb.send().await {
        Ok(r) => r,
        // Strip the reqwest chain down to its root: the outer layers repeat the URL the user just
        // typed, and the cause ("dns error", "connection refused") is the actionable part.
        Err(e) => {
            let root = std::error::Error::source(&e)
                .map(|s| s.to_string())
                .unwrap_or_else(|| e.to_string());
            return EndpointCheck::Unreachable(root);
        }
    };
    let status = resp.status();
    let code = status.as_u16();
    if status.is_success() {
        let infos = match parse_models_body(resp).await {
            Ok(infos) => infos,
            // A 200 that isn't a model list means this path isn't a models endpoint, even though
            // something answered — same user-facing fix as a 404.
            Err(e) => return EndpointCheck::NotFound(e.to_string()),
        };
        // A 200 here does NOT prove the key: plenty of gateways (kizeai among them) serve `/models`
        // to anyone, so a corrupted key sails through validation and only fails on the first real
        // turn. When we have a key, confirm it on the endpoint that enforces auth.
        if let Some(k) = api_key {
            let probe_model = infos
                .first()
                .map(|m| m.id.as_str())
                .unwrap_or("gpt-4o-mini");
            if let Some(detail) =
                chat_auth_rejection(client, base_url, k, probe_model, probe_deadline).await
            {
                return EndpointCheck::Auth(detail);
            }
        }
        return EndpointCheck::Ok(infos);
    }
    let detail = snippet_of(resp.text().await.unwrap_or_default());
    match code {
        401 | 403 => EndpointCheck::Auth(detail),
        404 | 405 => EndpointCheck::NotFound(detail),
        _ => EndpointCheck::Http(code, detail),
    }
}

/// Deadline for the key-confirmation probe below. A rejection is a single fast round-trip (an
/// upstream 401 needs no model work), so this only has to cover network latency — not generation.
const CHAT_AUTH_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Does `{base}/chat/completions` reject this key? `Some(detail)` on a 401/403, `None` otherwise.
///
/// The point is to catch a bad credential that `GET /models` waved through, so ONLY an explicit auth
/// rejection counts. Every other outcome — 400 for a model this account can't use, 404 on an odd
/// path, a timeout, a transport error — returns `None`, because none of them prove the key is wrong
/// and a false "bad key" here would send the user off to re-paste a credential that was fine.
///
/// Kept as cheap as possible: one token, and the reply is discarded unread.
///
/// Independently deadlined. Callers pass the shared client, whose read timeout is sized for a full
/// streamed turn (300s) — far too long to hold up interactive setup if a gateway stalls. A key that
/// is genuinely bad answers 401 immediately; anything slower is not worth waiting on, since a
/// timeout here means "unproven", not "bad".
async fn chat_auth_rejection(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    deadline: std::time::Duration,
) -> Option<String> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 1,
        "messages": [{ "role": "user", "content": "." }],
    });
    let send = with_provider_auth(client.post(&url), base_url, api_key)
        .json(&body)
        .send();
    let resp = tokio::time::timeout(deadline, send)
        .await
        .ok()? // deadline hit ⇒ unproven
        .ok()?; // transport error ⇒ unproven
    match resp.status().as_u16() {
        401 | 403 => Some(snippet_of(resp.text().await.unwrap_or_default())),
        _ => None,
    }
}

/// First line, ≤200 chars — provider error bodies are often a full HTML page or a deep JSON blob,
/// and neither belongs in a prompt the user is reading between keystrokes.
fn snippet_of(body: String) -> String {
    let first = body.trim().lines().next().unwrap_or("").trim().to_string();
    if first.chars().count() > 200 {
        let cut: String = first.chars().take(200).collect();
        format!("{cut}…")
    } else {
        first
    }
}

/// Lightweight reachability probe for the TUI health chip: a single `GET {base}/models` with **no**
/// retry/backoff (unlike [`fetch_models_info`]). Callers should pass a short-timeout client so a dead
/// endpoint fails fast. Success = 2xx (body is discarded). Failures surface as the same
/// `upstream returned HTTP {status}: …` / transport error shapes [`classify_api_error`] already knows.
pub async fn probe_models(client: &reqwest::Client, base_url: &str, api_key: &str) -> Result<()> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = with_provider_auth(client.get(&url), base_url, api_key)
        .send()
        .await
        .context("health probe request failed")?;
    let status = resp.status();
    if status.is_success() {
        // Drain the body so the connection can be reused; ignore parse — status alone is the signal.
        let _ = resp.bytes().await;
        return Ok(());
    }
    let detail = resp.text().await.unwrap_or_default();
    bail!("upstream returned HTTP {status}: {detail}");
}

/// Fetch the provider's model list with context windows when available (see [`ModelInfo`]).
pub async fn fetch_models_info(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>> {
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = send_with_retry(|| with_provider_auth(client.get(&url), base_url, api_key)).await?;
    parse_models_body(resp).await
}

/// Parse a `/models` response body into [`ModelInfo`]s. Split out of [`fetch_models_info`] so the
/// interactive [`check_endpoint`] can reuse the exact same schema tolerance instead of re-deriving
/// it — a validator that accepts a narrower set of providers than the real fetch would reject
/// endpoints that actually work.
async fn parse_models_body(resp: reqwest::Response) -> Result<Vec<ModelInfo>> {
    #[derive(Deserialize)]
    struct ModelsResp {
        #[serde(default)]
        data: Vec<ModelEntry>,
    }
    #[derive(Deserialize)]
    struct TopProvider {
        #[serde(default)]
        context_length: Option<usize>,
    }
    #[derive(Deserialize)]
    struct ModelEntry {
        id: String,
        // OpenRouter: `context_length`; others: `context_window` / `max_context_length`.
        #[serde(default)]
        context_length: Option<usize>,
        #[serde(default)]
        context_window: Option<usize>,
        #[serde(default)]
        max_context_length: Option<usize>,
        // Anthropic's native `/v1/models` names it `max_input_tokens`. Without this the Claude models
        // would all fall back to the name heuristic even though the provider stated the real number.
        #[serde(default)]
        max_input_tokens: Option<usize>,
        // OpenRouter nests it here too: `top_provider.context_length`.
        #[serde(default)]
        top_provider: Option<TopProvider>,
    }

    let parsed: ModelsResp = resp.json().await.context("parsing models response")?;
    Ok(parsed
        .data
        .into_iter()
        .map(|m| {
            let ctx = m
                .context_length
                .or(m.context_window)
                .or(m.max_context_length)
                .or(m.max_input_tokens)
                .or_else(|| m.top_provider.and_then(|t| t.context_length))
                .filter(|&n| n > 0);
            ModelInfo {
                id: m.id,
                context_length: ctx,
            }
        })
        .collect())
}

/// Transient upstream statuses worth a retry (rate-limit + gateway/server errors). A permanent 4xx
/// (auth, bad request) is NOT here — retrying it just wastes a round-trip and money.
fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// Exponential backoff CEILING (ms): `base * 2^attempt`, capped, floored at 1. Pure + monotonic so
/// it can be unit-tested; the live delay adds jitter under this ceiling (see `backoff_ms`).
fn backoff_ceiling_ms(attempt: u32, base_ms: u64, cap_ms: u64) -> u64 {
    base_ms
        .saturating_mul(1u64 << attempt.min(20))
        .min(cap_ms)
        .max(1)
}

/// A small random u64 from the OS CSPRNG (already a dep) — for backoff jitter only.
fn rand_u64() -> u64 {
    let mut b = [0u8; 8];
    let _ = getrandom::getrandom(&mut b);
    u64::from_le_bytes(b)
}

/// Longest SILENCE tolerated inside a live SSE stream before we declare it dead.
///
/// This is the fix for a confirmed hang: `send_with_retry` protects the request, but once the
/// provider answers 200 the `while let Some(event) = stream.next().await` loop had NO deadline at
/// all. A gateway that accepts the request and then stops writing (dropped upstream socket, a
/// load-balancer holding the connection open, a rate-limit stall) parks that loop forever — the
/// turn never fails, never returns, and the only way out is Esc + asking the model to continue.
/// reqwest's `read_timeout` is the backstop at 300s, which is far past the point where a person
/// concludes the tool is broken.
const STREAM_STALL_SECS: u64 = 90;

/// Env override for the stall deadline, clamped to a sane band. A reasoning model can legitimately
/// think for a long time before its first token, so the floor stays generous.
const STREAM_STALL_ENV: &str = "AIZEN_STREAM_STALL_SECS";

/// Resolve the inter-event stall deadline: env override (clamped 15s..=1800s) or the default.
fn stream_stall_timeout() -> std::time::Duration {
    let secs = std::env::var(STREAM_STALL_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map(|v| v.clamp(15, 1800))
        .unwrap_or(STREAM_STALL_SECS);
    std::time::Duration::from_secs(secs)
}

/// How many times a stream that died BEFORE producing anything is replayed. Bounded and only ever
/// on the blank case — see `stream_chat_with_tools_eager`.
const STREAM_BLANK_RETRIES: u32 = 2;

/// How many unparseable-frame warnings one stream may print before they collapse into a single
/// count — and only ever under [`FRAME_DEBUG_ENV`], since the normal path prints none at all.
///
/// The failure this bounds is per-TOKEN, not occasional: when our model of a provider's delta shape
/// is wrong, EVERY frame fails to parse, so an uncapped warn prints one line per streamed token and
/// buries the session under hundreds of near-identical lines. A handful names the problem; the rest
/// only repeat it.
const MAX_FRAME_WARNS: usize = 3;

/// Longest frame payload echoed in a warn line. A full SSE frame is one long JSON line that wraps
/// across the whole terminal; the head is where the offending key is, and the tail teaches nothing.
const FRAME_WARN_MAX: usize = 160;

/// Opt-in switch for stream-frame diagnostics. Unset (the default) means a frame we cannot read is
/// handled silently: see [`parse_chunk`] for why the remaining cases carry nothing worth a line.
const FRAME_DEBUG_ENV: &str = "AIZEN_DEBUG_STREAM";

/// Whether to narrate frames that survived neither parse attempt.
fn frame_debug() -> bool {
    std::env::var(FRAME_DEBUG_ENV)
        .ok()
        .map(|v| {
            let v = v.trim().to_ascii_lowercase();
            !v.is_empty() && v != "0" && v != "false" && v != "off" && v != "no"
        })
        .unwrap_or(false)
}

/// Shorten a frame payload for a warn line, cutting on a char boundary (frames carry UTF-8 text).
fn truncate_frame(data: &str) -> String {
    let data = data.trim();
    if data.chars().count() <= FRAME_WARN_MAX {
        return data.to_string();
    }
    let head: String = data.chars().take(FRAME_WARN_MAX).collect();
    format!("{head}… ({} bytes total)", data.len())
}

/// Deserialize one SSE data frame into a [`ChatChunk`], strictly first and then LENIENTLY.
///
/// The lenient retry is the whole point. Deriving `Deserialize` on a struct makes serde reject an
/// object outright on things that are not actually ambiguous on the wire — most importantly a
/// DUPLICATE KEY, which OpenRouter-shaped gateways produce every time they mirror one value into two
/// spellings of the same channel. A rejected frame is not just a warning: whatever `content` or
/// `tool_calls` rode in that delta is dropped with it, and a lost tool-call fragment is a step the
/// agent never takes.
///
/// Routing through [`serde_json::Value`] fixes that class generically, because its object map takes
/// duplicate keys last-wins instead of erroring — so the retry succeeds without us having to predict
/// which field a given gateway will double up next. It costs one extra allocation on a path that,
/// when everything is well-shaped, is never taken.
///
/// What survives BOTH attempts is therefore not a chunk at all: keepalive/comment/`{"type":"ping"}`
/// noise and other non-completion frames gateways interleave. Those carry nothing this loop wants,
/// which is why the caller drops them silently unless [`FRAME_DEBUG_ENV`] is set.
fn parse_chunk(data: &str) -> Result<ChatChunk, serde_json::Error> {
    match serde_json::from_str::<ChatChunk>(data) {
        Ok(chunk) => Ok(chunk),
        Err(strict) => match serde_json::from_str::<serde_json::Value>(data) {
            // Report the STRICT error on failure, not the lenient one: it names the offending key,
            // while the second pass only says the value had the wrong shape.
            Ok(value) => serde_json::from_value::<ChatChunk>(value).map_err(|_| strict),
            Err(_) => Err(strict),
        },
    }
}

/// Tell the user the stream stalled and is being replayed. Routed through the TUI funnel: a raw
/// `eprintln!` here would be painted over by the retained render thread.
fn stream_retry_note(reason: &str, attempt: u32, max: u32, delay_ms: u64) {
    let line = format!(
        "⟳ stream died before any output ({reason}) — retrying {attempt}/{max} in {delay_ms}ms"
    );
    if crate::ui::tui::active() {
        crate::ui::tui::emit_line(&crate::ui::theme::faint(line).to_string());
    } else {
        eprintln!("{line}");
    }
}

/// Full-jitter backoff: a uniform delay in `[ceil/2, ceil]`. Jitter spreads retries so a fleet of
/// callers doesn't thunder back in lock-step after a shared 503.
fn backoff_ms(attempt: u32, base_ms: u64, cap_ms: u64) -> u64 {
    let ceil = backoff_ceiling_ms(attempt, base_ms, cap_ms);
    let half = ceil / 2;
    half + (rand_u64() % (ceil - half + 1))
}

/// `Retry-After` as ms (integer-seconds form only), capped at 30s so a hostile header can't park us.
/// The HTTP-date form is ignored (falls back to exponential backoff).
fn retry_after_ms(resp: &reqwest::Response) -> Option<u64> {
    let v = resp
        .headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    let secs: u64 = v.trim().parse().ok()?;
    Some(secs.saturating_mul(1000).min(30_000))
}

/// Send a request with bounded retry + exponential backoff. `build` is re-invoked per attempt
/// because `.send()` consumes the `RequestBuilder`. Retries transport errors and transient upstream
/// statuses (429/5xx), honoring `Retry-After`; a permanent non-2xx (or exhausted retries) reads the
/// body and returns the SAME `upstream returned HTTP {status}: {detail}` error the call sites used
/// before this layer existed. A 2xx `Response` is returned untouched for the caller to parse/stream.
async fn send_with_retry<F>(build: F) -> Result<reqwest::Response>
where
    F: Fn() -> reqwest::RequestBuilder,
{
    const MAX_RETRIES: u32 = 3;
    const BASE_MS: u64 = 400;
    const CAP_MS: u64 = 8_000;
    let mut attempt: u32 = 0;
    loop {
        match build().send().await {
            Ok(resp) => {
                let status = resp.status();
                if status.is_success() {
                    return Ok(resp);
                }
                if is_retryable_status(status.as_u16()) && attempt < MAX_RETRIES {
                    let delay = retry_after_ms(&resp)
                        .unwrap_or_else(|| backoff_ms(attempt, BASE_MS, CAP_MS));
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    continue;
                }
                let detail = resp.text().await.unwrap_or_default();
                bail!("upstream returned HTTP {status}: {detail}");
            }
            Err(e) => {
                if attempt < MAX_RETRIES {
                    let delay = backoff_ms(attempt, BASE_MS, CAP_MS);
                    attempt += 1;
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    continue;
                }
                return Err(anyhow!(e)).context("request failed after retries");
            }
        }
    }
}

/// Classification of a `chat()` error for GOAL MODE's smart retry (see the goal loop in
/// `src/agent/mod.rs`). Goal mode keeps working through API flakiness, but must distinguish a
/// transient upstream hiccup (retry indefinitely with backoff) from a permanent client error (bad
/// key/request/endpoint — retry only a few times then give up, since hammering it just burns calls).
/// Ordinary (non-goal) turns never call this — their errors stay fatal after the HTTP client's own
/// bounded retries inside `send_with_retry`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErrorKind {
    /// 429 / 5xx / transport / timeout / mid-stream drop — safe to retry indefinitely with backoff.
    Transient,
    /// 400 / 401 / 403 / 404 — a client/config error retrying can't fix. Goal mode retries a small
    /// bounded number of times (the provider may be briefly misbehaving) then stops with the error.
    Permanent,
}

/// Classify a `chat()` error for goal-mode retry. `send_with_retry` surfaces upstream failures as
/// `upstream returned HTTP {status}: …` (where `{status}` renders like `400 Bad Request`) and
/// transport failures as `request failed after retries`; the streaming path adds `SSE stream error:
/// …`. We match the permanent 4xx shapes and treat everything else as Transient — goal mode's whole
/// point is surviving a flaky API, so the default leans toward retrying, not giving up.
pub fn classify_api_error(e: &anyhow::Error) -> ApiErrorKind {
    let msg = e.to_string();
    for code in ["HTTP 400", "HTTP 401", "HTTP 403", "HTTP 404"] {
        if msg.contains(code) {
            return ApiErrorKind::Permanent;
        }
    }
    ApiErrorKind::Transient
}

/// Full-jitter backoff (ms) for GOAL MODE's loop-level retry, indexed by attempt. Reuses the exact
/// full-jitter formula as the HTTP client's own `send_with_retry` (spreads retries, no thundering
/// herd) but with a higher ceiling: goal mode may retry a flaky endpoint many times over a long run,
/// so the per-attempt wait grows to a ~30s plateau instead of the client's 8s (which is tuned for 3
/// quick tries inside a single call).
pub fn goal_backoff_ms(attempt: u32) -> u64 {
    const BASE_MS: u64 = 500;
    const CAP_MS: u64 = 30_000;
    backoff_ms(attempt, BASE_MS, CAP_MS)
}

/// Full-jitter backoff (ms) for an INTERACTIVE turn's loop-level retry — a person is sitting there
/// watching. Base 300ms, cap 4s: the whole ~10-attempt chain lands around ~30–40s total (enough for
/// a gateway to recover from a 429/5xx blip), while no SINGLE wait ever reaches the 30s that
/// `goal_backoff_ms` allows — a lone 30s pause reads as a hang, which is exactly the feel this avoids.
/// `goal_backoff_ms` stays higher-ceilinged for `/goal`, which runs long with nobody watching.
pub fn interactive_backoff_ms(attempt: u32) -> u64 {
    const BASE_MS: u64 = 300;
    const CAP_MS: u64 = 4_000;
    backoff_ms(attempt, BASE_MS, CAP_MS)
}

/// Models that rejected `reasoning_effort` with a 400 THIS SESSION. Populated reactively — never
/// guessed from the model name (a name heuristic would mis-serve every gateway that renames models).
/// The first time a provider 400s on the field we record the model here, so every later turn strips
/// it up front: at most ONE failed call per model per session, not one per turn.
static EFFORT_UNSUPPORTED: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashSet<String>>,
> = std::sync::OnceLock::new();

fn effort_unsupported_set() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    EFFORT_UNSUPPORTED.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

/// Has `model` already rejected `reasoning_effort` this session? (Then we send the field-free wire.)
fn effort_known_unsupported(model: &str) -> bool {
    effort_unsupported_set()
        .lock()
        .map(|s| s.contains(model))
        .unwrap_or(false)
}

fn mark_effort_unsupported(model: &str) {
    if let Ok(mut s) = effort_unsupported_set().lock() {
        s.insert(model.to_string());
    }
}

/// Does this error detail blame `reasoning_effort`? Providers word it differently ("unknown parameter
/// reasoning_effort", "reasoning_effort: unsupported value", "does not support reasoning effort"), so
/// match the field name loosely (case-insensitive, with/without the underscore).
fn body_blames_effort(detail: &str) -> bool {
    let d = detail.to_ascii_lowercase();
    d.contains("reasoning_effort") || d.contains("reasoning effort")
}

/// POST a chat request with a WIRE-LEVEL effort fallback (the model-agnostic way to honour the
/// per-model effort ceiling): send the tier as configured; if the provider 400s SPECIFICALLY because
/// of `reasoning_effort` — a model that doesn't take the field at all, or doesn't accept the tier we
/// sent (e.g. `max` to an o-series that tops out at `high`) — strip the field, remember the model for
/// the rest of the session, and retry once. Any other error propagates unchanged. Models already
/// learned-unsupported get the field stripped up front, so the 400 costs one call per model, once.
async fn send_chat(
    client: &reqwest::Client,
    url: &str,
    api_key: &str,
    mut body: ChatRequest,
) -> Result<reqwest::Response> {
    if body.reasoning_effort.is_some() && effort_known_unsupported(&body.model) {
        body.reasoning_effort = None;
    }
    let had_effort = body.reasoning_effort.is_some();
    match send_with_retry(|| client.post(url).bearer_auth(api_key).json(&body)).await {
        Ok(r) => Ok(r),
        Err(e) => {
            let msg = e.to_string();
            // Only intervene on the exact "400 blames reasoning_effort" case, and only if we sent it.
            if had_effort && msg.contains("HTTP 400") && body_blames_effort(&msg) {
                mark_effort_unsupported(&body.model);
                body.reasoning_effort = None;
                let note = format!(
                    "reasoning_effort not accepted by {} — retrying without it (won't send it again this session)",
                    body.model
                );
                if crate::ui::tui::active() {
                    crate::ui::tui::emit_line(&crate::ui::theme::faint(note).to_string());
                } else {
                    eprintln!("{}", crate::ui::theme::faint(note));
                }
                send_with_retry(|| client.post(url).bearer_auth(api_key).json(&body)).await
            } else {
                Err(e)
            }
        }
    }
}

/// The outcome of one non-streaming tool-calling turn.
pub struct ChatTurn {
    /// Natural-language content (the final answer when `tool_calls` is empty).
    pub content: Option<String>,
    /// Tool calls the model wants executed. NON-EMPTY ⇒ tools path (the source of truth —
    /// do not branch on `finish_reason`).
    pub tool_calls: Vec<ToolCall>,
    /// Advisory only — the loop classifies by `tool_calls`, not this. Kept for diagnostics.
    #[allow(dead_code)]
    pub finish_reason: Option<String>,
    /// Provider-reported usage for THIS call, when sent (the final chunk in streaming). The loop's
    /// context guards prefer this real number over the chars/4 estimate.
    pub usage: Option<Usage>,
    /// EAGERLY-STARTED tool executions from the streaming path: `(position in tool_calls, handle)`.
    /// A read-only call whose arguments finished streaming may already be running before the
    /// response ends — the executor ADOPTS these instead of re-spawning. Discarding a `ChatTurn`
    /// (divergence, error) just detaches them; eager calls are read-only, so that is harmless.
    pub eager: Vec<(usize, tokio::task::JoinHandle<String>)>,
}

/// Hook offered each tool call the moment its streamed arguments complete: return a running
/// handle to START it early (read-only + prefix-safe only — the caller owns that policy), or
/// `None` to let the executor run it normally after the stream ends.
pub type EagerStartFn<'a> =
    &'a (dyn Fn(usize, &ToolCall) -> Option<tokio::task::JoinHandle<String>> + Send + Sync);

/// Stream a chat completion. Prints content deltas to stdout as they arrive and
/// returns the full concatenated assistant text. Returns a typed error on a non-2xx
/// response (so the caller can decide retry/stop) instead of panicking.
///
/// Superseded by the tool-aware streaming path: every live caller needs tool calls out of the same
/// stream, and printing straight to stdout would corrupt the retained frame anyway. Kept as the
/// plain-text reference shape of an SSE read.
#[allow(dead_code)]
pub async fn stream_chat(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: Vec<Message>,
) -> Result<String> {
    stream_chat_with_visual_contract(client, base_url, api_key, model, messages, false).await
}

/// Stream one top-level answer with optional visual-response guidance and terminal Markdown rendering.
pub async fn stream_chat_with_visual_contract(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    mut messages: Vec<Message>,
    visual_contract: bool,
) -> Result<String> {
    if visual_contract {
        if let Some(block) = crate::agent::response_visuals_prompt_block(
            crate::core::cli_config::load().response_visuals(),
        ) {
            messages.insert(0, Message::system(block));
        }
    }
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = ChatRequest {
        model: model.to_string(),
        messages,
        stream: true,
        temperature: None,
        max_tokens: crate::core::cli_config::load().max_tokens,
        tools: Vec::new(),
        tool_choice: None,
        parallel_tool_calls: None,
        stream_options: None,
        reasoning_effort: crate::core::cli_config::resolved_reasoning_effort(
            crate::core::cli_config::load().reasoning_effort,
        ),
    };

    let mut spin = Some(crate::ui::spinner::Spinner::start("thinking"));

    let resp = match send_chat(client, &url, api_key, body).await {
        Ok(r) => r,
        Err(e) => {
            spin.take();
            return Err(e);
        }
    };

    let mut stream = resp.bytes_stream().eventsource();
    let mut full = String::new();
    let decorate = std::io::IsTerminal::is_terminal(&std::io::stdout());
    let mut md = crate::ui::markdown::MarkdownStream::new(decorate, crate::ui::tui::width());

    // Capture a mid-stream transport error and break rather than `?`-ing out, so the closing
    // newline below still runs (a clean line break before the error surfaces) — same invariant
    // as `stream_chat_with_tools`.
    let mut stream_err: Option<anyhow::Error> = None;
    // Same stall watchdog as the tool-calling path: a provider that answers 200 and then goes quiet
    // would otherwise park this loop until reqwest's 300s read timeout — and only if it withholds
    // BYTES, which keepalive frames defeat entirely. No replay here: this one-shot surface has no
    // eager handles to detach and its caller already prints the error.
    let stall = stream_stall_timeout();
    // Rate-limit counter, same reason as the tool-calling path: a delta shape we mis-model breaks
    // EVERY frame, and one warn per token buries the reply.
    let mut bad_frames: usize = 0;
    loop {
        let event = match tokio::time::timeout(stall, stream.next()).await {
            Ok(Some(Ok(e))) => e,
            Ok(Some(Err(e))) => {
                stream_err = Some(anyhow!("SSE stream error: {e}"));
                break;
            }
            Ok(None) => break,
            Err(_) => {
                stream_err = Some(anyhow!(
                    "SSE stream error: timeout — no data for {}s",
                    stall.as_secs()
                ));
                break;
            }
        };

        // OpenAI signals end-of-stream with a literal `data: [DONE]`.
        if event.data.trim() == "[DONE]" {
            break;
        }
        if event.data.trim().is_empty() {
            continue;
        }

        match parse_chunk(&event.data) {
            Ok(chunk) => {
                if let Some(choice) = chunk.choices.first() {
                    if let Some(content) = &choice.delta.content {
                        spin.take(); // clear before the first token prints
                        let rendered = md.push(content);
                        if !rendered.is_empty() {
                            print!("{rendered}");
                            let _ = std::io::Write::flush(&mut std::io::stdout());
                        }
                        full.push_str(content);
                    }
                }
            }
            // Keepalive / ping / non-completion frames gateways interleave. Dropped SILENTLY: after
            // `parse_chunk`'s lenient retry, what lands here carries nothing this loop consumes, and a
            // line per frame is one line per token on a stream we merely fail to recognise.
            Err(e) => {
                bad_frames += 1;
                if frame_debug() && bad_frames <= MAX_FRAME_WARNS {
                    spin.take();
                    eprintln!(
                        "\n[warn] unparseable stream frame ({e}): {}",
                        truncate_frame(&event.data)
                    );
                }
            }
        }
    }

    spin.take();
    if frame_debug() && bad_frames > MAX_FRAME_WARNS {
        eprintln!(
            "[warn] {} more unparseable stream frames suppressed ({bad_frames} total this response)",
            bad_frames - MAX_FRAME_WARNS
        );
    }
    let closing = md.finish();
    if !closing.is_empty() {
        print!("{closing}");
    }
    println!();
    if let Some(e) = stream_err {
        return Err(e);
    }
    Ok(full)
}

/// One non-streaming chat turn WITH tools advertised. Returns the assistant's content
/// and/or the tool calls it wants executed. Used by the `task` sub-agent (which runs silently
/// and returns only its final text) and the workflow fan-out; the streaming counterpart
/// (`stream_chat_with_tools`) drives the top-level `aizen agent` for live output. Both return
/// `ChatTurn`, so the loop is agnostic.
pub async fn chat_with_tools(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
) -> Result<ChatTurn> {
    let effort = crate::core::cli_config::resolved_reasoning_effort(
        crate::core::cli_config::load().reasoning_effort,
    );
    chat_with_tools_effort(client, base_url, api_key, model, messages, tools, effort).await
}

/// `chat_with_tools` with the `reasoning_effort` decided by the CALLER rather than read from the
/// process-global override.
///
/// Concurrent `serve` lanes each carry their own effort tier: reading the global here would stamp
/// whichever lane armed it last onto every in-flight request. The two-level `Option` matches
/// [`crate::core::cli_config::resolved_reasoning_effort`] — `None` omits the field entirely, keeping
/// the request byte-identical for providers that reject it.
#[allow(clippy::option_option, clippy::too_many_arguments)]
pub async fn chat_with_tools_effort(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    effort: Option<String>,
) -> Result<ChatTurn> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let cfg = crate::core::cli_config::load();
    let mut msgs = messages.to_vec();
    let mut tool_defs = tools.to_vec();
    // Stamp on cache_enabled alone — the system/history breakpoints pay off even with no tools
    // (summaries, persona/skill learning, sub-tasks); the last-tool-def breakpoint inside is a
    // no-op when tools is empty.
    if cache_enabled(cfg.prompt_cache, model) {
        apply_cache_breakpoints(&mut msgs, &mut tool_defs);
    }
    let body = ChatRequest {
        model: model.to_string(),
        messages: msgs,
        stream: false,
        temperature: None,
        max_tokens: cfg.max_tokens,
        tools: tool_defs,
        tool_choice: if tools.is_empty() {
            None
        } else {
            Some("auto".to_string())
        },
        parallel_tool_calls: if tools.is_empty() { None } else { Some(true) },
        stream_options: None, // non-streaming responses carry `usage` natively
        reasoning_effort: effort,
    };

    let resp = send_chat(client, &url, api_key, body).await?;

    let parsed: ChatResponse = resp
        .json()
        .await
        .context("parsing chat-completions response")?;
    if let Some(u) = &parsed.usage {
        cost_meter().record(u);
    }
    let usage = parsed.usage;
    let choice = parsed
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("response had no choices"))?;
    Ok(ChatTurn {
        content: resolve_content(&choice.message),
        tool_calls: choice.message.tool_calls,
        finish_reason: choice.finish_reason,
        usage,
        eager: Vec::new(),
    })
}

/// The message's text, falling back to its REASONING channel when `content` is empty.
///
/// Some providers put the whole reply in `reasoning_content` (DeepSeek) / `reasoning` (OpenRouter)
/// and leave `content` null. With no tool calls that deserializes to "no text, no calls" — which
/// every caller classifies as an empty-200 provider failure — so the loop retried an identical
/// request, got the identical "empty" answer, and burned its whole budget before reporting silence.
/// The model DID speak.
///
/// The asymmetry only bit SUB-AGENTS: [`Delta`] (streaming) has always read this field, and a
/// sub-agent is the one caller that runs exclusively on the non-streaming [`chat_with_tools`]. The
/// same model answering the same way over the streaming path was fine, which is why this looked like
/// "sub-agents get empty responses" rather than a provider-shape gap.
///
/// Narrow on purpose. The fallback applies ONLY when there are no tool calls AND `content` is
/// absent/blank, so a normal turn is untouched and reasoning is never APPENDED to real content —
/// appending would leak the chain-of-thought the CLI deliberately suppresses on the streaming path.
/// A blank `content` alongside tool calls stays blank: that is the canonical tool-call turn shape.
fn resolve_content(m: &crate::core::types::RespMessage) -> Option<String> {
    let has_text = m.content.as_deref().is_some_and(|s| !s.trim().is_empty());
    if has_text || !m.tool_calls.is_empty() {
        return m.content.clone();
    }
    m.reasoning_text()
        .map(str::to_string)
        .or_else(|| m.content.clone())
}

/// Reassembles streamed `delta.tool_calls[]` fragments into whole `ToolCall`s, keyed by index.
/// `id`/`name` land once (first fragment for an index); `arguments` pieces concatenate.
///
/// Robustness: spec-compliant providers tag every fragment with an `index`, but `ToolCallDelta.index`
/// is `#[serde(default)]` → a provider that OMITS index sends every fragment as index 0. Two distinct
/// parallel tool calls would then merge onto slot 0 (second id/name clobbers the first; the two
/// argument strings concatenate into one broken JSON blob). So when a fragment carries a non-empty
/// `id`, we route by id (via `id_index`) and allocate a fresh slot if index 0 is already claimed by a
/// different id — keeping the common compliant case (id+index first, then index-only arg fragments)
/// unchanged.
#[derive(Default)]
pub struct ToolCallAccumulator {
    calls: BTreeMap<usize, AccCall>,
    /// Maps a seen tool-call `id` → the slot key it owns (for index-omitting providers).
    id_index: std::collections::HashMap<String, usize>,
    /// The slot the CURRENT fragment stream is filling — a fragment landing on a different slot
    /// means the previous call's arguments are complete (spec: one call streams contiguously).
    active: Option<usize>,
}

#[derive(Default)]
struct AccCall {
    id: String,
    name: String,
    args: String,
}

/// Are these streamed `arguments` whole enough to RUN — i.e. do they parse as a JSON object?
///
/// The empty string is deliberately NOT complete. A genuinely argument-less call still streams
/// `"{}"`; `""` only ever means "the arguments haven't arrived yet". Treating it as complete is what
/// let a truncated fragment reach a tool as `{}` (see [`ToolCallAccumulator::snapshot`]).
fn args_complete(raw: &str) -> bool {
    let t = raw.trim();
    !t.is_empty()
        && serde_json::from_str::<serde_json::Value>(t)
            .map(|v| v.is_object())
            .unwrap_or(false)
}

impl ToolCallAccumulator {
    /// Merge a batch of fragments. Returns the calls whose arguments COMPLETED because a later
    /// slot started (the eager-execution trigger). The FINAL call only completes at stream end —
    /// `finish`/`finish_indexed` covers it.
    pub fn ingest(&mut self, deltas: &[ToolCallDelta]) -> Vec<(usize, ToolCall)> {
        let mut completed = Vec::new();
        for d in deltas {
            // Resolve which slot this fragment belongs to. Default: key by `index`. But a non-empty
            // `id` takes precedence — if we've seen the id, reuse its slot; if it's new and `d.index`
            // is already held by a DIFFERENT id (the index-omitting collision), allocate a fresh slot.
            let key = match d.id.as_deref().filter(|s| !s.is_empty()) {
                Some(id) => {
                    if let Some(&k) = self.id_index.get(id) {
                        k
                    } else {
                        let collides = self
                            .calls
                            .get(&d.index)
                            .map(|e| !e.id.is_empty() && e.id != id)
                            .unwrap_or(false);
                        let k = if collides {
                            self.calls.keys().last().map(|m| m + 1).unwrap_or(d.index)
                        } else {
                            d.index
                        };
                        self.id_index.insert(id.to_string(), k);
                        k
                    }
                }
                // An `arguments` fragment carries NO id (only the opening fragment does). If the
                // provider also omits `index`, `d.index` defaults to 0 — so the second call's
                // arguments would pour into slot 0 instead of the slot its id was rerouted to, both
                // corrupting slot 0's JSON and bouncing `active` (which snapshots a half-built
                // call). An id-less fragment belongs to the call currently streaming: the spec
                // guarantees one call streams contiguously. Kept narrow (`index == 0` and an active
                // slot that isn't 0) so compliant providers — where that fragment carries its real
                // `index: 1` — are routed exactly as before.
                None => match self.active {
                    Some(a) if d.index == 0 && a != 0 => a,
                    _ => d.index,
                },
            };
            if let Some(prev) = self.active {
                if prev != key {
                    if let Some(tc) = self.snapshot(prev) {
                        completed.push((prev, tc));
                    }
                }
            }
            self.active = Some(key);
            let e = self.calls.entry(key).or_default();
            if let Some(id) = &d.id {
                if !id.is_empty() {
                    e.id = id.clone();
                }
            }
            if let Some(f) = &d.function {
                if let Some(n) = &f.name {
                    if !n.is_empty() {
                        e.name = n.clone();
                    }
                }
                if let Some(a) = &f.arguments {
                    e.args.push_str(a);
                }
            }
        }
        completed
    }

    /// A clone of slot `key`'s call as accumulated so far (None when it has no name yet, or when its
    /// arguments are not yet a WHOLE JSON object). The id may still be empty here — eager adoption
    /// keys by POSITION, not id, so that's fine.
    ///
    /// The args-completeness check is what keeps a half-streamed call from being dispatched: the
    /// "a fragment landed on another slot ⇒ the previous call is done" rule is only true for
    /// providers that tag every fragment with an `index`. When one doesn't, an argument fragment
    /// (which carries no `id`) defaults to index 0 and can bounce the active slot, snapshotting a
    /// call whose `args` is still `""` or a partial `{"path":`. An empty string then reads as a
    /// valid `{}` in `parse_call_args`, so the tool would run with NO ARGUMENTS and fail with
    /// `missing required string arg` — while the transcript line, built later from the complete
    /// call, showed the argument right there. Refusing to snapshot costs only the eager head start
    /// (the call still runs normally in `execute_calls` with its full arguments).
    fn snapshot(&self, key: usize) -> Option<ToolCall> {
        let c = self.calls.get(&key)?;
        if c.name.is_empty() || !args_complete(&c.args) {
            return None;
        }
        Some(ToolCall {
            id: c.id.clone(),
            kind: "function".to_string(),
            function: FunctionCall {
                name: c.name.clone(),
                arguments: c.args.clone(),
            },
        })
    }

    /// Finalize, keeping each call's SLOT KEY alongside — the eager path maps slot → final
    /// position through this (positions are what the executor stitches by).
    ///
    /// Like [`snapshot`], we reject calls whose arguments are not a complete JSON object.  A stream
    /// that was cut in the middle of `arguments` leaves `args` as `""` or a partial fragment; both
    /// fail `args_complete`.  Dispatching them would produce a spurious `missing required arg` error
    /// (and a wasted round-trip) even though the model would have sent correct arguments had the
    /// stream finished.  We drop the truncated call instead — the executor will see it is missing
    /// from the results and the model retries with full context.
    pub fn finish_indexed(self) -> Vec<(usize, ToolCall)> {
        self.calls
            .into_iter()
            .filter(|(_, c)| !c.name.is_empty() && args_complete(&c.args))
            .enumerate()
            .map(|(i, (k, c))| {
                (
                    k,
                    ToolCall {
                        id: if c.id.is_empty() {
                            format!("call_{i}")
                        } else {
                            c.id
                        },
                        kind: "function".to_string(),
                        function: FunctionCall {
                            name: c.name,
                            arguments: c.args,
                        },
                    },
                )
            })
            .collect()
    }

    /// Slot-key-free finalize (the streaming path itself uses `finish_indexed` for eager-handle
    /// position mapping; this stays as the plain API + test surface).
    #[allow(dead_code)]
    pub fn finish(self) -> Vec<ToolCall> {
        self.finish_indexed()
            .into_iter()
            .map(|(_, tc)| tc)
            .collect()
    }
}

/// Streaming variant of `chat_with_tools`: prints assistant content deltas live as they
/// arrive AND reassembles any tool-call fragments, returning the same `ChatTurn` the loop
/// consumes. (The loop is unchanged — only the live UX differs.)
pub async fn stream_chat_with_tools(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
) -> Result<ChatTurn> {
    stream_chat_with_tools_eager(client, base_url, api_key, model, messages, tools, None).await
}

/// [`stream_chat_with_tools`] plus EAGER tool execution: each tool call is offered to `eager_hook`
/// the moment its streamed arguments complete, so read-only work overlaps the rest of the
/// generation (first-tool latency hides inside stream time). Handles come back on
/// `ChatTurn.eager` keyed by final POSITION; a mid-stream transport error drops them (detached —
/// eager calls are read-only by policy, so discarding is safe).
pub async fn stream_chat_with_tools_eager(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    eager_hook: Option<EagerStartFn<'_>>,
) -> Result<ChatTurn> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let cfg = crate::core::cli_config::load();
    let mut msgs = messages.to_vec();
    let mut tool_defs = tools.to_vec();
    // See chat_with_tools: stamp on cache_enabled alone (system/history breakpoints help no-tool calls too).
    if cache_enabled(cfg.prompt_cache, model) {
        apply_cache_breakpoints(&mut msgs, &mut tool_defs);
    }
    let body = ChatRequest {
        model: model.to_string(),
        messages: msgs,
        stream: true,
        temperature: None,
        max_tokens: cfg.max_tokens,
        tools: tool_defs,
        tool_choice: if tools.is_empty() {
            None
        } else {
            Some("auto".to_string())
        },
        parallel_tool_calls: if tools.is_empty() { None } else { Some(true) },
        stream_options: Some(StreamOptions {
            include_usage: true,
        }), // ask for a final usage chunk
        reasoning_effort: crate::core::cli_config::resolved_reasoning_effort(
            cfg.reasoning_effort.clone(),
        ),
    };

    // BLANK-STREAM REPLAY. The request layer (`send_with_retry`) only covers failures that happen
    // before the 200; a stream that is accepted and then dies or goes silent used to leave the turn
    // parked forever with nothing to show. Retry the whole call, but ONLY while the stream has
    // produced literally nothing (no text, no tool-call fragment, no usage): re-sending after
    // partial output would duplicate text in the transcript or resurrect a half-streamed tool call.
    // Once anything has arrived we fall through to the existing error path and let the caller decide.
    let mut blank_attempt: u32 = 0;
    loop {
        // Spinner during the "thinking" gap: from request send until the first token / tool delta
        // streams back. TTY-only (silent no-op on pipes/CI). Cleared before any output is printed.
        // Suppressed under the sticky TUI — its box shows the "⚡ working…" indicator instead, and a
        // carriage-return spinner would fight the pinned footer.
        let mut spin = if crate::ui::tui::active() {
            None
        } else {
            Some(crate::ui::spinner::Spinner::start("thinking"))
        };

        let resp = match send_chat(client, &url, api_key, body.clone()).await {
            Ok(r) => r,
            Err(e) => {
                spin.take(); // clear the spinner before surfacing the error
                return Err(e);
            }
        };

        let mut stream = resp.bytes_stream().eventsource();
        let mut full = String::new();
        let mut acc = ToolCallAccumulator::default();
        let mut finish_reason: Option<String> = None;
        let mut final_usage: Option<Usage> = None; // the final-chunk usage report, threaded to the loop
                                                   // Eager starts, keyed by accumulator SLOT until the final position mapping exists.
        let mut eager_by_slot: std::collections::HashMap<usize, tokio::task::JoinHandle<String>> =
            std::collections::HashMap::new();
        let mut think = ThinkFilter::default(); // suppress `<think>…</think>` reasoning from the display
                                                // Retained mode owns raw assistant blocks and reparses the whole active message; classic/one-shot
                                                // paths keep the line-buffered display renderer. History always keeps the same raw Markdown.
        let retained_display = crate::ui::tui::retained_active();
        let decorate = !retained_display
            && (crate::ui::tui::active() || std::io::IsTerminal::is_terminal(&std::io::stdout()));
        let cols = crate::ui::tui::width(); // wrap to the box width (not a separately-probed window edge)
        let mut md = crate::ui::markdown::MarkdownStream::new(decorate, cols);

        // A mid-stream transport error (timeout, gateway drop, truncated body) must NOT short-circuit
        // with `?` here: that would skip the `think.finish()` / `md.finish()` cleanup below, stranding
        // the terminal with a half-rendered line or an UNCLOSED code-fence box that corrupts every
        // subsequent turn. Capture the error, break, flush the display to a clean state, THEN propagate.
        let mut stream_err: Option<anyhow::Error> = None;
        // IDLE WATCHDOG. `reqwest`'s `read_timeout` fires only on a socket that delivers no BYTES; a
        // gateway that keeps the connection warm with comment/keepalive frames, or an upstream that
        // stalls after `role`, satisfies it forever — so the turn hung with no cap at all. Bound the gap
        // between USEFUL events instead, and re-arm the timer on every one.
        let idle_cap = stream_stall_timeout();
        // Has this stream produced anything a retry would duplicate? Text, a tool-call fragment, or a
        // usage report all count. Governs both the watchdog's error wording and blank-stream replay.
        let mut produced = false;
        // Unparseable frames seen this stream. Counted so the warn can be rate-limited: the shape that
        // breaks parsing usually breaks EVERY frame, which is one line per token over the live UI.
        let mut bad_frames: usize = 0;
        loop {
            let event = match tokio::time::timeout(idle_cap, stream.next()).await {
                Ok(Some(Ok(e))) => e,
                Ok(Some(Err(e))) => {
                    stream_err = Some(anyhow!("SSE stream error: {e}"));
                    break;
                }
                Ok(None) => break, // stream ended without [DONE]
                Err(_) => {
                    // No useful frame for `idle_cap`. Name it as a stall so the transcript says why the
                    // turn ended, and mark it Transient-shaped ("timeout") for goal-mode classification.
                    stream_err = Some(anyhow!(
                        "SSE stream error: timeout — no data for {}s{}",
                        idle_cap.as_secs(),
                        if produced {
                            " (stream stalled mid-response)"
                        } else {
                            " (stream never started)"
                        }
                    ));
                    break;
                }
            };
            if event.data.trim() == "[DONE]" {
                break;
            }
            if event.data.trim().is_empty() {
                continue;
            }
            match parse_chunk(&event.data) {
                Ok(chunk) => {
                    // Record usage ONLY on the final chunk (choices empty). Spec-compliant OpenAI sends
                    // usage=null until then, but some gateways (vLLM/LiteLLM/OpenRouter) attach a
                    // CUMULATIVE usage object to EVERY chunk — without this guard an N-chunk stream sums it
                    // N times and inflates /cost (and calls_with_usage) ~N×.
                    if let Some(u) = &chunk.usage {
                        if chunk.choices.is_empty() {
                            cost_meter().record(u);
                            final_usage = Some(u.clone());
                            produced = true; // a usage report means the call really was billed
                        }
                    }
                    if let Some(choice) = chunk.choices.first() {
                        // A dedicated reasoning channel (`reasoning_content`/`reasoning`) is the model
                        // thinking out loud — suppress it entirely so output is uniform across models.
                        // (Clear the spinner: the model IS producing, just not user-facing text yet.)
                        if choice.delta.reasoning_text().is_some() {
                            spin.take();
                        }
                        if let Some(content) = &choice.delta.content {
                            spin.take(); // stop+clear the spinner before the first token prints
                            produced = true; // even a content delta filtered to nothing was real output
                            let shown = think.push(content);
                            if !shown.is_empty() {
                                full.push_str(&shown); // history keeps the RAW markdown
                                crate::ui::tui::add_stream_chars(shown.chars().count() as u64); // live ↑tok pill
                                if retained_display {
                                    crate::ui::tui::assistant_stream_delta(&shown);
                                } else {
                                    let rendered = md.push(&shown); // styled, complete lines (gutter, md, code)
                                    if !rendered.is_empty() {
                                        crate::ui::tui::emit(&rendered); // classic TUI funnel / plain print
                                    }
                                }
                            }
                        }
                        if !choice.delta.tool_calls.is_empty() {
                            spin.take(); // a tool-only turn: clear before the loop prints tool traces
                            produced = true; // a tool-call fragment must never be re-requested
                            let completed = acc.ingest(&choice.delta.tool_calls);
                            if let Some(hook) = eager_hook {
                                for (slot, tc) in completed {
                                    if let Some(h) = hook(slot, &tc) {
                                        eager_by_slot.insert(slot, h);
                                    }
                                }
                            }
                        }
                        if choice.finish_reason.is_some() {
                            finish_reason = choice.finish_reason.clone();
                        }
                    }
                }
                // SILENT BY DEFAULT. What reaches here survived neither parse attempt, so it is not a
                // completion chunk at all — it is the keepalive / comment / `{"type":"ping"}` noise
                // gateways interleave, which this loop has nothing to do with. Narrating it taught the
                // user nothing and, because the shape repeats per frame, printed one line per token
                // over the live UI. The chunk-shaped failures that WERE worth knowing about (a
                // duplicate key dropping a `content` or `tool_calls` delta) no longer land here at all:
                // `parse_chunk` recovers them. Keep the diagnostic behind an env switch for the next
                // time a gateway invents a shape.
                Err(e) => {
                    bad_frames += 1;
                    if frame_debug() && bad_frames <= MAX_FRAME_WARNS {
                        // A raw `eprintln!` would bypass the retained render thread and corrupt the
                        // pinned frame, so route through the TUI funnel when it owns the screen.
                        let warn = format!(
                            "[warn] unparseable stream frame ({e}): {}",
                            truncate_frame(&event.data)
                        );
                        if crate::ui::tui::active() {
                            crate::ui::tui::emit_line(&crate::ui::theme::faint(warn).to_string());
                        } else {
                            eprintln!("\n{warn}");
                        }
                    }
                }
            }
        }
        spin.take(); // stream ended (e.g. empty turn) — ensure the spinner is gone
                     // Debug-only tally, for the same reason the per-frame lines are: these frames are
                     // gateway noise, so a count of them on a healthy turn is a number the user cannot act
                     // on. Under the env switch it tells whoever is diagnosing a new gateway how wide the
                     // mismatch is.
        if frame_debug() && bad_frames > MAX_FRAME_WARNS {
            let line = format!(
                "[warn] {} more unparseable stream frames suppressed ({bad_frames} total this response)",
                bad_frames - MAX_FRAME_WARNS
            );
            if crate::ui::tui::active() {
                crate::ui::tui::emit_line(&crate::ui::theme::faint(line).to_string());
            } else {
                eprintln!("\n{line}");
            }
        }
        let tail = think.finish();
        if !tail.is_empty() {
            full.push_str(&tail);
            if retained_display {
                crate::ui::tui::assistant_stream_delta(&tail);
            } else {
                let rendered = md.push(&tail);
                if !rendered.is_empty() {
                    crate::ui::tui::emit(&rendered);
                }
            }
        }
        if retained_display {
            crate::ui::tui::assistant_stream_finish(stream_err.is_some());
        } else {
            let closing = md.finish(); // flush the final partial line + close any dangling code fence
            if !closing.is_empty() {
                crate::ui::tui::emit(&closing);
            }
            if !full.is_empty() {
                crate::ui::tui::emit("\n"); // one blank line of breathing room before the next turn
            }
        }

        // BLANK STREAM ⇒ REPLAY, not a dead turn. A stall or drop that produced NOTHING is exactly the
        // case the user hits as "it froze; I killed the turn and told it to continue, then it was fine" —
        // the manual retry worked because there was nothing wrong with the request. Do that retry here.
        // Guarded on `!produced`, so a stream that emitted any text/tool-fragment/usage never replays
        // (that would duplicate output). Also skipped once the model claimed `finish_reason`: an
        // intentionally empty completion is a real answer, not a failure.
        let blank_failure = !produced && finish_reason.is_none();
        if blank_failure && blank_attempt < STREAM_BLANK_RETRIES {
            blank_attempt += 1;
            let delay = backoff_ms(blank_attempt - 1, 400, 4_000);
            let why = stream_err
                .as_ref()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "stream closed with no output".to_string());
            stream_retry_note(&why, blank_attempt, STREAM_BLANK_RETRIES, delay);
            // Detach any eager handles from the abandoned attempt. They are read-only by policy (the
            // eager starter refuses destructive/unsafe calls), so dropping them is safe.
            drop(eager_by_slot);
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            continue;
        }

        // The display is now clean (fence closed, partial line flushed) — surface any transport error
        // that interrupted the stream. The partial `full` / tool-calls are intentionally discarded: a
        // truncated tool-call's arguments JSON is unparseable, so feeding it back would be worse than a
        // clean turn failure the caller can retry.
        if let Some(e) = stream_err {
            return Err(e);
        }

        // Map eager handles from accumulator SLOT to final POSITION (what the executor stitches by).
        let indexed = acc.finish_indexed();
        let eager: Vec<(usize, tokio::task::JoinHandle<String>)> = indexed
            .iter()
            .enumerate()
            .filter_map(|(pos, (slot, _))| eager_by_slot.remove(slot).map(|h| (pos, h)))
            .collect();
        return Ok(ChatTurn {
            content: if full.is_empty() { None } else { Some(full) },
            tool_calls: indexed.into_iter().map(|(_, tc)| tc).collect(),
            finish_reason,
            usage: final_usage,
            eager,
        });
    } // end blank-stream replay loop
}

/// A streaming filter that suppresses `<think>…</think>` reasoning blocks from the printed output.
/// Some models emit literal think tags into the content channel (sometimes empty `<think></think>`),
/// which is visual noise in the TUI. `push` buffers across SSE deltas so a tag split between chunks
/// is still caught; it holds back only the trailing bytes that could be a partial tag, so latency
/// stays one short tag-length behind at most.
#[derive(Default)]
struct ThinkFilter {
    buf: String,
    in_think: bool,
}

const THINK_OPEN: &str = "<think>";
const THINK_CLOSE: &str = "</think>";

impl ThinkFilter {
    /// Feed a content delta; return the text safe to print now (think blocks removed).
    fn push(&mut self, s: &str) -> String {
        self.buf.push_str(s);
        let mut out = String::new();
        loop {
            if self.in_think {
                if let Some(i) = self.buf.find(THINK_CLOSE) {
                    self.buf.drain(..i + THINK_CLOSE.len());
                    self.in_think = false;
                } else {
                    let keep = partial_suffix(&self.buf, THINK_CLOSE);
                    let drop_to = self.buf.len() - keep;
                    self.buf.drain(..drop_to);
                    break;
                }
            } else if let Some(i) = self.buf.find(THINK_OPEN) {
                out.push_str(&self.buf[..i]);
                self.buf.drain(..i + THINK_OPEN.len());
                self.in_think = true;
            } else {
                let keep = partial_suffix(&self.buf, THINK_OPEN);
                let emit_to = self.buf.len() - keep;
                out.push_str(&self.buf[..emit_to]);
                self.buf.drain(..emit_to);
                break;
            }
        }
        out
    }

    /// Stream end: emit any buffered non-think remainder (a dangling, never-closed `<think>` is dropped).
    fn finish(&mut self) -> String {
        if self.in_think {
            return String::new();
        }
        std::mem::take(&mut self.buf)
    }
}

/// Length of the longest suffix of `s` that equals a prefix of `tag` (both compared as bytes; `tag`
/// is ASCII so any match lands on a char boundary). Used to hold back a possible partial tag.
fn partial_suffix(s: &str, tag: &str) -> usize {
    let max = tag.len().min(s.len());
    for k in (1..=max).rev() {
        if &s.as_bytes()[s.len() - k..] == &tag.as_bytes()[..k] {
            return k;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::FunctionDelta;

    #[test]
    fn stall_timeout_defaults_and_clamps_the_env_override() {
        // Serialize with every other env-touching test (the shared process environment is global).
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        std::env::remove_var(STREAM_STALL_ENV);
        assert_eq!(
            stream_stall_timeout().as_secs(),
            STREAM_STALL_SECS,
            "no override → the default"
        );
        std::env::set_var(STREAM_STALL_ENV, "240");
        assert_eq!(
            stream_stall_timeout().as_secs(),
            240,
            "a sane override is honoured"
        );
        // A typo must not disable the deadline that keeps a stalled turn from hanging forever.
        std::env::set_var(STREAM_STALL_ENV, "0");
        assert_eq!(
            stream_stall_timeout().as_secs(),
            15,
            "clamped up to the floor"
        );
        std::env::set_var(STREAM_STALL_ENV, "999999");
        assert_eq!(
            stream_stall_timeout().as_secs(),
            1800,
            "clamped down to the ceiling"
        );
        std::env::set_var(STREAM_STALL_ENV, "not-a-number");
        assert_eq!(
            stream_stall_timeout().as_secs(),
            STREAM_STALL_SECS,
            "garbage → the default"
        );
        std::env::remove_var(STREAM_STALL_ENV);
    }

    #[test]
    fn a_stalled_stream_classifies_as_transient_so_goal_mode_retries() {
        // The watchdog's wording must land on the retryable side of `classify_api_error` — a stall is
        // exactly the flakiness goal mode exists to survive.
        let e = anyhow!("SSE stream error: timeout — no data for 90s (stream never started)");
        assert_eq!(classify_api_error(&e), ApiErrorKind::Transient);
        let mid =
            anyhow!("SSE stream error: timeout — no data for 90s (stream stalled mid-response)");
        assert_eq!(classify_api_error(&mid), ApiErrorKind::Transient);
    }

    #[test]
    fn cost_meter_last_call_tracks_most_recent() {
        let m = CostMeter::default();
        assert_eq!(m.last_call(), None, "no usage seen yet");
        m.record(&Usage {
            prompt_tokens: Some(1000),
            completion_tokens: Some(50),
            cache_read_input_tokens: Some(800),
            ..Default::default()
        });
        assert_eq!(m.last_call(), Some((1000, 800, 50)));
        m.record(&Usage {
            prompt_tokens: Some(2000),
            completion_tokens: Some(10),
            ..Default::default()
        });
        assert_eq!(
            m.last_call(),
            Some((2000, 0, 10)),
            "most recent call wins; no cache → 0"
        );
        m.reset();
        assert_eq!(m.last_call(), None, "reset clears the last-call signal");
    }

    #[test]
    fn body_blames_effort_matches_provider_wordings() {
        // The three shapes real providers use — with/without the underscore, various phrasings.
        assert!(body_blames_effort("Unknown parameter: reasoning_effort"));
        assert!(body_blames_effort(
            "reasoning_effort: unsupported value 'max'"
        ));
        assert!(body_blames_effort(
            "This model does not support reasoning effort"
        ));
        assert!(
            body_blames_effort("REASONING_EFFORT is invalid"),
            "case-insensitive"
        );
        // A 400 about something else must NOT be mistaken for an effort rejection.
        assert!(!body_blames_effort("Unknown parameter: temperature"));
        assert!(!body_blames_effort("context_length_exceeded"));
    }

    #[test]
    fn effort_unsupported_set_records_and_reports_per_model() {
        // A model is "supported" until it 400s once; then it's remembered for the session.
        let model = "test-only-effort-model-xyz"; // unique so it can't collide with another test
        assert!(
            !effort_known_unsupported(model),
            "unseen model starts supported"
        );
        mark_effort_unsupported(model);
        assert!(
            effort_known_unsupported(model),
            "marked model is remembered"
        );
        // An unrelated model is unaffected by the mark above.
        assert!(!effort_known_unsupported("some-other-model-abc"));
    }

    #[test]
    fn retry_classifier_only_retries_transient_statuses() {
        for s in [429u16, 500, 502, 503, 504] {
            assert!(is_retryable_status(s), "{s} should be retryable");
        }
        for s in [200u16, 400, 401, 403, 404, 422, 501] {
            assert!(!is_retryable_status(s), "{s} should NOT be retryable");
        }
    }

    #[test]
    fn backoff_ceiling_is_monotonic_and_capped() {
        let (base, cap) = (400u64, 8_000u64);
        let seq: Vec<u64> = (0..6).map(|a| backoff_ceiling_ms(a, base, cap)).collect();
        // base * 2^attempt until the cap, never decreasing, never above the cap, never zero.
        assert_eq!(seq[0], 400);
        assert_eq!(seq[1], 800);
        assert_eq!(seq[2], 1600);
        for w in seq.windows(2) {
            assert!(w[1] >= w[0], "ceiling must not decrease: {seq:?}");
        }
        assert!(
            seq.iter().all(|&d| d >= 1 && d <= cap),
            "every delay in [1, cap]: {seq:?}"
        );
        assert_eq!(*seq.last().unwrap(), cap, "saturates at the cap");
    }

    #[test]
    fn backoff_jitter_stays_within_half_to_ceiling() {
        let (base, cap) = (400u64, 8_000u64);
        for attempt in 0..6 {
            let ceil = backoff_ceiling_ms(attempt, base, cap);
            for _ in 0..50 {
                let d = backoff_ms(attempt, base, cap);
                assert!(
                    d >= ceil / 2 && d <= ceil,
                    "jitter {d} outside [{}, {ceil}]",
                    ceil / 2
                );
            }
        }
    }

    #[test]
    fn interactive_backoff_is_faster_than_goal() {
        // The whole point of splitting the two: an interactive turn (someone waiting) must never sit
        // in a lone 30s pause — that reads as a hang. Goal mode (nobody watching, runs long) keeps the
        // higher ceiling. Pin BOTH the absolute cap and the relative ordering so a future edit to
        // either constant can't quietly collapse them back together.
        for attempt in 0..12 {
            let iv = interactive_backoff_ms(attempt);
            assert!(
                iv <= 4_000,
                "interactive delay {iv} (attempt {attempt}) exceeded its 4s cap — a lone pause this \
                 long is the hang feel this split exists to remove"
            );
        }
        // At a high attempt both have saturated: interactive at 4s, goal at 30s. Compare CEILINGS
        // (not the jittered draw) so the assertion is deterministic.
        let iv_ceil = backoff_ceiling_ms(6, 300, 4_000);
        let goal_ceil = backoff_ceiling_ms(6, 500, 30_000);
        assert_eq!(iv_ceil, 4_000, "interactive saturates at 4s");
        assert_eq!(goal_ceil, 30_000, "goal saturates at 30s");
        assert!(
            iv_ceil < goal_ceil,
            "interactive must back off faster than goal ({iv_ceil} vs {goal_ceil})"
        );
    }

    #[test]
    fn classify_api_error_splits_permanent_from_transient() {
        // Permanent 4xx — retrying can't fix these.
        for code in ["HTTP 400", "HTTP 401", "HTTP 403", "HTTP 404"] {
            let e = anyhow::anyhow!("upstream returned {code} Bad Request: nope");
            assert_eq!(
                classify_api_error(&e),
                ApiErrorKind::Permanent,
                "{code} must be Permanent"
            );
        }
        // Everything else leans Transient (goal mode's survival bias).
        for msg in [
            "upstream returned HTTP 429 Too Many Requests: slow down",
            "upstream returned HTTP 503 Service Unavailable: try later",
            "request failed after retries",
            "SSE stream error: connection reset",
            "health probe request failed",
        ] {
            let e = anyhow::anyhow!(msg);
            assert_eq!(
                classify_api_error(&e),
                ApiErrorKind::Transient,
                "{msg} must be Transient"
            );
        }
    }

    #[test]
    fn cost_meter_sums_real_usage_and_ignores_empty() {
        // A LOCAL meter (not the global) so parallel tests don't race on it.
        let m = CostMeter::default();
        m.record(&Usage {
            prompt_tokens: Some(100),
            completion_tokens: Some(40),
            total_tokens: Some(140),
            ..Default::default()
        });
        m.record(&Usage {
            prompt_tokens: Some(10),
            completion_tokens: Some(5),
            total_tokens: None,
            ..Default::default()
        });
        m.record(&Usage::default()); // all-None → NOT counted as a usage-reporting call
        assert_eq!(m.snapshot(), (110, 45, 2));
        m.reset();
        assert_eq!(m.snapshot(), (0, 0, 0));
    }

    #[test]
    fn cost_meter_tracks_cache_reads() {
        let m = CostMeter::default();
        m.record(&Usage {
            prompt_tokens: Some(100),
            cache_read_input_tokens: Some(80),
            ..Default::default()
        });
        assert_eq!(
            m.cache_read(),
            80,
            "cache-read tokens accumulate for the /cost probe"
        );
        m.reset();
        assert_eq!(m.cache_read(), 0);
    }

    #[test]
    fn anthropic_model_detection() {
        for m in [
            "opus-4-8",
            "claude-3-5",
            "Sonnet",
            "fable-5",
            "anthropic/claude",
            "claude-haiku-4-5",
        ] {
            assert!(is_anthropic_model(m), "{m} should be detected as Anthropic");
        }
        for m in ["gpt-4o", "deepseek-chat", "gemini-1.5", "llama-3"] {
            assert!(!is_anthropic_model(m), "{m} should NOT be Anthropic");
        }
        // Word-boundary guard: community models that merely EMBED a token must not trip AUTO cache.
        for m in [
            "fable13b",
            "haikuwriter",
            "mythos-13b",
            "opusculum-7b",
            "sonnetizer",
        ] {
            assert!(
                !is_anthropic_model(m),
                "{m} embeds a token but is not Anthropic"
            );
        }
    }

    #[test]
    fn anthropic_endpoint_is_matched_on_host_not_substring() {
        // First-party, with and without a path/port/scheme variation.
        for b in [
            "https://api.anthropic.com/v1",
            "https://api.anthropic.com/v1/",
            "https://API.Anthropic.COM/v1",
            "http://api.anthropic.com:443/v1",
        ] {
            assert!(is_anthropic_endpoint(b), "{b} is first-party Anthropic");
        }
        // A proxy that merely MENTIONS anthropic in its path is NOT first-party. Getting this wrong
        // would attach `x-api-key` + `anthropic-version` to a third-party gateway, and would make the
        // check misreport which dialect the endpoint speaks.
        for b in [
            "https://gw.example.com/anthropic/v1",
            "https://openrouter.ai/api/v1",
            "https://api.anthropic.com.evil.test/v1",
            "http://localhost:11434/v1",
        ] {
            assert!(!is_anthropic_endpoint(b), "{b} must not be first-party");
        }
        // A subdomain of the real host still is.
        assert!(is_anthropic_endpoint("https://eu.api.anthropic.com/v1"));
    }

    #[test]
    fn cache_enabled_auto_and_forced() {
        assert!(cache_enabled(None, "opus-4-8"), "AUTO on for Anthropic");
        assert!(!cache_enabled(None, "gpt-4o"), "AUTO off for non-Anthropic");
        assert!(!cache_enabled(Some(false), "opus-4-8"), "explicit off wins");
        assert!(cache_enabled(Some(true), "gpt-4o"), "explicit on wins");
    }

    #[test]
    fn cache_breakpoints_stamp_tools_system_and_last_stable_msg() {
        let mut msgs = vec![
            Message::system("sys"),
            Message::user("first"),
            Message::assistant("answer"),
            Message::tool_result("c1", "result"),
            Message::user("newest"), // volatile — must NOT be stamped
        ];
        let mut tools = vec![
            ToolDef::function("a", "d", serde_json::json!({"type":"object"})),
            ToolDef::function("b", "d", serde_json::json!({"type":"object"})),
        ];
        apply_cache_breakpoints(&mut msgs, &mut tools);
        assert!(
            tools[0].cache_control.is_none(),
            "only the LAST tool def is stamped"
        );
        assert!(
            tools[1].cache_control.is_some(),
            "last tool def caches the whole tool block"
        );
        assert!(msgs[0].cache_control.is_some(), "system message stamped");
        assert!(
            msgs[3].cache_control.is_some(),
            "last stable assistant/tool message stamped"
        );
        assert!(
            msgs[4].cache_control.is_none(),
            "the newest user turn stays uncached"
        );
    }

    #[test]
    fn cache_breakpoints_short_history_only_system_and_tools() {
        let mut msgs = vec![Message::system("sys"), Message::user("hi")];
        let mut tools = vec![ToolDef::function(
            "a",
            "d",
            serde_json::json!({"type":"object"}),
        )];
        apply_cache_breakpoints(&mut msgs, &mut tools);
        assert!(msgs[0].cache_control.is_some());
        assert!(tools[0].cache_control.is_some());
        assert!(
            msgs[1].cache_control.is_none(),
            "no history breakpoint when n < 3"
        );
    }

    #[test]
    fn accumulator_does_not_complete_a_call_with_partial_args() {
        // THE BUG: a call is only "done" once its arguments are whole. Slot 0's JSON is still open
        // when slot 1 starts, so completing it would hand the executor `{"path":` — which
        // `parse_call_args` cannot parse, or (when the fragment is empty) reads as `{}`, running the
        // tool with NO arguments and failing with `missing required string arg` while the transcript
        // line, built later from the full call, showed the argument plainly.
        let mut acc = ToolCallAccumulator::default();
        assert!(acc
            .ingest(&[delta(0, Some("a"), Some("file_read"), Some(r#"{"path":"#))])
            .is_empty());
        // The rest of slot 0's arguments arrive (id-less, still the active call)…
        assert!(acc
            .ingest(&[delta(0, None, None, Some(r#""x.rs"}"#))])
            .is_empty());
        // …and only NOW, whole, does the next slot's start complete it.
        let done = acc.ingest(&[delta(1, Some("b"), Some("file_glob"), Some("{}"))]);
        assert_eq!(done.len(), 1, "a whole call completes");
        assert_eq!(done[0].1.function.arguments, r#"{"path":"x.rs"}"#);

        // And the failing shape: a call still mid-JSON when the next slot starts is NOT dispatched.
        let mut acc = ToolCallAccumulator::default();
        acc.ingest(&[delta(0, Some("a"), Some("file_read"), Some(r#"{"path":"#))]);
        assert!(
            acc.ingest(&[delta(1, Some("b"), Some("file_glob"), Some("{}"))])
                .is_empty(),
            "a half-streamed call must not be dispatched"
        );
    }

    #[test]
    fn accumulator_does_not_complete_a_call_with_empty_args() {
        // An id+name opening fragment with no arguments yet: `""` is indistinguishable from "no
        // arguments", and `parse_call_args` turns it into a valid `{}`. Never eager-dispatch it.
        let mut acc = ToolCallAccumulator::default();
        acc.ingest(&[delta(0, Some("a"), Some("web_fetch"), Some(""))]);
        assert!(
            acc.ingest(&[delta(1, Some("b"), Some("web_search"), Some("{}"))])
                .is_empty(),
            "empty args are 'not yet', not 'no arguments'"
        );
    }

    #[test]
    fn accumulator_routes_idless_fragments_to_the_active_slot() {
        // An index-omitting provider: every fragment claims index 0. The second call's `id` reroutes
        // it to a fresh slot, but its ARGUMENT fragments carry no id — they must follow the active
        // slot rather than pouring back into slot 0 (which corrupted slot 0's JSON and left call #2
        // argument-less).
        let mut acc = ToolCallAccumulator::default();
        acc.ingest(&[delta(
            0,
            Some("a"),
            Some("file_read"),
            Some(r#"{"path":"a.rs"}"#),
        )]);
        acc.ingest(&[delta(0, Some("b"), Some("web_fetch"), Some(""))]);
        acc.ingest(&[delta(0, None, None, Some(r#"{"url":"#))]);
        acc.ingest(&[delta(0, None, None, Some(r#""https://x.dev"}"#))]);
        let calls = acc.finish();
        assert_eq!(calls.len(), 2, "both calls survive");
        assert_eq!(calls[0].function.arguments, r#"{"path":"a.rs"}"#);
        assert_eq!(
            calls[1].function.arguments, r#"{"url":"https://x.dev"}"#,
            "id-less argument fragments followed the active call"
        );
    }

    #[test]
    fn accumulator_still_completes_when_args_are_whole() {
        // The eager fast path must survive the tightening: whole arguments + a later slot ⇒ complete.
        let mut acc = ToolCallAccumulator::default();
        acc.ingest(&[delta(
            0,
            Some("a"),
            Some("file_read"),
            Some(r#"{"path":"x.rs"}"#),
        )]);
        let done = acc.ingest(&[delta(1, Some("b"), Some("file_glob"), Some("{}"))]);
        assert_eq!(done.len(), 1, "a complete call still dispatches early");
        assert_eq!(done[0].1.function.arguments, r#"{"path":"x.rs"}"#);
    }

    #[test]
    fn args_complete_accepts_only_whole_objects() {
        assert!(args_complete("{}"), "an argument-less call streams `{{}}`");
        assert!(args_complete(r#"{"a":1}"#));
        assert!(!args_complete(""), "`\"\"` means 'not yet', never '{{}}'");
        assert!(!args_complete("   "));
        assert!(!args_complete(r#"{"a":"#), "truncated");
        assert!(!args_complete("[1,2]"), "an array is not an args object");
        assert!(!args_complete(r#""a string""#));
    }

    fn delta(
        index: usize,
        id: Option<&str>,
        name: Option<&str>,
        args: Option<&str>,
    ) -> ToolCallDelta {
        ToolCallDelta {
            index,
            id: id.map(String::from),
            function: Some(FunctionDelta {
                name: name.map(String::from),
                arguments: args.map(String::from),
            }),
        }
    }

    #[test]
    fn accumulator_emits_completion_when_next_slot_starts() {
        let mut acc = ToolCallAccumulator::default();
        // First call streams over two batches — nothing completes yet.
        assert!(acc
            .ingest(&[delta(0, Some("a"), Some("file_read"), Some(r#"{"path":"#))])
            .is_empty());
        assert!(acc
            .ingest(&[delta(0, None, None, Some(r#""x.rs"}"#))])
            .is_empty());
        // Slot 1 starts → slot 0 completes with its FULL arguments.
        let done = acc.ingest(&[delta(1, Some("b"), Some("file_glob"), Some("{}"))]);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].0, 0, "completed slot key");
        assert_eq!(done[0].1.function.name, "file_read");
        assert_eq!(done[0].1.function.arguments, r#"{"path":"x.rs"}"#);
        // The LAST call never emits mid-stream — finish_indexed covers it, slot keys intact.
        let indexed = acc.finish_indexed();
        assert_eq!(indexed.len(), 2);
        assert_eq!(
            (indexed[0].0, indexed[0].1.function.name.as_str()),
            (0, "file_read")
        );
        assert_eq!(
            (indexed[1].0, indexed[1].1.function.name.as_str()),
            (1, "file_glob")
        );
    }

    #[test]
    fn accumulator_completion_survives_id_reroute() {
        // Index-omitting provider: both calls claim index 0; the second id reroutes to a fresh
        // slot — and that reroute must still emit the first call's completion.
        let mut acc = ToolCallAccumulator::default();
        assert!(acc
            .ingest(&[delta(0, Some("a"), Some("t_one"), Some("{}"))])
            .is_empty());
        let done = acc.ingest(&[delta(0, Some("b"), Some("t_two"), Some("{}"))]);
        assert_eq!(done.len(), 1);
        assert_eq!(done[0].1.function.name, "t_one");
        let calls = acc.finish();
        assert_eq!(calls.len(), 2, "both calls survive the collision");
    }

    #[test]
    fn accumulator_reassembles_streamed_tool_call() {
        let mut acc = ToolCallAccumulator::default();
        acc.ingest(&[delta(
            0,
            Some("call_1"),
            Some("memory_search"),
            Some("{\"que"),
        )]);
        acc.ingest(&[delta(0, None, None, Some("ry\":\"x\"}"))]);
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].function.name, "memory_search");
        assert_eq!(calls[0].function.arguments, "{\"query\":\"x\"}");
    }

    #[test]
    fn accumulator_handles_two_parallel_calls() {
        let mut acc = ToolCallAccumulator::default();
        acc.ingest(&[
            delta(0, Some("a"), Some("f"), Some("{}")),
            delta(1, Some("b"), Some("g"), Some("{}")),
        ]);
        let calls = acc.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "f");
        assert_eq!(calls[1].function.name, "g");
    }

    #[test]
    fn delta_captures_both_reasoning_field_names() {
        // DeepSeek-style `reasoning_content` and OpenRouter-style `reasoning` both resolve to the
        // same channel, whichever spelling the provider used.
        let a: crate::core::types::Delta =
            serde_json::from_str(r#"{"reasoning_content":"thinking…"}"#).unwrap();
        assert_eq!(a.reasoning_text(), Some("thinking…"));
        let b: crate::core::types::Delta =
            serde_json::from_str(r#"{"reasoning":"thinking…"}"#).unwrap();
        assert_eq!(b.reasoning_text(), Some("thinking…"));
        // Plain content chunk leaves the reasoning channel empty.
        let c: crate::core::types::Delta = serde_json::from_str(r#"{"content":"hi"}"#).unwrap();
        assert!(c.reasoning_text().is_none());
    }

    #[test]
    fn delta_with_both_reasoning_spellings_still_parses() {
        // THE BUG. The two spellings used to be ONE field plus `#[serde(alias = "reasoning")]`, so a
        // provider that mirrors its reasoning text into BOTH keys of a single delta — which
        // OpenRouter-shaped gateways do — was a serde `duplicate field` error. serde rejects the
        // whole object, so the frame was DISCARDED, and the loop printed
        // `[warn] unparseable stream frame (duplicate field ...)` once per streamed reasoning token
        // over the live UI. Two separate fields cannot collide.
        let d: crate::core::types::Delta = serde_json::from_str(
            r#"{"content":null,"reasoning":" ch","reasoning_content":" ch",
                "reasoning_details":[{"format":"unknown"}]}"#,
        )
        .expect("both spellings in one delta must parse");
        assert_eq!(d.reasoning_text(), Some("ch"));

        // The real cost was never the noise: anything riding in the SAME delta died with the frame.
        // A tool call lost this way is a step the agent never takes.
        let d: crate::core::types::Delta = serde_json::from_str(
            r#"{"reasoning":"x","reasoning_content":"x",
                "tool_calls":[{"index":0,"id":"c1","function":{"name":"file_read","arguments":"{}"}}]}"#,
        )
        .expect("a tool-call fragment must survive a mirrored reasoning delta");
        assert_eq!(d.tool_calls.len(), 1);
        assert_eq!(
            d.tool_calls[0].function.as_ref().unwrap().name.as_deref(),
            Some("file_read")
        );

        // Same shape on the non-streaming path, where a dropped message is a whole dead turn.
        let m = resp_msg(
            r#"{"content":null,"reasoning":"the answer","reasoning_content":"the answer"}"#,
        );
        assert_eq!(resolve_content(&m).as_deref(), Some("the answer"));
    }

    #[test]
    fn parse_chunk_recovers_a_duplicate_key_frame() {
        // A gateway that mirrors one value into two spellings of the same channel emits a literal
        // duplicate key once the two spellings map to one field. Deriving `Deserialize` rejects the
        // whole object there — dropping whatever `content`/`tool_calls` shared that delta — so the
        // lenient pass routes through `Value`, whose object map takes duplicate keys last-wins.
        let dup = r#"{"choices":[{"delta":{"content":"hi","content":"hi"}}]}"#;
        assert!(
            serde_json::from_str::<ChatChunk>(dup).is_err(),
            "precondition: the strict derive must reject a duplicate key",
        );
        let chunk = parse_chunk(dup).expect("lenient pass recovers the frame");
        assert_eq!(
            chunk.choices[0].delta.content.as_deref(),
            Some("hi"),
            "the payload riding in that delta must survive",
        );

        // The shape from the report: both reasoning spellings plus a tool-call fragment in one delta.
        // The fragment is the part that matters — a lost one is a step the agent never takes.
        let real = r#"{"choices":[{"delta":{"reasoning":" ch","reasoning_content":" ch",
            "tool_calls":[{"index":0,"id":"c1","function":{"name":"file_read","arguments":"{}"}}]}}]}"#;
        let chunk = parse_chunk(real).expect("mirrored reasoning + tool call must parse");
        assert_eq!(chunk.choices[0].delta.reasoning_text(), Some("ch"));
        assert_eq!(chunk.choices[0].delta.tool_calls.len(), 1);

        // Well-shaped frames still take the strict path unchanged.
        let ok = r#"{"choices":[{"delta":{"content":"x"}}]}"#;
        assert_eq!(
            parse_chunk(ok).unwrap().choices[0].delta.content.as_deref(),
            Some("x"),
        );

        // Genuine non-chunks stay errors — the caller drops them, it must not treat them as content.
        assert!(parse_chunk(": keepalive").is_err());
        assert!(parse_chunk(r#"{"choices":"not-an-array"}"#).is_err());
        // The reported error names the offending key (the strict one), not the vaguer second pass.
        let e = parse_chunk(r#"{"choices":[{"delta":{"content":"a","content":1}}]}"#).unwrap_err();
        assert!(e.to_string().contains("duplicate field"), "err: {e}");
    }

    #[test]
    fn frame_diagnostics_are_off_unless_the_env_switch_is_truthy() {
        // The default must be silent: these frames are gateway noise, and one line per frame is one
        // line per token painted over a live turn. Serial-safe — no other test reads this var.
        let prev = std::env::var(FRAME_DEBUG_ENV).ok();
        std::env::remove_var(FRAME_DEBUG_ENV);
        assert!(!frame_debug(), "unset must mean silent");
        for off in ["", "0", "false", "off", "no", " OFF "] {
            std::env::set_var(FRAME_DEBUG_ENV, off);
            assert!(!frame_debug(), "{off:?} must read as off");
        }
        for on in ["1", "true", "yes", "please"] {
            std::env::set_var(FRAME_DEBUG_ENV, on);
            assert!(frame_debug(), "{on:?} must read as on");
        }
        match prev {
            Some(v) => std::env::set_var(FRAME_DEBUG_ENV, v),
            None => std::env::remove_var(FRAME_DEBUG_ENV),
        }
    }

    #[test]
    fn frame_warn_payload_is_truncated_on_a_char_boundary() {
        // Frames carry UTF-8 (the user's own prose comes back as reasoning), so a byte-index cut
        // would panic mid-codepoint on exactly the streams this warn exists to describe.
        let short = r#"{"choices":[]}"#;
        assert_eq!(truncate_frame(short), short);

        let long = format!(r#"{{"reasoning":"{}"}}"#, "ườ".repeat(200));
        let cut = truncate_frame(&long);
        assert!(cut.chars().count() <= FRAME_WARN_MAX + 32, "cut: {cut}");
        assert!(cut.contains("bytes total"), "cut: {cut}");
    }

    /// Deserialize a non-streaming `message` object the way `chat_with_tools` does.
    fn resp_msg(json: &str) -> crate::core::types::RespMessage {
        serde_json::from_str(json).expect("valid message json")
    }

    #[test]
    fn reasoning_only_reply_is_not_mistaken_for_provider_silence() {
        // THE BUG: the streaming `Delta` has always read `reasoning_content`/`reasoning`, but the
        // non-streaming `RespMessage` did not — and SUB-AGENTS are the one caller that runs
        // exclusively on the non-streaming path. A provider that puts the whole reply in the
        // reasoning channel therefore deserialized to `content: None, tool_calls: []`, which every
        // caller classifies as an empty-200 provider failure. The model spoke; we could not hear it.
        assert_eq!(
            resolve_content(&resp_msg(
                r#"{"content":null,"reasoning_content":"the answer"}"#
            ))
            .as_deref(),
            Some("the answer"),
        );
        // OpenRouter spells the same channel `reasoning` — a separate field, resolved together.
        assert_eq!(
            resolve_content(&resp_msg(r#"{"reasoning":"the answer"}"#)).as_deref(),
            Some("the answer"),
        );
        // Whitespace-only `content` is as silent as null.
        assert_eq!(
            resolve_content(&resp_msg(
                r#"{"content":"   ","reasoning_content":"the answer"}"#
            ))
            .as_deref(),
            Some("the answer"),
        );
    }

    #[test]
    fn reasoning_never_displaces_or_appends_to_a_real_reply() {
        // The fallback is exactly that — a fallback. Real content wins untouched: appending reasoning
        // to it would leak the chain-of-thought the CLI deliberately suppresses on the streaming path.
        assert_eq!(
            resolve_content(&resp_msg(
                r#"{"content":"real answer","reasoning_content":"private thinking"}"#
            ))
            .as_deref(),
            Some("real answer"),
        );
        // A TOOL-CALL turn legitimately has no content. Substituting reasoning there would turn a
        // tool call into a text turn and lose the step.
        let with_call = resp_msg(
            r#"{"content":null,"reasoning_content":"private thinking",
                "tool_calls":[{"id":"c","type":"function","function":{"name":"f","arguments":"{}"}}]}"#,
        );
        assert!(
            resolve_content(&with_call).is_none(),
            "a tool-call turn must stay a tool-call turn"
        );
        // Genuine silence stays silence — the empty-200 path must still fire.
        assert!(resolve_content(&resp_msg(r#"{"content":null}"#)).is_none());
        assert!(resolve_content(&resp_msg(r#"{"reasoning_content":"  "}"#)).is_none());
    }

    #[test]
    fn think_filter_strips_a_whole_block() {
        let mut f = ThinkFilter::default();
        let out = f.push("Hello <think>secret reasoning</think>world") + &f.finish();
        assert_eq!(out, "Hello world");
    }

    #[test]
    fn think_filter_strips_empty_tags() {
        let mut f = ThinkFilter::default();
        let out = f.push("<think></think>answer") + &f.finish();
        assert_eq!(out, "answer");
    }

    #[test]
    fn think_filter_handles_a_tag_split_across_deltas() {
        // The open/close tags arrive in pieces — the classic SSE-delta hazard.
        let mut f = ThinkFilter::default();
        let mut out = String::new();
        for piece in ["A<thi", "nk>hidden</thi", "nk>B"] {
            out.push_str(&f.push(piece));
        }
        out.push_str(&f.finish());
        assert_eq!(out, "AB");
    }

    #[test]
    fn think_filter_passes_plain_text_through_unchanged() {
        let mut f = ThinkFilter::default();
        let out = f.push("just a normal answer, no tags") + &f.finish();
        assert_eq!(out, "just a normal answer, no tags");
    }

    #[test]
    fn think_filter_drops_an_unclosed_block() {
        let mut f = ThinkFilter::default();
        let out = f.push("before <think>dangling forever") + &f.finish();
        assert_eq!(out, "before ");
    }

    #[test]
    fn accumulator_synthesizes_missing_id() {
        let mut acc = ToolCallAccumulator::default();
        acc.ingest(&[delta(0, None, Some("f"), Some("{}"))]);
        let calls = acc.finish();
        assert_eq!(calls.len(), 1);
        assert!(!calls[0].id.is_empty(), "a missing id must be synthesized");
    }

    /// A one-request-per-connection stub gateway. `models_status`/`chat_status` let a test model the
    /// shape that actually bit us: a gateway that serves `/models` to anyone but enforces auth on
    /// `/chat/completions`.
    async fn stub_gateway(
        models_status: u16,
        chat_status: u16,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let (status, body) = if req.contains("/chat/completions") {
                    match chat_status {
                        200 => (
                            200,
                            r#"{"choices":[{"message":{"content":"ok"}}]}"#.to_string(),
                        ),
                        c => (c, r#"{"error":{"message":"Invalid API key"}}"#.to_string()),
                    }
                } else {
                    match models_status {
                        200 => (200, r#"{"object":"list","data":[{"id":"m1"}]}"#.to_string()),
                        c => (c, r#"{"error":{"message":"nope"}}"#.to_string()),
                    }
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        (base, handle)
    }

    /// The regression: a gateway that hands out its model list unauthenticated must NOT let a bad
    /// key pass config validation. Before the chat-probe, this returned `Ok` and the user only found
    /// out on their first real turn — which reads as "the endpoint and key are right but it doesn't
    /// work".
    #[tokio::test]
    async fn open_models_list_does_not_certify_a_key_the_chat_path_rejects() {
        let (base, srv) = stub_gateway(200, 401).await;
        let http = reqwest::Client::new();
        let got = check_endpoint(&http, &base, Some("sk-corrupted")).await;
        srv.abort();
        assert!(
            matches!(got, EndpointCheck::Auth(_)),
            "a 401 on /chat/completions must surface as Auth, got {got:?}"
        );
    }

    #[tokio::test]
    async fn a_key_both_endpoints_accept_still_validates() {
        let (base, srv) = stub_gateway(200, 200).await;
        let http = reqwest::Client::new();
        let got = check_endpoint(&http, &base, Some("sk-good")).await;
        srv.abort();
        match got {
            EndpointCheck::Ok(infos) => {
                assert_eq!(infos.len(), 1, "the model list is carried back")
            }
            other => panic!("expected Ok, got {other:?}"),
        }
    }

    /// Only 401/403 counts. A 400 (e.g. the probe model isn't enabled for this account) must not be
    /// reported as a bad key, or we'd send the user off to re-paste a credential that was fine.
    #[tokio::test]
    async fn a_non_auth_chat_failure_does_not_condemn_the_key() {
        let (base, srv) = stub_gateway(200, 400).await;
        let http = reqwest::Client::new();
        let got = check_endpoint(&http, &base, Some("sk-good")).await;
        srv.abort();
        assert!(
            matches!(got, EndpointCheck::Ok(_)),
            "a 400 proves nothing about the key, got {got:?}"
        );
    }

    /// A gateway that accepts the connection and then says nothing must not hang interactive setup:
    /// the probe has its own deadline (the shared client's read timeout is sized for a streamed turn),
    /// and a timeout means "unproven", never "bad key".
    #[tokio::test]
    async fn a_stalled_chat_probe_times_out_without_condemning_the_key() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        // Answer /models, then accept the chat connection and never reply.
        let srv = tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut held = Vec::new();
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                if String::from_utf8_lossy(&buf[..n]).contains("/chat/completions") {
                    held.push(sock); // hold it open, send nothing
                    continue;
                }
                let body = r#"{"object":"list","data":[{"id":"m1"}]}"#;
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        let http = reqwest::Client::new();
        // A short injected deadline: the production 20s would just make this test slow, and what
        // matters is that the deadline is enforced at all and classified as inconclusive.
        let got = check_endpoint_with_deadline(
            &http,
            &base,
            Some("sk-unknown"),
            std::time::Duration::from_millis(300),
        )
        .await;
        srv.abort();
        assert!(
            matches!(got, EndpointCheck::Ok(_)),
            "a stalled probe is inconclusive, not a rejection, got {got:?}"
        );
    }

    /// With no key there is nothing to certify, so the base-URL-only check must stay a single request
    /// and keep classifying purely on the model list.
    #[tokio::test]
    async fn base_url_only_check_skips_the_chat_probe() {
        let (base, srv) = stub_gateway(200, 401).await;
        let http = reqwest::Client::new();
        let got = check_endpoint(&http, &base, None).await;
        srv.abort();
        assert!(
            matches!(got, EndpointCheck::Ok(_)),
            "no key ⇒ reachability only, got {got:?}"
        );
    }
}
