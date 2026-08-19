//! ChatGPT Codex Responses API client (experimental).
//!
//! Maps Aizen's chat/tools types onto `POST …/codex/responses` SSE and back into [`ChatTurn`].
//! Not the OpenAI Platform Responses API — field allowlists and headers follow the Codex CLI
//! compatibility surface and may change without notice.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::time::Duration;

use crate::core::types::{FunctionCall, Message, ToolCall, ToolDef, Usage};
use crate::llm::client::ChatTurn;
use crate::llm::codex_models;
use crate::llm::oauth_codex::{self, CODEX_RESPONSES_URL};

const ORIGINATOR: &str = "codex_cli_rs";
const CODEX_UA: &str = "codex_cli_rs/0.136.0";
const OVERLOAD_MARKERS: &[&str] = &["server_is_overloaded", "service_unavailable_error"];
const CAPACITY_MARKERS: &[&str] = &["selected model is at capacity", "model_at_capacity"];

const ALLOWLIST: &[&str] = &[
    "model",
    "input",
    "instructions",
    "tools",
    "tool_choice",
    "stream",
    "store",
    "reasoning",
    "service_tier",
    "include",
    "prompt_cache_key",
    "client_metadata",
    "text",
];

/// Build Codex Responses JSON body from Aizen messages/tools.
pub fn build_request_body(
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    session_id: &str,
    instructions: Option<&str>,
) -> Value {
    let (base_model, suffix_effort) = codex_models::strip_effort_suffix(model);
    let mut input: Vec<Value> = Vec::new();
    let mut sys_bits: Vec<String> = Vec::new();

    for m in messages {
        let role = m.role.as_str();
        match role {
            "system" => {
                if let Some(c) = m.content.as_deref() {
                    if !c.is_empty() {
                        sys_bits.push(c.to_string());
                    }
                }
            }
            "tool" => {
                let call_id = m.tool_call_id.clone().unwrap_or_default();
                let text = m.content.clone().unwrap_or_default();
                input.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": text,
                }));
            }
            "assistant" if !m.tool_calls.is_empty() => {
                if let Some(c) = m.content.as_deref() {
                    if !c.is_empty() {
                        input.push(message_item("assistant", c));
                    }
                }
                for tc in &m.tool_calls {
                    input.push(json!({
                        "type": "function_call",
                        "call_id": tc.id,
                        "name": tc.function.name,
                        "arguments": tc.function.arguments,
                    }));
                }
            }
            "assistant" | "user" | "developer" => {
                let role = if role == "assistant" {
                    "assistant"
                } else if role == "developer" {
                    "developer"
                } else {
                    "user"
                };
                let text = m.content.clone().unwrap_or_default();
                // Images: pass data URLs as input_image when present on user turns.
                if role == "user" && !m.images.is_empty() {
                    let mut content = Vec::new();
                    if !text.is_empty() {
                        content.push(json!({"type": "input_text", "text": text}));
                    }
                    for img in &m.images {
                        content.push(
                            json!({"type": "input_image", "image_url": img, "detail": "auto"}),
                        );
                    }
                    input.push(json!({"type": "message", "role": "user", "content": content}));
                } else if !text.is_empty() || role == "user" {
                    input.push(message_item(role, &text));
                }
            }
            _ => {
                if let Some(c) = m.content.as_deref() {
                    if !c.is_empty() {
                        input.push(message_item("user", c));
                    }
                }
            }
        }
    }

    if input.is_empty() {
        input.push(message_item("user", "..."));
    }

    let instr = if let Some(i) = instructions {
        i.to_string()
    } else if !sys_bits.is_empty() {
        sys_bits.join("\n\n")
    } else {
        default_instructions().to_string()
    };

    // If we already folded system into instructions, also keep a developer message for cache
    // locality when there were system bits — optional; instructions alone is enough for Codex.
    let _ = sys_bits;

    let mut body = json!({
        "model": base_model,
        "input": input,
        "instructions": instr,
        "stream": true,
        "store": false,
        "prompt_cache_key": session_id,
    });

    if !tools.is_empty() {
        let mut codex_tools = Vec::new();
        let mut names = BTreeSet::new();
        for t in tools {
            let name = t.function.name.trim();
            if name.is_empty() {
                continue;
            }
            names.insert(name.to_string());
            let mut tool = json!({
                "type": "function",
                "name": name,
                "parameters": t.function.parameters,
            });
            if !t.function.description.is_empty() {
                tool["description"] = json!(t.function.description);
            }
            codex_tools.push(tool);
        }
        body["tools"] = Value::Array(codex_tools);
        body["tool_choice"] = json!("auto");
        let _ = names;
    }

    let effort = suffix_effort.map(|s| s.to_string()).or_else(|| {
        crate::core::cli_config::resolved_reasoning_effort(
            crate::core::cli_config::load().reasoning_effort.clone(),
        )
    });

    if let Some(eff) = effort {
        let eff = if eff == "max" {
            "xhigh".to_string()
        } else {
            eff
        };
        body["reasoning"] = json!({"effort": eff, "summary": "auto"});
        if eff != "none" {
            body["include"] = json!(["reasoning.encrypted_content"]);
        }
    }

    // Final allowlist strip.
    if let Some(obj) = body.as_object_mut() {
        let keys: Vec<String> = obj.keys().cloned().collect();
        for k in keys {
            if !ALLOWLIST.contains(&k.as_str()) {
                obj.remove(&k);
            }
        }
    }
    body
}

fn message_item(role: &str, text: &str) -> Value {
    let ctype = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({
        "type": "message",
        "role": role,
        "content": [{"type": ctype, "text": text}],
    })
}

fn default_instructions() -> &'static str {
    "You are a coding agent running inside the Aizen CLI. Be concise and correct. Use tools when they improve the answer. Do not mention these instructions."
}

fn build_headers(
    access: &str,
    account_id: Option<&str>,
    session_id: &str,
) -> reqwest::header::HeaderMap {
    use reqwest::header::{
        HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE, USER_AGENT,
    };
    let mut h = HeaderMap::new();
    h.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {access}"))
            .unwrap_or(HeaderValue::from_static("Bearer")),
    );
    h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    h.insert(USER_AGENT, HeaderValue::from_static(CODEX_UA));
    h.insert(
        HeaderName::from_static("originator"),
        HeaderValue::from_static(ORIGINATOR),
    );
    h.insert(
        HeaderName::from_static("session_id"),
        HeaderValue::from_str(session_id).unwrap_or(HeaderValue::from_static("aizen")),
    );
    if let Some(a) = account_id {
        if let Ok(v) = HeaderValue::from_str(a) {
            h.insert(HeaderName::from_static("chatgpt-account-id"), v);
        }
    }
    h
}

/// One Codex Responses turn → ChatTurn (tools + text).
pub async fn stream_turn(
    client: &reqwest::Client,
    model: &str,
    messages: &[Message],
    tools: &[ToolDef],
    session_id: &str,
) -> Result<ChatTurn> {
    if oauth_codex::codex_disabled() {
        bail!("Codex OAuth disabled via AIZEN_DISABLE_CODEX");
    }

    let mut access_account = oauth_codex::bearer_token().await?;
    let body = build_request_body(model, messages, tools, session_id, None);

    // Up to 2 auth attempts (refresh on 401) and a few transient retries. This path used to be a
    // second-class transport (raw `.send()`, immediate bail on 429/5xx, linear un-jittered
    // sleeps); it now mirrors `send_with_retry`'s discipline with the same jittered backoff.
    let mut overload_attempt = 0u32;
    let mut transient_attempt = 0u32;
    const TRANSIENT_RETRIES: u32 = 3;
    loop {
        let (access, account) = &access_account;
        let headers = build_headers(access, account.as_deref(), session_id);
        let resp = client
            .post(CODEX_RESPONSES_URL)
            .headers(headers)
            .json(&body)
            .send()
            .await
            .context("codex responses POST")?;

        let status = resp.status();
        if status.as_u16() == 401 {
            // Force refresh once.
            if let Some(set) = oauth_codex::load_token() {
                match oauth_codex::refresh_token(&set).await {
                    oauth_codex::RefreshOutcome::Ok(s) => {
                        access_account = (s.access_token, s.account_id);
                        continue;
                    }
                    oauth_codex::RefreshOutcome::ReauthRequired(m) => {
                        bail!("Codex auth failed (re-login): {m}");
                    }
                    oauth_codex::RefreshOutcome::Transient(m) => {
                        bail!("Codex auth failed: {m}");
                    }
                }
            }
            bail!("Codex HTTP 401 — run `aizen auth login codex`");
        }
        if status.as_u16() == 429 {
            let text = resp.text().await.unwrap_or_default();
            // A spent QUOTA is permanent for this window — retrying burns nothing but time.
            if let Some(msg) = parse_usage_limit(&text) {
                bail!("{msg}");
            }
            // Plain rate limiting is transient: back off and retry like every other gateway.
            if transient_attempt < TRANSIENT_RETRIES {
                let delay = crate::llm::client::backoff_ms(transient_attempt, 500, 15_000);
                transient_attempt += 1;
                tokio::time::sleep(Duration::from_millis(delay)).await;
                continue;
            }
            bail!("Codex rate limited (HTTP 429): {}", truncate(&text, 300));
        }
        if !status.is_success() {
            let code = status.as_u16();
            let text = resp.text().await.unwrap_or_default();
            if crate::llm::client::is_retryable_status(code)
                && transient_attempt < TRANSIENT_RETRIES
            {
                let delay = crate::llm::client::backoff_ms(transient_attempt, 500, 15_000);
                transient_attempt += 1;
                tokio::time::sleep(Duration::from_millis(delay)).await;
                continue;
            }
            bail!("Codex HTTP {status}: {}", truncate(&text, 500));
        }

        // Read SSE body fully (Codex forces stream). For v1 we buffer the stream; blank-stream
        // retry at higher layer can still re-call stream_turn.
        let bytes = resp.bytes().await.context("reading codex SSE body")?;
        let text = String::from_utf8_lossy(&bytes);
        let lower = text.to_ascii_lowercase();

        if CAPACITY_MARKERS.iter().any(|m| lower.contains(m)) {
            bail!("Selected model is at capacity. Try a different Codex model.");
        }
        if OVERLOAD_MARKERS.iter().any(|m| lower.contains(m)) {
            if overload_attempt >= 3 {
                bail!("Codex upstream overloaded — retries exhausted");
            }
            overload_attempt += 1;
            let delay = crate::llm::client::backoff_ms(overload_attempt - 1, 1_000, 15_000);
            tokio::time::sleep(Duration::from_millis(delay)).await;
            continue;
        }

        let turn = parse_sse_to_chat_turn(&text)?;
        // Codex turns used to be invisible to `/cost` and the cache HUD — the one chat path that
        // never recorded usage. Same meter as the OpenAI-dialect paths.
        if let Some(u) = &turn.usage {
            crate::llm::client::cost_meter().record(u);
        }
        return Ok(turn);
    }
}

fn parse_usage_limit(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    let err = v.get("error")?;
    if err.get("type").and_then(|t| t.as_str()) != Some("usage_limit_reached") {
        return None;
    }
    let msg = err
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or("Codex usage limit reached");
    if let Some(ts) = err.get("resets_at").and_then(|x| x.as_i64()) {
        return Some(format!("{msg} (resets_at={ts})"));
    }
    if let Some(s) = err.get("resets_in_seconds").and_then(|x| x.as_u64()) {
        return Some(format!("{msg} (resets in {s}s)"));
    }
    Some(msg.to_string())
}

/// Parse Codex Responses SSE into a ChatTurn.
pub fn parse_sse_to_chat_turn(sse: &str) -> Result<ChatTurn> {
    let mut text_out = String::new();
    // call_id -> (name, arguments)
    let mut tools: std::collections::BTreeMap<String, (String, String)> =
        std::collections::BTreeMap::new();
    let mut finish: Option<String> = None;
    let mut usage: Option<Usage> = None;
    let mut err_msg: Option<String> = None;

    for raw_line in sse.lines() {
        let line = raw_line.trim_end();
        if !line.starts_with("data:") {
            continue;
        }
        let data = line.trim_start_matches("data:").trim();
        if data.is_empty() || data == "[DONE]" {
            continue;
        }
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Nested error objects
        if let Some(msg) = nested_error_message(&v) {
            err_msg = Some(msg);
        }

        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");

        match ty {
            "response.output_text.delta" => {
                if let Some(d) = v.get("delta").and_then(|d| d.as_str()) {
                    text_out.push_str(d);
                } else if let Some(d) = v.pointer("/delta/text").and_then(|d| d.as_str()) {
                    text_out.push_str(d);
                }
            }
            "response.output_text.done" => {
                if let Some(t) = v.get("text").and_then(|t| t.as_str()) {
                    if text_out.is_empty() {
                        text_out.push_str(t);
                    }
                }
            }
            "response.function_call_arguments.delta" => {
                let id = v
                    .get("call_id")
                    .or_else(|| v.get("item_id"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("call")
                    .to_string();
                let name = v
                    .get("name")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let delta = v.get("delta").and_then(|x| x.as_str()).unwrap_or("");
                let e = tools
                    .entry(id)
                    .or_insert_with(|| (name.clone(), String::new()));
                if e.0.is_empty() && !name.is_empty() {
                    e.0 = name;
                }
                e.1.push_str(delta);
            }
            "response.function_call_arguments.done" | "response.output_item.done" => {
                // Full item may carry name/arguments/call_id
                if let Some(item) = v.get("item") {
                    ingest_output_item(item, &mut tools, &mut text_out);
                }
                if ty == "response.function_call_arguments.done" {
                    let id = v
                        .get("call_id")
                        .and_then(|x| x.as_str())
                        .unwrap_or("call")
                        .to_string();
                    let name = v
                        .get("name")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    let args = v
                        .get("arguments")
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !args.is_empty() || !name.is_empty() {
                        let e = tools
                            .entry(id)
                            .or_insert_with(|| (name.clone(), String::new()));
                        if e.0.is_empty() {
                            e.0 = name;
                        }
                        if e.1.is_empty() {
                            e.1 = args;
                        }
                    }
                }
            }
            "response.output_item.added" => {
                if let Some(item) = v.get("item") {
                    ingest_output_item(item, &mut tools, &mut text_out);
                }
            }
            "response.completed" => {
                finish = Some("stop".into());
                if let Some(u) = v.pointer("/response/usage") {
                    usage = parse_usage(u);
                } else if let Some(u) = v.get("usage") {
                    usage = parse_usage(u);
                }
                if let Some(out) = v.pointer("/response/output") {
                    if let Some(arr) = out.as_array() {
                        for item in arr {
                            ingest_output_item(item, &mut tools, &mut text_out);
                        }
                    }
                }
            }
            "response.failed" | "error" => {
                if let Some(msg) = nested_error_message(&v) {
                    err_msg = Some(msg);
                } else {
                    err_msg = Some(format!("codex {ty}"));
                }
            }
            _ => {
                // Some gateways put output text under response.output without type prefixing every delta.
                if let Some(out) = v.pointer("/response/output") {
                    if let Some(arr) = out.as_array() {
                        for item in arr {
                            ingest_output_item(item, &mut tools, &mut text_out);
                        }
                    }
                }
            }
        }
    }

    if let Some(e) = err_msg {
        // A failure event fails the turn unless the stream ALSO completed normally afterwards
        // (`response.completed` sets `finish`). The old gate only bailed when text AND tools were
        // both empty, so a stream cut down mid-answer returned its partial text as a clean
        // `finish_reason: "stop"` turn — the caller could not tell a finished answer from a
        // truncated one. An error the turn recovered from (completed anyway) is still fine.
        if finish.is_none() {
            bail!("Codex error: {e}");
        }
    }

    let tool_calls: Vec<ToolCall> = tools
        .into_iter()
        .map(|(id, (name, arguments))| ToolCall {
            id,
            kind: "function".into(),
            function: FunctionCall {
                name,
                arguments: if arguments.is_empty() {
                    "{}".into()
                } else {
                    arguments
                },
            },
        })
        .collect();

    if !tool_calls.is_empty() {
        finish = Some("tool_calls".into());
    }

    Ok(ChatTurn {
        content: if text_out.is_empty() {
            None
        } else {
            Some(text_out)
        },
        tool_calls,
        finish_reason: finish,
        usage,
        eager: Default::default(),
    })
}

fn ingest_output_item(
    item: &Value,
    tools: &mut std::collections::BTreeMap<String, (String, String)>,
    text_out: &mut String,
) {
    let ty = item.get("type").and_then(|t| t.as_str()).unwrap_or("");
    match ty {
        "message" => {
            if let Some(content) = item.get("content").and_then(|c| c.as_array()) {
                for part in content {
                    let pt = part.get("type").and_then(|t| t.as_str()).unwrap_or("");
                    if pt == "output_text" || pt == "text" {
                        if let Some(t) = part.get("text").and_then(|t| t.as_str()) {
                            if text_out.is_empty() {
                                text_out.push_str(t);
                            }
                        }
                    }
                }
            }
        }
        "function_call" => {
            let id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(|x| x.as_str())
                .unwrap_or("call")
                .to_string();
            let name = item
                .get("name")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let args = item
                .get("arguments")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string();
            let e = tools
                .entry(id)
                .or_insert_with(|| (name.clone(), String::new()));
            if e.0.is_empty() {
                e.0 = name;
            }
            if e.1.is_empty() {
                e.1 = args;
            }
        }
        _ => {}
    }
}

fn parse_usage(u: &Value) -> Option<Usage> {
    let mut usage = Usage::default();
    usage.prompt_tokens = u
        .get("input_tokens")
        .or_else(|| u.get("prompt_tokens"))
        .and_then(|x| x.as_u64());
    usage.completion_tokens = u
        .get("output_tokens")
        .or_else(|| u.get("completion_tokens"))
        .and_then(|x| x.as_u64());
    usage.total_tokens = u.get("total_tokens").and_then(|x| x.as_u64());
    // The Responses dialect reports cached prompt tokens under `input_tokens_details`; keep the
    // flat spelling too for gateways that mirror the Anthropic shape.
    usage.cache_read_input_tokens = u
        .get("cache_read_input_tokens")
        .and_then(|x| x.as_u64())
        .or_else(|| {
            u.pointer("/input_tokens_details/cached_tokens")
                .and_then(|x| x.as_u64())
        });
    Some(usage)
}

fn nested_error_message(v: &Value) -> Option<String> {
    if let Some(m) = v.pointer("/error/message").and_then(|m| m.as_str()) {
        return Some(m.to_string());
    }
    if let Some(m) = v.get("message").and_then(|m| m.as_str()) {
        if v.get("type").and_then(|t| t.as_str()) == Some("error") {
            return Some(m.to_string());
        }
    }
    None
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let t: String = s.chars().take(n).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::types::{FunctionDef, ToolDef};

    #[test]
    fn build_body_maps_tools_and_strips_disallowed() {
        let msgs = vec![Message::user("hi")];
        let tools = vec![ToolDef {
            kind: "function".into(),
            function: FunctionDef {
                name: "read_file".into(),
                description: "read".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
            cache_control: None,
        }];
        let body = build_request_body("gpt-5.4-mini-high", &msgs, &tools, "sess", None);
        assert_eq!(body["model"], "gpt-5.4-mini");
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert!(body.get("temperature").is_none());
        assert!(body.get("max_tokens").is_none());
        assert_eq!(body["reasoning"]["effort"], "high");
        assert!(body["tools"].as_array().unwrap().len() == 1);
        assert_eq!(body["tools"][0]["name"], "read_file");
        assert_eq!(body["prompt_cache_key"], "sess");
    }

    #[test]
    fn parse_text_delta_sse() {
        let sse = "\
event: response.output_text.delta\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n\n\
event: response.output_text.delta\n\
data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\n\n\
event: response.completed\n\
data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n\
";
        let turn = parse_sse_to_chat_turn(sse).unwrap();
        assert_eq!(turn.content.as_deref(), Some("Hello"));
        assert!(turn.tool_calls.is_empty());
        assert_eq!(turn.usage.unwrap().prompt_tokens, Some(3));
    }

    #[test]
    fn parse_function_call_sse() {
        let sse = r#"
data: {"type":"response.function_call_arguments.delta","call_id":"c1","name":"shell","delta":"{\"cmd\":"}
data: {"type":"response.function_call_arguments.delta","call_id":"c1","delta":"\"ls\"}"}
data: {"type":"response.function_call_arguments.done","call_id":"c1","name":"shell","arguments":"{\"cmd\":\"ls\"}"}
data: {"type":"response.completed","response":{"output":[]}}
"#;
        let turn = parse_sse_to_chat_turn(sse).unwrap();
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].function.name, "shell");
        assert!(turn.tool_calls[0].function.arguments.contains("ls"));
    }

    #[test]
    fn system_becomes_instructions() {
        let msgs = vec![Message::system("be brief"), Message::user("hi")];
        let body = build_request_body("gpt-5.4", &msgs, &[], "s", None);
        assert_eq!(body["instructions"], "be brief");
        let input = body["input"].as_array().unwrap();
        assert!(input
            .iter()
            .all(|i| i.get("role").and_then(|r| r.as_str()) != Some("system")));
    }
}
