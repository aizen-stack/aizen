//! ChatGPT Codex OAuth (experimental) — browser PKCE against auth.openai.com, tokens on disk.
//!
//! This is the ChatGPT/Codex *consumer* login path (not the OpenAI Platform API key flow).
//! Upstream endpoints and the public Codex CLI client_id are private/compatibility surfaces and
//! may break or conflict with vendor terms without notice. Gated by user opt-in + RISK notice.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

/// Public Codex CLI OAuth client id (no secret; PKCE public client).
pub const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
pub const AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
pub const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
pub const SCOPE: &str = "openid profile email offline_access";
/// Codex CLI historically binds this port; we try it first.
pub const PREFERRED_CALLBACK_PORT: u16 = 1455;
const AUTH_TIMEOUT: Duration = Duration::from_secs(300);
const EXPIRY_MARGIN_SECS: i64 = 60;
/// Refresh this far before expiry when the token lives longer than a day (Codex refresh lead).
const LONG_REFRESH_LEAD_SECS: i64 = 5 * 24 * 3600;

/// Default Responses API URL used when the active provider is Codex OAuth.
pub const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";
/// Sentinel base_url stored in cli-config for Codex OAuth profiles (no /v1 chat suffix).
pub const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

/// Env kill-switch — when set to 1/true/yes, Codex OAuth is refused.
pub fn codex_disabled() -> bool {
    matches!(
        std::env::var("AIZEN_DISABLE_CODEX")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexTokenSet {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<i64>,
    #[serde(default)]
    pub id_token: Option<String>,
    #[serde(default)]
    pub account_id: Option<String>,
    #[serde(default)]
    pub client_id: String,
    #[serde(default)]
    pub last_refresh_at: Option<i64>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

impl CodexTokenSet {
    pub fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(exp) => {
                let lead = if exp - now() > 24 * 3600 {
                    LONG_REFRESH_LEAD_SECS
                        .min(exp - now() - 60)
                        .max(EXPIRY_MARGIN_SECS)
                } else {
                    EXPIRY_MARGIN_SECS
                };
                now() >= exp - lead
            }
            None => false,
        }
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

pub fn tokens_dir() -> PathBuf {
    crate::core::config::aizen_home().join("provider-tokens")
}

pub fn token_path() -> PathBuf {
    tokens_dir().join("codex.json")
}

pub fn has_token() -> bool {
    token_path().is_file()
}

pub fn load_token() -> Option<CodexTokenSet> {
    let s = std::fs::read_to_string(token_path()).ok()?;
    serde_json::from_str(&s).ok()
}

pub fn clear_token() {
    let _ = std::fs::remove_file(token_path());
}

pub fn save_token(t: &CodexTokenSet) -> Result<()> {
    let dir = tokens_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = token_path();
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_vec_pretty(t).context("serializing codex token")?;
    std::fs::write(&tmp, &body).with_context(|| format!("writing {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
    }
    std::fs::rename(&tmp, &path).with_context(|| format!("renaming to {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn b64url(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[((n >> 18) & 63) as usize] as char);
        out.push(T[((n >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(T[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(T[(n & 63) as usize] as char);
        }
    }
    out
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let d = ring::digest::digest(&ring::digest::SHA256, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

fn rand_b64url(n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).map_err(|e| anyhow::anyhow!("system RNG unavailable: {e}"))?;
    Ok(b64url(&buf))
}

fn pkce() -> Result<(String, String)> {
    let verifier = rand_b64url(48)?;
    let challenge = b64url(&sha256(verifier.as_bytes()));
    Ok((verifier, challenge))
}

fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent("aizen-codex-oauth")
        .build()
        .context("building OAuth HTTP client")
}

/// Build the authorize URL. Spaces in scope are %20 (not '+'), matching Codex CLI.
pub fn build_authorize_url(redirect_uri: &str, state: &str, code_challenge: &str) -> String {
    let params = [
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", redirect_uri),
        ("scope", SCOPE),
        ("code_challenge", code_challenge),
        ("code_challenge_method", "S256"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", "codex_cli_rs"),
        ("state", state),
    ];
    let qs = params
        .iter()
        .map(|(k, v)| format!("{k}={}", urlencoding_encode(v)))
        .collect::<Vec<_>>()
        .join("&");
    format!("{AUTHORIZE_URL}?{qs}")
}

fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.as_bytes() {
        match *b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(*b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Pull a ChatGPT account id out of an id_token payload when present.
pub fn account_id_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let padded = match payload.len() % 4 {
        2 => format!("{payload}=="),
        3 => format!("{payload}="),
        _ => payload.to_string(),
    };
    let b64 = padded.replace('-', "+").replace('_', "/");
    let raw = b64_decode_std(&b64)?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    // Claim names observed across Codex/ChatGPT id_tokens — first hit wins.
    for key in [
        "https://api.openai.com/auth.chatgpt_account_id",
        "chatgpt_account_id",
        "account_id",
        "https://api.openai.com/auth.account_id",
    ] {
        if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
            if !s.is_empty() {
                return Some(s.to_string());
            }
        }
    }
    // Nested organizations / auth object fallbacks.
    if let Some(s) = v
        .pointer("/https://api.openai.com/auth/chatgpt_account_id")
        .and_then(|x| x.as_str())
    {
        if !s.is_empty() {
            return Some(s.to_string());
        }
    }
    None
}

fn b64_decode_std(s: &str) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            b'=' => Some(0),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    if bytes.len() % 4 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for chunk in bytes.chunks(4) {
        let (a, b, c, d) = (
            val(chunk[0])?,
            val(chunk[1])?,
            val(chunk[2])?,
            val(chunk[3])?,
        );
        out.push((a << 2) | (b >> 4));
        if chunk[2] != b'=' {
            out.push((b << 4) | (c >> 2));
        }
        if chunk[3] != b'=' {
            out.push((c << 6) | d);
        }
    }
    Some(out)
}

fn label_from_id_token(id_token: &str) -> Option<String> {
    let payload = id_token.split('.').nth(1)?;
    let padded = match payload.len() % 4 {
        2 => format!("{payload}=="),
        3 => format!("{payload}="),
        _ => payload.to_string(),
    };
    let b64 = padded.replace('-', "+").replace('_', "/");
    let raw = b64_decode_std(&b64)?;
    let v: serde_json::Value = serde_json::from_slice(&raw).ok()?;
    v.get("email")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
}

async fn respond(sock: &mut tokio::net::TcpStream, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Aizen</title>         <body style=\"font-family:system-ui;padding:2rem\"><h1>Aizen</h1><p>{message}</p>         <p>You can close this window.</p></body>"
    );
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = sock.write_all(resp.as_bytes()).await;
    let _ = sock.flush().await;
}

async fn bind_callback_listener() -> Result<(tokio::net::TcpListener, u16, bool)> {
    match tokio::net::TcpListener::bind(("127.0.0.1", PREFERRED_CALLBACK_PORT)).await {
        Ok(l) => Ok((l, PREFERRED_CALLBACK_PORT, false)),
        Err(_) => {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .context("binding loopback callback port")?;
            let port = l.local_addr()?.port();
            Ok((l, port, true))
        }
    }
}

/// Interactive browser login. Returns the saved token set.
pub async fn login_interactive() -> Result<CodexTokenSet> {
    if codex_disabled() {
        bail!("Codex OAuth disabled via AIZEN_DISABLE_CODEX");
    }
    let (listener, port, fallback) = bind_callback_listener().await?;
    if fallback {
        eprintln!(
            "note: port {PREFERRED_CALLBACK_PORT} busy — using ephemeral :{port}. If OpenAI rejects the redirect, free {PREFERRED_CALLBACK_PORT} (other Codex CLI) and retry."
        );
    }
    let redirect_uri = format!("http://127.0.0.1:{port}/auth/callback");
    let (verifier, challenge) = pkce()?;
    let state = rand_b64url(16)?;
    let auth_url = build_authorize_url(&redirect_uri, &state, &challenge);

    eprintln!("Opening browser for ChatGPT / Codex sign-in…");
    eprintln!("If it does not open, visit:\n{auth_url}\n");
    open_browser(&auth_url);

    let (mut sock, _) = tokio::time::timeout(AUTH_TIMEOUT, listener.accept())
        .await
        .context("timed out waiting for OAuth callback (5 minutes)")?
        .context("accepting OAuth callback")?;
    let mut buf = vec![0u8; 8192];
    let n = sock.read(&mut buf).await.unwrap_or(0);
    let req = String::from_utf8_lossy(&buf[..n]);
    let line = req.lines().next().unwrap_or("");
    // GET /auth/callback?code=...&state=... HTTP/1.1
    let path = line.split_whitespace().nth(1).unwrap_or("");
    let q = path.split_once('?').map(|(_, q)| q).unwrap_or("");
    let mut code: Option<String> = None;
    let mut got_state: Option<String> = None;
    let mut err: Option<String> = None;
    let mut err_desc: Option<String> = None;
    for pair in q.split('&') {
        let mut it = pair.splitn(2, '=');
        let k = it.next().unwrap_or("");
        let v = it.next().unwrap_or("");
        let v = urlencoding_decode(v);
        match k {
            "code" => code = Some(v),
            "state" => got_state = Some(v),
            "error" => err = Some(v),
            "error_description" => err_desc = Some(v),
            _ => {}
        }
    }
    if let Some(e) = err {
        respond(
            &mut sock,
            &format!("Sign-in failed: {}", err_desc.as_deref().unwrap_or(&e)),
        )
        .await;
        bail!("OAuth error: {}", err_desc.unwrap_or(e));
    }
    if got_state.as_deref() != Some(state.as_str()) {
        respond(&mut sock, "State mismatch — possible CSRF. Aborted.").await;
        bail!("OAuth state mismatch (possible CSRF) — aborted");
    }
    let code = code.context("callback missing authorization code")?;
    respond(
        &mut sock,
        "Sign-in complete. Return to the terminal — Aizen is saving tokens.",
    )
    .await;

    let client = http_client()?;
    let form = [
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("code_verifier", verifier.as_str()),
    ];
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&form)
        .send()
        .await
        .context("token exchange request")?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "token exchange failed: HTTP {status} — {}",
            truncate(&body, 400)
        );
    }
    let v: serde_json::Value = resp.json().await.context("parsing token JSON")?;
    let access = v
        .get("access_token")
        .and_then(|x| x.as_str())
        .context("token response missing access_token")?
        .to_string();
    let refresh = v
        .get("refresh_token")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let id_token = v
        .get("id_token")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let expires_in = v.get("expires_in").and_then(|x| x.as_u64()).or_else(|| {
        v.get("expires_in")
            .and_then(|x| x.as_str())
            .and_then(|s| s.parse().ok())
    });
    let scope = v
        .get("scope")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string());
    let account_id = id_token.as_deref().and_then(account_id_from_id_token);
    let label = id_token.as_deref().and_then(label_from_id_token);
    let set = CodexTokenSet {
        access_token: access,
        refresh_token: refresh,
        expires_at: expires_in.map(|s| now() + s as i64),
        id_token,
        account_id,
        client_id: CLIENT_ID.to_string(),
        last_refresh_at: Some(now()),
        scope,
        label,
    };
    save_token(&set)?;
    Ok(set)
}

fn urlencoding_decode(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let h = |c: u8| match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    b'A'..=b'F' => Some(c - b'A' + 10),
                    _ => None,
                };
                if let (Some(hi), Some(lo)) = (h(b[i + 1]), h(b[i + 2])) {
                    out.push((hi << 4) | lo);
                    i += 3;
                } else {
                    out.push(b[i]);
                    i += 1;
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn truncate(s: &str, n: usize) -> String {
    if s.len() <= n {
        s.to_string()
    } else {
        format!("{}…", &s[..n])
    }
}

#[derive(Debug)]
pub enum RefreshOutcome {
    Ok(CodexTokenSet),
    /// Refresh token rejected permanently — caller must re-login.
    ReauthRequired(String),
    Transient(String),
}

static REFRESH_LOCK: once_cell::sync::Lazy<Arc<Mutex<()>>> =
    once_cell::sync::Lazy::new(|| Arc::new(Mutex::new(())));

/// Refresh the access token. Single-flight across the process.
pub async fn refresh_token(set: &CodexTokenSet) -> RefreshOutcome {
    let _guard = REFRESH_LOCK.lock().await;
    // Re-load disk in case another task already refreshed.
    if let Some(fresh) = load_token() {
        if !fresh.is_expired() && fresh.access_token != set.access_token {
            return RefreshOutcome::Ok(fresh);
        }
    }
    let Some(rt) = set.refresh_token.as_deref() else {
        return RefreshOutcome::ReauthRequired("no refresh_token on file".into());
    };
    let client = match http_client() {
        Ok(c) => c,
        Err(e) => return RefreshOutcome::Transient(e.to_string()),
    };
    // Codex refresh path uses JSON body (observed in compatible clients).
    let body = serde_json::json!({
        "client_id": set.client_id.as_str(),
        "grant_type": "refresh_token",
        "refresh_token": rt,
    });
    let resp = match client
        .post(TOKEN_URL)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => return RefreshOutcome::Transient(format!("network: {e}")),
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let lower = text.to_ascii_lowercase();
        if status.as_u16() == 400
            || lower.contains("invalid_grant")
            || lower.contains("already")
            || lower.contains("revoked")
        {
            clear_token();
            return RefreshOutcome::ReauthRequired(format!(
                "HTTP {status}: {}",
                truncate(&text, 240)
            ));
        }
        return RefreshOutcome::Transient(format!("HTTP {status}: {}", truncate(&text, 240)));
    }
    let v: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => return RefreshOutcome::Transient(e.to_string()),
    };
    let access = match v.get("access_token").and_then(|x| x.as_str()) {
        Some(a) => a.to_string(),
        None => return RefreshOutcome::Transient("refresh response missing access_token".into()),
    };
    let refresh = v
        .get("refresh_token")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .or_else(|| set.refresh_token.clone());
    let id_token = v
        .get("id_token")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .or_else(|| set.id_token.clone());
    let expires_in = v.get("expires_in").and_then(|x| x.as_u64());
    let account_id = id_token
        .as_deref()
        .and_then(account_id_from_id_token)
        .or_else(|| set.account_id.clone());
    let label = id_token
        .as_deref()
        .and_then(label_from_id_token)
        .or_else(|| set.label.clone());
    let out = CodexTokenSet {
        access_token: access,
        refresh_token: refresh,
        expires_at: expires_in.map(|s| now() + s as i64).or(set.expires_at),
        id_token,
        account_id,
        client_id: set.client_id.clone(),
        last_refresh_at: Some(now()),
        scope: v
            .get("scope")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .or_else(|| set.scope.clone()),
        label,
    };
    if let Err(e) = save_token(&out) {
        return RefreshOutcome::Transient(format!("save failed: {e}"));
    }
    RefreshOutcome::Ok(out)
}

/// Return a usable access token, refreshing if needed.
pub async fn bearer_token() -> Result<(String, Option<String>)> {
    if codex_disabled() {
        bail!("Codex OAuth disabled via AIZEN_DISABLE_CODEX");
    }
    let mut set = load_token().context(
        "no Codex login — run `aizen auth login codex` (experimental ChatGPT/Codex OAuth)",
    )?;
    if set.is_expired() {
        match refresh_token(&set).await {
            RefreshOutcome::Ok(s) => set = s,
            RefreshOutcome::ReauthRequired(m) => {
                bail!("Codex re-login required: {m} — run `aizen auth login codex`")
            }
            RefreshOutcome::Transient(m) => bail!("Codex token refresh failed: {m}"),
        }
    }
    Ok((set.access_token, set.account_id))
}

/// True when this base_url should use the Codex Responses backend.
pub fn is_codex_base_url(base_url: &str) -> bool {
    let b = base_url.trim().trim_end_matches('/').to_ascii_lowercase();
    if b == CODEX_BASE_URL
        || b == CODEX_RESPONSES_URL
            .trim_end_matches('/')
            .to_ascii_lowercase()
    {
        return true;
    }
    // Host-matched: only first-party chatgpt.com Codex paths.
    let after_scheme = b.split_once("://").map(|(_, r)| r).unwrap_or(b.as_str());
    let host = after_scheme
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .split('@')
        .next_back()
        .unwrap_or("")
        .split(':')
        .next()
        .unwrap_or("");
    if host != "chatgpt.com" && !host.ends_with(".chatgpt.com") {
        return false;
    }
    after_scheme.contains("/backend-api/codex")
}

/// Human status lines for `aizen auth status` (never prints raw tokens).
pub fn status_lines() -> Vec<String> {
    let mut lines = Vec::new();
    if codex_disabled() {
        lines.push("codex: disabled (AIZEN_DISABLE_CODEX)".into());
        return lines;
    }
    match load_token() {
        None => lines.push("codex: not logged in".into()),
        Some(t) => {
            let who = t.label.as_deref().unwrap_or("(no email claim)");
            let exp = match t.expires_at {
                Some(ts) => {
                    let left = ts - now();
                    if left <= 0 {
                        "expired".into()
                    } else if left > 3600 {
                        format!("expires in {}h", left / 3600)
                    } else {
                        format!("expires in {}m", left / 60)
                    }
                }
                None => "no expiry".into(),
            };
            let acct = t
                .account_id
                .as_deref()
                .map(|a| format!("account {}", &a[..a.len().min(8)]))
                .unwrap_or_else(|| "account ?".into());
            let st = if t.is_expired() {
                "needs refresh"
            } else {
                "ok"
            };
            lines.push(format!("codex: {st} · {who} · {acct}… · {exp}"));
            lines.push(format!("  token file: {}", token_path().display()));
        }
    }
    lines
}

pub fn risk_notice() -> &'static str {
    "EXPERIMENTAL. Uses ChatGPT/Codex consumer OAuth and private backend APIs (not the OpenAI Platform API). May break without notice; may conflict with OpenAI terms; rate limits follow your ChatGPT/Codex plan. Prefer OpenAI API keys or OpenRouter for supported use. Remove tokens with: aizen auth logout codex"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_url_encodes_scope_spaces_as_percent20() {
        let u = build_authorize_url("http://127.0.0.1:1455/auth/callback", "st", "ch");
        assert!(u.contains("scope=openid%20profile%20email%20offline_access"));
        assert!(!u.contains("scope=openid+profile"));
        assert!(u.contains("code_challenge_method=S256"));
        assert!(u.contains("originator=codex_cli_rs"));
        assert!(u.contains("client_id=app_EMoamEEZ73f0CkXaXp7hrann"));
    }

    #[test]
    fn codex_base_url_detection() {
        assert!(is_codex_base_url(CODEX_BASE_URL));
        assert!(is_codex_base_url("https://chatgpt.com/backend-api/codex/"));
        assert!(is_codex_base_url(CODEX_RESPONSES_URL));
        assert!(!is_codex_base_url("https://api.openai.com/v1"));
        assert!(!is_codex_base_url(
            "https://evil.com/chatgpt.com/backend-api/codex"
        ));
    }

    #[test]
    fn account_id_from_synthetic_jwt() {
        // header.payload.sig — payload {"chatgpt_account_id":"acc-123","email":"a@b.c"}
        let payload = b64url(br#"{"chatgpt_account_id":"acc-123","email":"a@b.c"}"#);
        let jwt = format!("x.{payload}.y");
        assert_eq!(account_id_from_id_token(&jwt).as_deref(), Some("acc-123"));
        assert_eq!(label_from_id_token(&jwt).as_deref(), Some("a@b.c"));
    }
}
