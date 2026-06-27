//! OpenAI-compatible streaming chat client — the "call API like hermes" layer.
//! POST {base_url}/chat/completions with Bearer auth, parse the SSE stream, emit content deltas.

use anyhow::{anyhow, bail, Context, Result};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::Deserialize;
use std::io::Write;

use crate::types::{
    CacheControl, ChatChunk, ChatRequest, ChatResponse, FunctionCall, Message, StreamOptions, ToolCall,
    ToolCallDelta, ToolDef, Usage,
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
}

static COST_METER: CostMeter = CostMeter {
    prompt_tokens: AtomicU64::new(0),
    completion_tokens: AtomicU64::new(0),
    calls_with_usage: AtomicU64::new(0),
    cache_read_tokens: AtomicU64::new(0),
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
    ["opus", "sonnet", "haiku", "fable"].iter().any(|k| contains_word(&m, k))
}

/// `haystack` contains `needle` delimited by non-alphanumeric boundaries (so `-`, `_`, `.`, space
/// count as separators but an adjacent letter/digit does not). ASCII-only (model ids are ASCII).
fn contains_word(haystack: &str, needle: &str) -> bool {
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

/// Stamp ephemeral cache breakpoints (≤3 of Anthropic's 4): the LAST tool def (caches the whole
/// tool block), the system message (caches tools+system), and the last stable assistant/tool message
/// before the newest turn (caches the prior conversation). The volatile newest user turn stays
/// uncached. Computed per call, never stored, so mid-history `system` nudges don't disturb it.
fn apply_cache_breakpoints(messages: &mut [Message], tools: &mut [ToolDef]) {
    if let Some(last) = tools.last_mut() {
        last.cache_control = Some(CacheControl::ephemeral());
    }
    if let Some(first) = messages.first_mut() {
        if first.role == "system" {
            first.cache_control = Some(CacheControl::ephemeral());
        }
    }
    let n = messages.len();
    if n >= 3 {
        if let Some(idx) = (0..n).rev().find(|&i| matches!(messages[i].role.as_str(), "assistant" | "tool")) {
            if idx != 0 {
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
        self.prompt_tokens.fetch_add(u.prompt_tokens.unwrap_or(0), Ordering::Relaxed);
        self.completion_tokens.fetch_add(u.completion_tokens.unwrap_or(0), Ordering::Relaxed);
        self.cache_read_tokens.fetch_add(u.cache_read(), Ordering::Relaxed);
        self.calls_with_usage.fetch_add(1, Ordering::Relaxed);
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
    }
}

/// Fetch the provider's advertised model ids (`GET {base}/models`). Used by `ng models` and to
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

/// Fetch the provider's model list with context windows when available (see [`ModelInfo`]).
pub async fn fetch_models_info(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
) -> Result<Vec<ModelInfo>> {
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
        // OpenRouter nests it here too: `top_provider.context_length`.
        #[serde(default)]
        top_provider: Option<TopProvider>,
    }

    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = client
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;
    let status = resp.status();
    if !status.is_success() {
        let detail = resp.text().await.unwrap_or_default();
        bail!("upstream returned HTTP {status}: {detail}");
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
                .or_else(|| m.top_provider.and_then(|t| t.context_length))
                .filter(|&n| n > 0);
            ModelInfo { id: m.id, context_length: ctx }
        })
        .collect())
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
}

/// Stream a chat completion. Prints content deltas to stdout as they arrive and
/// returns the full concatenated assistant text. Returns a typed error on a non-2xx
/// response (so the caller can decide retry/stop) instead of panicking.
pub async fn stream_chat(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: Vec<Message>,
) -> Result<String> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let body = ChatRequest {
        model: model.to_string(),
        messages,
        stream: true,
        temperature: None,
        max_tokens: crate::cli_config::load().max_tokens,
        tools: Vec::new(),
        tool_choice: None,
        parallel_tool_calls: None,
        stream_options: None,
    };

    let mut spin = Some(crate::spinner::Spinner::start("thinking"));

    let resp = client
        .post(&url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;

    let status = resp.status();
    if !status.is_success() {
        spin.take();
        let detail = resp.text().await.unwrap_or_default();
        bail!("upstream returned HTTP {status}: {detail}");
    }

    let mut stream = resp.bytes_stream().eventsource();
    let mut full = String::new();
    let mut stdout = std::io::stdout();

    while let Some(event) = stream.next().await {
        let event = event.map_err(|e| anyhow!("SSE stream error: {e}"))?;

        // OpenAI signals end-of-stream with a literal `data: [DONE]`.
        if event.data.trim() == "[DONE]" {
            break;
        }
        if event.data.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<ChatChunk>(&event.data) {
            Ok(chunk) => {
                if let Some(choice) = chunk.choices.first() {
                    if let Some(content) = &choice.delta.content {
                        spin.take(); // clear before the first token prints
                        print!("{content}");
                        let _ = stdout.flush();
                        full.push_str(content);
                    }
                }
            }
            // Tolerate keepalive / non-JSON frames rather than aborting the whole turn.
            Err(e) => {
                spin.take();
                eprintln!("\n[warn] unparseable stream frame ({e}): {}", event.data);
            }
        }
    }

    spin.take();
    println!();
    Ok(full)
}

/// One non-streaming chat turn WITH tools advertised. Returns the assistant's content
/// and/or the tool calls it wants executed. Used by the `task` sub-agent (which runs silently
/// and returns only its final text) and the workflow fan-out; the streaming counterpart
/// (`stream_chat_with_tools`) drives the top-level `ng agent` for live output. Both return
/// `ChatTurn`, so the loop is agnostic.
pub async fn chat_with_tools(
    client: &reqwest::Client,
    base_url: &str,
    api_key: &str,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
) -> Result<ChatTurn> {
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let cfg = crate::cli_config::load();
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
        tool_choice: if tools.is_empty() { None } else { Some("auto".to_string()) },
        parallel_tool_calls: if tools.is_empty() { None } else { Some(true) },
        stream_options: None, // non-streaming responses carry `usage` natively
    };

    let resp = client
        .post(&url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;

    let status = resp.status();
    if !status.is_success() {
        let detail = resp.text().await.unwrap_or_default();
        bail!("upstream returned HTTP {status}: {detail}");
    }

    let parsed: ChatResponse = resp.json().await.context("parsing chat-completions response")?;
    if let Some(u) = &parsed.usage {
        cost_meter().record(u);
    }
    let choice = parsed
        .choices
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("response had no choices"))?;
    Ok(ChatTurn {
        content: choice.message.content,
        tool_calls: choice.message.tool_calls,
        finish_reason: choice.finish_reason,
    })
}

/// Reassembles streamed `delta.tool_calls[]` fragments into whole `ToolCall`s, keyed by index.
/// `id`/`name` land once (first fragment for an index); `arguments` pieces concatenate.
#[derive(Default)]
pub struct ToolCallAccumulator {
    calls: BTreeMap<usize, AccCall>,
}

#[derive(Default)]
struct AccCall {
    id: String,
    name: String,
    args: String,
}

impl ToolCallAccumulator {
    pub fn ingest(&mut self, deltas: &[ToolCallDelta]) {
        for d in deltas {
            let e = self.calls.entry(d.index).or_default();
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
    }

    pub fn finish(self) -> Vec<ToolCall> {
        self.calls
            .into_values()
            .filter(|c| !c.name.is_empty())
            .enumerate()
            .map(|(i, c)| ToolCall {
                id: if c.id.is_empty() { format!("call_{i}") } else { c.id },
                kind: "function".to_string(),
                function: FunctionCall { name: c.name, arguments: c.args },
            })
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
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let cfg = crate::cli_config::load();
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
        tool_choice: if tools.is_empty() { None } else { Some("auto".to_string()) },
        parallel_tool_calls: if tools.is_empty() { None } else { Some(true) },
        stream_options: Some(StreamOptions { include_usage: true }), // ask for a final usage chunk
    };

    // Spinner during the "thinking" gap: from request send until the first token / tool delta
    // streams back. TTY-only (silent no-op on pipes/CI). Cleared before any output is printed.
    // Suppressed under the sticky TUI — its box shows the "⚡ working…" indicator instead, and a
    // carriage-return spinner would fight the pinned footer.
    let mut spin = if crate::tui::active() { None } else { Some(crate::spinner::Spinner::start("thinking")) };

    let resp = client
        .post(&url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("request to {url} failed"))?;
    let status = resp.status();
    if !status.is_success() {
        spin.take(); // clear the spinner before surfacing the error
        let detail = resp.text().await.unwrap_or_default();
        bail!("upstream returned HTTP {status}: {detail}");
    }

    let mut stream = resp.bytes_stream().eventsource();
    let mut full = String::new();
    let mut acc = ToolCallAccumulator::default();
    let mut finish_reason: Option<String> = None;
    let mut think = ThinkFilter::default(); // suppress `<think>…</think>` reasoning from the display
    // Render Markdown for the DISPLAY only (history keeps the raw text via `full`). Decorate when
    // we own an interactive terminal (sticky TUI or a TTY one-shot); pipes/CI pass through verbatim.
    let decorate = crate::tui::active() || std::io::IsTerminal::is_terminal(&std::io::stdout());
    let cols = crate::tui::width(); // wrap to the box width (not a separately-probed window edge)
    let mut md = crate::markdown::MarkdownStream::new(decorate, cols);

    while let Some(event) = stream.next().await {
        let event = event.map_err(|e| anyhow!("SSE stream error: {e}"))?;
        if event.data.trim() == "[DONE]" {
            break;
        }
        if event.data.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<ChatChunk>(&event.data) {
            Ok(chunk) => {
                // The final chunk (choices empty) carries real token usage when we asked for it.
                if let Some(u) = &chunk.usage {
                    cost_meter().record(u);
                }
                if let Some(choice) = chunk.choices.first() {
                    // A dedicated reasoning channel (`reasoning_content`/`reasoning`) is the model
                    // thinking out loud — suppress it entirely so output is uniform across models.
                    // (Clear the spinner: the model IS producing, just not user-facing text yet.)
                    if choice.delta.reasoning_content.is_some() {
                        spin.take();
                    }
                    if let Some(content) = &choice.delta.content {
                        spin.take(); // stop+clear the spinner before the first token prints
                        let shown = think.push(content);
                        if !shown.is_empty() {
                            full.push_str(&shown); // history keeps the RAW markdown
                            let rendered = md.push(&shown); // styled, complete lines (gutter, md, code)
                            if !rendered.is_empty() {
                                crate::tui::emit(&rendered); // sticky TUI funnel (plain print! when inactive)
                            }
                        }
                    }
                    if !choice.delta.tool_calls.is_empty() {
                        spin.take(); // a tool-only turn: clear before the loop prints tool traces
                        acc.ingest(&choice.delta.tool_calls);
                    }
                    if choice.finish_reason.is_some() {
                        finish_reason = choice.finish_reason.clone();
                    }
                }
            }
            Err(e) => {
                spin.take();
                eprintln!("\n[warn] unparseable stream frame ({e}): {}", event.data);
            }
        }
    }
    spin.take(); // stream ended (e.g. empty turn) — ensure the spinner is gone
    let tail = think.finish();
    if !tail.is_empty() {
        full.push_str(&tail);
        let rendered = md.push(&tail);
        if !rendered.is_empty() {
            crate::tui::emit(&rendered);
        }
    }
    let closing = md.finish(); // flush the final partial line + close any dangling code fence
    if !closing.is_empty() {
        crate::tui::emit(&closing);
    }
    if !full.is_empty() {
        crate::tui::emit("\n"); // one blank line of breathing room before the next turn
    }

    Ok(ChatTurn {
        content: if full.is_empty() { None } else { Some(full) },
        tool_calls: acc.finish(),
        finish_reason,
    })
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
    use crate::types::FunctionDelta;

    #[test]
    fn cost_meter_sums_real_usage_and_ignores_empty() {
        // A LOCAL meter (not the global) so parallel tests don't race on it.
        let m = CostMeter::default();
        m.record(&Usage { prompt_tokens: Some(100), completion_tokens: Some(40), total_tokens: Some(140), ..Default::default() });
        m.record(&Usage { prompt_tokens: Some(10), completion_tokens: Some(5), total_tokens: None, ..Default::default() });
        m.record(&Usage::default()); // all-None → NOT counted as a usage-reporting call
        assert_eq!(m.snapshot(), (110, 45, 2));
        m.reset();
        assert_eq!(m.snapshot(), (0, 0, 0));
    }

    #[test]
    fn cost_meter_tracks_cache_reads() {
        let m = CostMeter::default();
        m.record(&Usage { prompt_tokens: Some(100), cache_read_input_tokens: Some(80), ..Default::default() });
        assert_eq!(m.cache_read(), 80, "cache-read tokens accumulate for the /cost probe");
        m.reset();
        assert_eq!(m.cache_read(), 0);
    }

    #[test]
    fn anthropic_model_detection() {
        for m in ["opus-4-8", "claude-3-5", "Sonnet", "fable-5", "anthropic/claude", "claude-haiku-4-5"] {
            assert!(is_anthropic_model(m), "{m} should be detected as Anthropic");
        }
        for m in ["gpt-4o", "deepseek-chat", "gemini-1.5", "llama-3"] {
            assert!(!is_anthropic_model(m), "{m} should NOT be Anthropic");
        }
        // Word-boundary guard: community models that merely EMBED a token must not trip AUTO cache.
        for m in ["fable13b", "haikuwriter", "mythos-13b", "opusculum-7b", "sonnetizer"] {
            assert!(!is_anthropic_model(m), "{m} embeds a token but is not Anthropic");
        }
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
        assert!(tools[0].cache_control.is_none(), "only the LAST tool def is stamped");
        assert!(tools[1].cache_control.is_some(), "last tool def caches the whole tool block");
        assert!(msgs[0].cache_control.is_some(), "system message stamped");
        assert!(msgs[3].cache_control.is_some(), "last stable assistant/tool message stamped");
        assert!(msgs[4].cache_control.is_none(), "the newest user turn stays uncached");
    }

    #[test]
    fn cache_breakpoints_short_history_only_system_and_tools() {
        let mut msgs = vec![Message::system("sys"), Message::user("hi")];
        let mut tools = vec![ToolDef::function("a", "d", serde_json::json!({"type":"object"}))];
        apply_cache_breakpoints(&mut msgs, &mut tools);
        assert!(msgs[0].cache_control.is_some());
        assert!(tools[0].cache_control.is_some());
        assert!(msgs[1].cache_control.is_none(), "no history breakpoint when n < 3");
    }

    fn delta(index: usize, id: Option<&str>, name: Option<&str>, args: Option<&str>) -> ToolCallDelta {
        ToolCallDelta {
            index,
            id: id.map(String::from),
            function: Some(FunctionDelta { name: name.map(String::from), arguments: args.map(String::from) }),
        }
    }

    #[test]
    fn accumulator_reassembles_streamed_tool_call() {
        let mut acc = ToolCallAccumulator::default();
        acc.ingest(&[delta(0, Some("call_1"), Some("memory_search"), Some("{\"que"))]);
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
        acc.ingest(&[delta(0, Some("a"), Some("f"), Some("{}")), delta(1, Some("b"), Some("g"), Some("{}"))]);
        let calls = acc.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].function.name, "f");
        assert_eq!(calls[1].function.name, "g");
    }

    #[test]
    fn delta_captures_both_reasoning_field_names() {
        // DeepSeek-style `reasoning_content` and OpenRouter-style `reasoning` both land in the field.
        let a: crate::types::Delta = serde_json::from_str(r#"{"reasoning_content":"thinking…"}"#).unwrap();
        assert_eq!(a.reasoning_content.as_deref(), Some("thinking…"));
        let b: crate::types::Delta = serde_json::from_str(r#"{"reasoning":"thinking…"}"#).unwrap();
        assert_eq!(b.reasoning_content.as_deref(), Some("thinking…"));
        // Plain content chunk leaves the reasoning channel empty.
        let c: crate::types::Delta = serde_json::from_str(r#"{"content":"hi"}"#).unwrap();
        assert!(c.reasoning_content.is_none());
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
}
