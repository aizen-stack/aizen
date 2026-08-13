//! ChatGPT-backed OpenAI Codex transport.
//!
//! ChatGPT Plus/Pro access is not an API key. Codex signs the user in with OpenAI OAuth, then sends
//! Responses API traffic to `chatgpt.com/backend-api/codex` with the OAuth access token and the
//! selected ChatGPT account id. This module mirrors that protocol without depending on Codex itself.

use anyhow::{anyhow, bail, Context, Result};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::core::types::{FunctionCall, Message, ToolCall, ToolDef, Usage};
use super::legacy_client::{ChatTurn, ModelInfo};

pub const CHATGPT_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const OAUTH_ISSUER: &str = "https://auth.openai.com";
const OAUTH_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const OAUTH_SCOPE: &str = "openid profile email offline_access";
const AUTH_TIMEOUT: Duration = Duration::from_secs(300);
const EXPIRY_MARGIN_SECS: i64 = 60;

pub fn is_chatgpt_base(base_url: &str) -> bool {
    url::Url::parse(base_url)
        .ok()
        .and_then(|u| {
            let host = u.host_str()?.to_ascii_lowercase();
            Some(host == "chatgpt.com" && u.path().trim_end_matches('/') == "/backend-api/codex")
        })
        .unwrap_or(false)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredAuth {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    id_token: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    expires_at: Option<i64>,
}

fn auth_path() -> std::path::PathBuf {
    crate::core::config::aizen_home().join("openai-auth.json")
}

fn load_auth() -> Option<StoredAuth> {
    serde_json::from_slice(&std::fs::read(auth_path()).ok()?).ok()
}

fn save_auth(auth: &StoredAuth) -> Result<()> {
    let mut bytes = serde_json::to_vec_pretty(auth)?;
    bytes.push(b'\n');
    crate::core::persist::atomic_write_owner_only(&auth_path(), &bytes)
        .context("saving OpenAI OAuth credentials")
}

fn now() -> i64 { chrono::Utc::now().timestamp() }

fn b64url(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 { out.push(T[((n >> 6) & 63) as usize] as char); }
        if chunk.len() > 2 { out.push(T[(n & 63) as usize] as char); }
    }
    out
}

fn b64url_decode(s: &str) -> Result<Vec<u8>> {
    fn val(b: u8) -> Option<u8> {
        match b {
            b'A'..=b'Z' => Some(b - b'A'), b'a'..=b'z' => Some(b - b'a' + 26),
            b'0'..=b'9' => Some(b - b'0' + 52), b'-' => Some(62), b'_' => Some(63), _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc: u32 = 0;
    let mut bits = 0u8;
    for b in s.bytes() {
        let v = val(b).context("invalid base64url character")? as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((acc >> bits) & 0xff) as u8);
        }
    }
    Ok(out)
}

fn jwt_claims(jwt: &str) -> Option<Value> {
    let payload = jwt.split('.').nth(1)?;
    serde_json::from_slice(&b64url_decode(payload).ok()?).ok()
}

fn claims_account_id(jwt: &str) -> Option<String> {
    let v = jwt_claims(jwt)?;
    v.pointer("/https:~1~1api.openai.com~1auth/chatgpt_account_id")
        .or_else(|| v.get("chatgpt_account_id"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

fn jwt_exp(jwt: &str) -> Option<i64> { jwt_claims(jwt)?.get("exp")?.as_i64() }

fn random_b64url(n: usize) -> Result<String> {
    let mut bytes = vec![0u8; n];
    getrandom::getrandom(&mut bytes).map_err(|e| anyhow!("system RNG unavailable: {e}"))?;
    Ok(b64url(&bytes))
}

fn pkce() -> Result<(String, String)> {
    let verifier = random_b64url(48)?;
    let digest = ring::digest::digest(&ring::digest::SHA256, verifier.as_bytes());
    Ok((verifier, b64url(digest.as_ref())))
}

fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

async fn callback_listener() -> Result<(tokio::net::TcpListener, String)> {
    for port in [1455u16, 1457u16] {
        if let Ok(listener) = tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            return Ok((listener, format!("http://localhost:{port}/auth/callback")));
        }
    }
    bail!("OpenAI OAuth callback ports 1455 and 1457 are both in use")
}

async fn callback_code(listener: tokio::net::TcpListener, expected_state: &str) -> Result<String> {
    let deadline = tokio::time::Instant::now() + AUTH_TIMEOUT;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let (mut socket, _) = tokio::time::timeout(left, listener.accept())
            .await.context("timed out waiting for OpenAI sign-in")??;
        let mut buf = vec![0u8; 16 * 1024];
        let n = tokio::time::timeout(Duration::from_secs(5), socket.read(&mut buf)).await??;
        let req = String::from_utf8_lossy(&buf[..n]);
        let path = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/");
        let parsed = url::Url::parse(&format!("http://localhost{path}")).ok();
        let mut code = None;
        let mut state = None;
        let mut error = None;
        if let Some(url) = parsed {
            for (k, v) in url.query_pairs() {
                match k.as_ref() { "code" => code = Some(v.to_string()), "state" => state = Some(v.to_string()), "error" => error = Some(v.to_string()), _ => {} }
            }
        }
        let ok = error.is_none() && code.is_some() && state.as_deref() == Some(expected_state);
        let body = if ok { "OpenAI sign-in complete. You can close this tab and return to Aizen." } else { "OpenAI sign-in was not completed. Return to Aizen for details." };
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
        let _ = socket.write_all(response.as_bytes()).await;
        if let Some(err) = error { bail!("OpenAI authorization failed: {err}"); }
        if code.is_none() { continue; }
        if state.as_deref() != Some(expected_state) { bail!("OpenAI OAuth state mismatch; sign-in aborted"); }
        return Ok(code.unwrap());
    }
}

async fn interactive_login(client: &reqwest::Client) -> Result<StoredAuth> {
    let (listener, redirect_uri) = callback_listener().await?;
    let (verifier, challenge) = pkce()?;
    let state = random_b64url(24)?;
    let mut url = url::Url::parse(&format!("{OAUTH_ISSUER}/oauth/authorize"))?;
    {
        let mut q = url.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", OAUTH_CLIENT_ID)
            .append_pair("redirect_uri", &redirect_uri)
            .append_pair("scope", OAUTH_SCOPE)
            .append_pair("code_challenge", &challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("id_token_add_organizations", "true")
            .append_pair("codex_cli_simplified_flow", "true")
            .append_pair("state", &state)
            .append_pair("originator", "aizen_cli");
    }
    eprintln!("Opening OpenAI sign-in in your browser. If it does not open, visit:\n{}", url);
    open_browser(url.as_str());
    let code = callback_code(listener, &state).await?;
    #[derive(Deserialize)] struct TokenResponse { id_token: String, access_token: String, refresh_token: String }
    let response = client.post(format!("{OAUTH_ISSUER}/oauth/token"))
        .form(&[("grant_type", "authorization_code"), ("code", code.as_str()), ("redirect_uri", redirect_uri.as_str()), ("client_id", OAUTH_CLIENT_ID), ("code_verifier", verifier.as_str())])
        .send().await.context("exchanging OpenAI authorization code")?;
    let status = response.status();
    if !status.is_success() { bail!("OpenAI token exchange returned HTTP {status}: {}", response.text().await.unwrap_or_default()); }
    let raw: TokenResponse = response.json().await.context("parsing OpenAI token response")?;
    let auth = StoredAuth {
        account_id: claims_account_id(&raw.id_token),
        expires_at: jwt_exp(&raw.access_token),
        access_token: raw.access_token,
        refresh_token: raw.refresh_token,
        id_token: Some(raw.id_token),
    };
    save_auth(&auth)?;
    Ok(auth)
}

async fn refresh(client: &reqwest::Client, old: &StoredAuth) -> Result<StoredAuth> {
    #[derive(Deserialize)] struct RefreshResponse { access_token: Option<String>, refresh_token: Option<String>, id_token: Option<String> }
    let response = client.post(format!("{OAUTH_ISSUER}/oauth/token"))
        .json(&json!({"client_id": OAUTH_CLIENT_ID, "grant_type": "refresh_token", "refresh_token": old.refresh_token}))
        .send().await.context("refreshing OpenAI OAuth token")?;
    let status = response.status();
    if !status.is_success() { bail!("OpenAI token refresh returned HTTP {status}: {}", response.text().await.unwrap_or_default()); }
    let raw: RefreshResponse = response.json().await?;
    let access = raw.access_token.context("OpenAI refresh response omitted access_token")?;
    let id_token = raw.id_token.or_else(|| old.id_token.clone());
    let auth = StoredAuth {
        account_id: id_token.as_deref().and_then(claims_account_id).or_else(|| old.account_id.clone()),
        expires_at: jwt_exp(&access),
        access_token: access,
        refresh_token: raw.refresh_token.unwrap_or_else(|| old.refresh_token.clone()),
        id_token,
    };
    save_auth(&auth)?;
    Ok(auth)
}

async fn credentials(client: &reqwest::Client) -> Result<StoredAuth> {
    if let Some(auth) = load_auth() {
        if auth.expires_at.map(|exp| now() < exp - EXPIRY_MARGIN_SECS).unwrap_or(true) { return Ok(auth); }
        match refresh(client, &auth).await { Ok(new) => return Ok(new), Err(e) => eprintln!("OpenAI token refresh failed ({e}); signing in again.") }
    }
    interactive_login(client).await
}

fn auth_request(rb: reqwest::RequestBuilder, auth: &StoredAuth) -> reqwest::RequestBuilder {
    let mut rb = rb.bearer_auth(&auth.access_token);
    if let Some(account) = &auth.account_id { rb = rb.header("ChatGPT-Account-ID", account); }
    rb.header("originator", "aizen_cli")
}

fn response_input(messages: &[Message]) -> (String, Vec<Value>) {
    let mut instructions = Vec::new();
    let mut input = Vec::new();
    for m in messages {
        if m.role == "system" {
            if let Some(s) = m.content.as_deref().filter(|s| !s.is_empty()) { instructions.push(s.to_string()); }
            continue;
        }
        if m.role == "tool" {
            if let Some(call_id) = &m.tool_call_id {
                input.push(json!({"type":"function_call_output","call_id":call_id,"output":m.content.clone().unwrap_or_default()}));
            }
            continue;
        }
        if m.role == "assistant" && !m.tool_calls.is_empty() {
            if let Some(text) = m.content.as_deref().filter(|s| !s.is_empty()) {
                input.push(json!({"type":"message","role":"assistant","content":[{"type":"output_text","text":text}]}));
            }
            for c in &m.tool_calls {
                input.push(json!({"type":"function_call","call_id":c.id,"name":c.function.name,"arguments":c.function.arguments}));
            }
            continue;
        }
        let typ = if m.role == "assistant" { "output_text" } else { "input_text" };
        let mut content = Vec::new();
        if let Some(text) = m.content.as_deref().filter(|s| !s.is_empty()) { content.push(json!({"type":typ,"text":text})); }
        if m.role == "user" { for image in &m.images { content.push(json!({"type":"input_image","image_url":image})); } }
        if !content.is_empty() { input.push(json!({"type":"message","role":m.role,"content":content})); }
    }
    (instructions.join("\n\n"), input)
}

fn response_tools(tools: &[ToolDef]) -> Vec<Value> {
    tools.iter().map(|t| json!({"type":"function","name":t.function.name,"description":t.function.description,"parameters":t.function.parameters})).collect()
}

fn response_body(model: &str, messages: &[Message], tools: &[ToolDef], effort: Option<String>) -> Value {
    let (instructions, input) = response_input(messages);
    let mut body = json!({
        "model": model, "instructions": instructions, "input": input, "tools": response_tools(tools),
        "tool_choice": "auto", "parallel_tool_calls": !tools.is_empty(), "store": false, "stream": true,
        "include": ["reasoning.encrypted_content"]
    });
    if let Some(effort) = effort { body["reasoning"] = json!({"effort": effort, "summary": "auto"}); }
    body
}

#[derive(Default)]
struct TurnAcc { text: String, calls: BTreeMap<String, ToolCall>, usage: Option<Usage> }

fn completed_usage(v: &Value) -> Option<Usage> {
    let u = v.pointer("/response/usage")?;
    Some(Usage {
        prompt_tokens: u.get("input_tokens").and_then(Value::as_u64),
        completion_tokens: u.get("output_tokens").and_then(Value::as_u64),
        total_tokens: u.get("total_tokens").and_then(Value::as_u64),
        ..Usage::default()
    })
}

fn ingest_event(acc: &mut TurnAcc, data: &str, print_live: bool) -> Result<bool> {
    let v: Value = serde_json::from_str(data).context("parsing ChatGPT Responses SSE event")?;
    match v.get("type").and_then(Value::as_str).unwrap_or("") {
        "response.output_text.delta" => {
            if let Some(delta) = v.get("delta").and_then(Value::as_str) {
                acc.text.push_str(delta);
                if print_live { crate::ui::tui::emit(delta); }
            }
        }
        "response.output_item.done" => {
            if let Some(item) = v.get("item") {
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("call").to_string();
                    let call = ToolCall { id: call_id.clone(), kind: "function".into(), function: FunctionCall { name: item.get("name").and_then(Value::as_str).unwrap_or("").to_string(), arguments: item.get("arguments").and_then(Value::as_str).unwrap_or("{}").to_string() } };
                    acc.calls.insert(call_id, call);
                }
            }
        }
        "response.completed" => { acc.usage = completed_usage(&v); return Ok(true); }
        "response.failed" | "error" => { bail!("ChatGPT Responses API error: {}", v.get("response").or_else(|| v.get("error")).unwrap_or(&v)); }
        _ => {}
    }
    Ok(false)
}

pub async fn chat_turn(client: &reqwest::Client, base_url: &str, model: &str, messages: &[Message], tools: &[ToolDef], effort: Option<String>, print_live: bool) -> Result<ChatTurn> {
    let mut auth = credentials(client).await?;
    let url = format!("{}/responses", base_url.trim_end_matches('/'));
    let body = response_body(model, messages, tools, effort);
    let mut response = auth_request(client.post(&url).json(&body), &auth).send().await?;
    if response.status() == reqwest::StatusCode::UNAUTHORIZED {
        auth = match refresh(client, &auth).await {
            Ok(next) => next,
            Err(_) => interactive_login(client).await?,
        };
        response = auth_request(client.post(&url).json(&body), &auth).send().await?;
    }
    let status = response.status();
    if !status.is_success() { bail!("ChatGPT Responses API returned HTTP {status}: {}", response.text().await.unwrap_or_default()); }
    let mut stream = response.bytes_stream().eventsource();
    let mut acc = TurnAcc::default();
    while let Some(event) = stream.next().await {
        let event = event.context("ChatGPT Responses SSE stream failed")?;
        if event.data.trim().is_empty() || event.data.trim() == "[DONE]" { continue; }
        if ingest_event(&mut acc, &event.data, print_live)? { break; }
    }
    if print_live && !acc.text.is_empty() { crate::ui::tui::emit("\n"); }
    Ok(ChatTurn { content: (!acc.text.is_empty()).then_some(acc.text), tool_calls: acc.calls.into_values().collect(), finish_reason: Some("stop".into()), usage: acc.usage, eager: Vec::new() })
}

pub async fn models(client: &reqwest::Client, base_url: &str) -> Result<Vec<ModelInfo>> {
    let auth = credentials(client).await?;
    let response = auth_request(client.get(format!("{}/models", base_url.trim_end_matches('/'))), &auth).send().await?;
    let status = response.status();
    if !status.is_success() { bail!("ChatGPT model list returned HTTP {status}: {}", response.text().await.unwrap_or_default()); }
    let v: Value = response.json().await?;
    let rows = v.get("data").or_else(|| v.get("models")).and_then(Value::as_array).cloned().unwrap_or_default();
    Ok(rows.into_iter().filter_map(|m| {
        let id = m.get("id").or_else(|| m.get("slug")).or_else(|| m.get("model")).and_then(Value::as_str)?.to_string();
        let context_length = m.get("context_window").or_else(|| m.get("context_length")).and_then(Value::as_u64).map(|n| n as usize);
        Some(ModelInfo { id, context_length })
    }).collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn identifies_only_codex_backend() {
        assert!(is_chatgpt_base("https://chatgpt.com/backend-api/codex"));
        assert!(is_chatgpt_base("https://chatgpt.com/backend-api/codex/"));
        assert!(!is_chatgpt_base("https://api.openai.com/v1"));
        assert!(!is_chatgpt_base("https://example.com/backend-api/codex"));
    }
    #[test] fn maps_tool_history_to_responses_items() {
        let messages = vec![Message::assistant_tool_calls(vec![ToolCall { id:"c1".into(), kind:"function".into(), function: FunctionCall { name:"read".into(), arguments:"{\"path\":\"x\"}".into() }}]), Message::tool_result("c1", "ok")];
        let (_, input) = response_input(&messages);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[1]["type"], "function_call_output");
    }
}
