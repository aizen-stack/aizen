//! Pure-Rust **OAuth 2.1 (PKCE)** client for MCP remote servers — the piece that lets `ng apps`
//! sign into the apps people actually want (Linear, Notion, Slack, Gmail/Google, Atlassian, Stripe),
//! all of which expose **OAuth-only** MCP endpoints. This closes the headline gap vs Hermes, whose
//! "apps" feature is MCP-over-OAuth. Without it every marquee app showed "not connectable".
//!
//! Flow (MCP Authorization spec, 2025-06-18, layering OAuth 2.1 + RFC 9728 + RFC 8414 + RFC 7591):
//!   1. Discover: parse the server's `401 WWW-Authenticate` for the protected-resource-metadata URL
//!      (RFC 9728), fetch it → the authorization server issuer + canonical `resource` (RFC 8707),
//!      then fetch the AS metadata (`/.well-known/oauth-authorization-server` or
//!      `/.well-known/openid-configuration`) → authorization + token + registration endpoints.
//!   2. Register: dynamic client registration (RFC 7591) for a public client (`token_endpoint_auth
//!      _method: none`) unless the user pinned `oauth.client_id` (Google/Atlassian-style).
//!   3. Authorize: PKCE S256 (code_verifier→SHA-256→base64url challenge), open the browser to the
//!      authorization endpoint, catch the redirect on an ephemeral `127.0.0.1` loopback listener,
//!      validate `state`.
//!   4. Exchange the code at the token endpoint → cache `{access,refresh}` token at
//!      `~/.aizen/mcp-tokens/<key>.json` (0600). Refresh transparently on expiry / 401.
//!
//! Static-binary posture: SHA-256 + the CSPRNG come from `ring` (already linked via reqwest→rustls);
//! `getrandom` is likewise already in the tree; base64url is hand-rolled (no dep); the loopback
//! catcher is tokio `net`. No `*-sys` beyond what rustls already pulls, no C toolchain added.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// How long we wait for the user to complete the browser sign-in before giving up.
const AUTH_TIMEOUT: Duration = Duration::from_secs(300);
/// Refresh a token this many seconds BEFORE its stated expiry (clock-skew + in-flight margin).
const EXPIRY_MARGIN_SECS: i64 = 60;

// ───────────────────────────── config (mcp.json `oauth` block) ─────────────────────────────

/// Optional OAuth overrides for a server that can't do dynamic client registration (Google,
/// Atlassian): a pre-registered client + explicit scopes / authorization-server override.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OAuthConfig {
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Skip resource-metadata discovery and use this authorization-server issuer directly.
    #[serde(default)]
    pub authorization_server: Option<String>,
}

// ───────────────────────────── token cache ─────────────────────────────

/// A cached OAuth token + everything needed to refresh it without re-discovering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenSet {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    /// Unix seconds when `access_token` stops being valid (None = no stated expiry).
    #[serde(default)]
    pub expires_at: Option<i64>,
    pub token_endpoint: String,
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub resource: Option<String>,
}

impl TokenSet {
    /// True when the access token is at/over its expiry (with a safety margin). A token with no
    /// stated expiry is treated as live (refresh happens reactively on a 401 instead).
    pub fn is_expired(&self) -> bool {
        match self.expires_at {
            Some(exp) => now() >= exp - EXPIRY_MARGIN_SECS,
            None => false,
        }
    }
}

fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

/// `~/.aizen/mcp-tokens/` — the per-server token cache directory.
pub fn tokens_dir() -> PathBuf {
    crate::core::config::nextgen_home().join("mcp-tokens")
}

/// Token-cache path for one server key (sanitized so a `/` in a key can't escape the dir).
pub fn token_path(key: &str) -> PathBuf {
    let safe: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    tokens_dir().join(format!("{safe}.json"))
}

pub fn load_token(key: &str) -> Option<TokenSet> {
    let s = std::fs::read_to_string(token_path(key)).ok()?;
    serde_json::from_str(&s).ok()
}

/// True when a token is cached for this server key (presence only — never reads the value out).
pub fn has_token(key: &str) -> bool {
    token_path(key).is_file()
}

pub fn clear_token(key: &str) {
    let _ = std::fs::remove_file(token_path(key));
}

fn save_token(key: &str, t: &TokenSet) -> Result<()> {
    let p = token_path(key);
    let mut bytes = serde_json::to_vec_pretty(t)?;
    bytes.push(b'\n');
    crate::core::persist::atomic_write_owner_only(&p, &bytes)
        .with_context(|| format!("writing {}", p.display()))
}

// ───────────────────────────── crypto / encoding (pure) ─────────────────────────────

/// base64url WITHOUT padding (RFC 4648 §5) — the encoding PKCE + JWT use. Hand-rolled so we add no
/// base64 dependency (and dodge any 0.13-vs-newer API churn).
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
        if chunk.len() > 1 {
            out.push(T[((n >> 6) & 63) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(T[(n & 63) as usize] as char);
        }
    }
    out
}

/// SHA-256 via ring (already linked by rustls — zero marginal cost).
fn sha256(data: &[u8]) -> [u8; 32] {
    let d = ring::digest::digest(&ring::digest::SHA256, data);
    let mut out = [0u8; 32];
    out.copy_from_slice(d.as_ref());
    out
}

/// `n` CSPRNG bytes, base64url-encoded (used for the PKCE verifier + the CSRF `state`).
fn rand_b64url(n: usize) -> Result<String> {
    let mut buf = vec![0u8; n];
    getrandom::getrandom(&mut buf).map_err(|e| anyhow::anyhow!("system RNG unavailable: {e}"))?;
    Ok(b64url(&buf))
}

/// (code_verifier, code_challenge) for PKCE S256. 48 random bytes → 64 base64url chars (within the
/// spec's 43..128 range); challenge = base64url(SHA-256(verifier)).
fn pkce() -> Result<(String, String)> {
    let verifier = rand_b64url(48)?;
    let challenge = b64url(&sha256(verifier.as_bytes()));
    Ok((verifier, challenge))
}

// ───────────────────────────── discovery (RFC 9728 + RFC 8414) ─────────────────────────────

#[derive(Debug, Clone)]
struct AuthMeta {
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    scopes_supported: Vec<String>,
    /// Canonical resource indicator (RFC 8707) sent on the authorize + token requests.
    resource: String,
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()
        .context("building OAuth HTTP client")
}

async fn fetch_json(client: &reqwest::Client, url: &str) -> Result<Value> {
    let resp = client.get(url).header(reqwest::header::ACCEPT, "application/json").send().await.with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("{url} → HTTP {}", resp.status().as_u16());
    }
    resp.json::<Value>().await.with_context(|| format!("parsing JSON from {url}"))
}

/// The `scheme://host[:port]` origin of a URL.
fn origin_of(url: &str) -> Result<String> {
    let u = url::Url::parse(url).with_context(|| format!("bad URL: {url}"))?;
    let host = u.host_str().context("URL has no host")?;
    Ok(match u.port() {
        Some(p) => format!("{}://{host}:{p}", u.scheme()),
        None => format!("{}://{host}", u.scheme()),
    })
}

/// Pull `resource_metadata="…"` out of a `WWW-Authenticate: Bearer …` challenge (RFC 9728).
fn parse_resource_metadata(header: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let idx = lower.find("resource_metadata")?;
    let after = header[idx + "resource_metadata".len()..].trim_start();
    let after = after.strip_prefix('=')?.trim_start();
    let after = after.strip_prefix('"').unwrap_or(after);
    let end = after.find('"').unwrap_or(after.len());
    let val = after[..end].trim();
    if val.is_empty() {
        None
    } else {
        Some(val.to_string())
    }
}

/// Candidate protected-resource-metadata URLs for a server (the WWW-Authenticate value wins; else the
/// well-known location at the origin, with and without the server's path component per RFC 9728).
fn prm_candidates(server_url: &str, www_authenticate: Option<&str>) -> Vec<String> {
    let mut v = Vec::new();
    if let Some(h) = www_authenticate {
        if let Some(u) = parse_resource_metadata(h) {
            v.push(u);
        }
    }
    if let Ok(origin) = origin_of(server_url) {
        if let Ok(u) = url::Url::parse(server_url) {
            let path = u.path().trim_end_matches('/');
            if !path.is_empty() {
                v.push(format!("{origin}/.well-known/oauth-protected-resource{path}"));
            }
        }
        v.push(format!("{origin}/.well-known/oauth-protected-resource"));
    }
    v
}

/// Candidate AS-metadata URLs for an issuer (RFC 8414: well-known is inserted between host and path).
fn as_metadata_candidates(issuer: &str) -> Vec<String> {
    let mut v = Vec::new();
    let Ok(origin) = origin_of(issuer) else { return v };
    let path = url::Url::parse(issuer).ok().map(|u| u.path().trim_end_matches('/').to_string()).unwrap_or_default();
    if path.is_empty() {
        v.push(format!("{origin}/.well-known/oauth-authorization-server"));
        v.push(format!("{origin}/.well-known/openid-configuration"));
    } else {
        v.push(format!("{origin}/.well-known/oauth-authorization-server{path}"));
        v.push(format!("{origin}{path}/.well-known/oauth-authorization-server"));
        v.push(format!("{origin}/.well-known/openid-configuration{path}"));
        v.push(format!("{origin}{path}/.well-known/openid-configuration"));
    }
    v
}

async fn discover(client: &reqwest::Client, server_url: &str, www_authenticate: Option<&str>, cfg: &OAuthConfig) -> Result<AuthMeta> {
    // 1. Resolve the authorization-server issuer + canonical resource.
    let (issuer, resource, mut scopes) = if let Some(asrv) = &cfg.authorization_server {
        (asrv.clone(), server_url.to_string(), Vec::new())
    } else {
        let mut found: Option<(String, String, Vec<String>)> = None;
        for prm in prm_candidates(server_url, www_authenticate) {
            if let Ok(meta) = fetch_json(client, &prm).await {
                let issuer = meta
                    .get("authorization_servers")
                    .and_then(|a| a.as_array())
                    .and_then(|a| a.first())
                    .and_then(|s| s.as_str())
                    .map(|s| s.to_string());
                if let Some(issuer) = issuer {
                    let resource = meta.get("resource").and_then(|r| r.as_str()).unwrap_or(server_url).to_string();
                    let scopes = string_list(meta.get("scopes_supported"));
                    found = Some((issuer, resource, scopes));
                    break;
                }
            }
        }
        // Fallback: some servers don't publish protected-resource metadata — treat the server origin
        // as the issuer and discover AS metadata there directly.
        found.unwrap_or_else(|| (origin_of(server_url).unwrap_or_else(|_| server_url.to_string()), server_url.to_string(), Vec::new()))
    };

    // 2. Fetch the authorization-server metadata.
    let mut meta_json: Option<Value> = None;
    for url in as_metadata_candidates(&issuer) {
        if let Ok(m) = fetch_json(client, &url).await {
            if m.get("authorization_endpoint").and_then(|v| v.as_str()).is_some() {
                meta_json = Some(m);
                break;
            }
        }
    }
    let meta = meta_json.with_context(|| {
        format!("couldn't discover OAuth metadata for authorization server '{issuer}' (no /.well-known/oauth-authorization-server)")
    })?;

    let authorization_endpoint =
        meta.get("authorization_endpoint").and_then(|v| v.as_str()).context("AS metadata missing authorization_endpoint")?.to_string();
    let token_endpoint =
        meta.get("token_endpoint").and_then(|v| v.as_str()).context("AS metadata missing token_endpoint")?.to_string();
    let registration_endpoint = meta.get("registration_endpoint").and_then(|v| v.as_str()).map(|s| s.to_string());
    if scopes.is_empty() {
        scopes = string_list(meta.get("scopes_supported"));
    }

    Ok(AuthMeta { authorization_endpoint, token_endpoint, registration_endpoint, scopes_supported: scopes, resource })
}

fn string_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(|x| x.as_array()).map(|a| a.iter().filter_map(|s| s.as_str().map(|s| s.to_string())).collect()).unwrap_or_default()
}

// ───────────────────────────── dynamic client registration (RFC 7591) ─────────────────────────────

async fn register_client(client: &reqwest::Client, reg_endpoint: &str, redirect_uri: &str, scopes: &[String]) -> Result<(String, Option<String>)> {
    let body = json!({
        "client_name": "Aizen CLI",
        "redirect_uris": [redirect_uri],
        "grant_types": ["authorization_code", "refresh_token"],
        "response_types": ["code"],
        "token_endpoint_auth_method": "none",
        "scope": scopes.join(" "),
    });
    let resp = client.post(reg_endpoint).json(&body).send().await.with_context(|| format!("POST {reg_endpoint}"))?;
    let status = resp.status();
    let v: Value = resp.json().await.context("parsing client-registration response")?;
    if !status.is_success() {
        let err = v.get("error_description").or_else(|| v.get("error")).and_then(|e| e.as_str()).unwrap_or("unknown error");
        bail!("dynamic client registration → HTTP {}: {err}", status.as_u16());
    }
    let client_id = v.get("client_id").and_then(|x| x.as_str()).context("registration response missing client_id")?.to_string();
    let client_secret = v.get("client_secret").and_then(|x| x.as_str()).map(|s| s.to_string());
    Ok((client_id, client_secret))
}

// ───────────────────────────── token requests ─────────────────────────────

struct RawToken {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

fn parse_token_response(v: &Value) -> Result<RawToken> {
    let access_token = v.get("access_token").and_then(|x| x.as_str()).context("token response missing access_token")?.to_string();
    let refresh_token = v.get("refresh_token").and_then(|x| x.as_str()).map(|s| s.to_string());
    let expires_in = v.get("expires_in").and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|s| s.parse().ok())));
    Ok(RawToken { access_token, refresh_token, expires_in })
}

async fn post_token(client: &reqwest::Client, token_endpoint: &str, form: &[(&str, &str)]) -> Result<RawToken> {
    let resp = client.post(token_endpoint).form(form).send().await.with_context(|| format!("POST {token_endpoint}"))?;
    let status = resp.status();
    let body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let detail = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|v| v.get("error_description").or_else(|| v.get("error")).and_then(|e| e.as_str()).map(|s| s.to_string()))
            .unwrap_or_else(|| body.chars().take(200).collect());
        bail!("token endpoint → HTTP {}: {detail}", status.as_u16());
    }
    let v: Value = serde_json::from_str(&body).context("parsing token response JSON")?;
    parse_token_response(&v)
}

// ───────────────────────────── loopback redirect catcher ─────────────────────────────

/// Open the user's default browser at `url` (best effort — also printed so they can click it).
fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let _ = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

async fn respond(sock: &mut tokio::net::TcpStream, message: &str) {
    let body = format!(
        "<!doctype html><meta charset=utf-8><title>Aizen</title>\
         <body style=\"font-family:system-ui;background:#0b0b0c;color:#eee;display:grid;place-items:center;height:100vh;margin:0\">\
         <div style=\"text-align:center\"><h2 style=\"color:#e3b341\">Aizen</h2><p>{message}</p></div>"
    );
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = sock.write_all(resp.as_bytes()).await;
    let _ = sock.flush().await;
}

/// Wait for the authorization redirect on the loopback listener; return the `code`, validating
/// `state`. Ignores stray hits (favicon, etc.) until the real callback arrives or we time out.
async fn wait_for_code(listener: tokio::net::TcpListener, expected_state: &str) -> Result<String> {
    let deadline = tokio::time::Instant::now() + AUTH_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            bail!("timed out after {}s waiting for the sign-in redirect", AUTH_TIMEOUT.as_secs());
        }
        let (mut sock, _) = match tokio::time::timeout(remaining, listener.accept()).await {
            Ok(Ok(pair)) => pair,
            Ok(Err(e)) => bail!("loopback accept failed: {e}"),
            Err(_) => bail!("timed out after {}s waiting for the sign-in redirect", AUTH_TIMEOUT.as_secs()),
        };
        // Accumulate across reads until we have at least the request line (CRLF). A single read() is
        // NOT guaranteed to deliver the whole line — when the auth server appends a long
        // state/code/iss query or a proxy segments the request, the first read can arrive truncated,
        // which used to parse a partial path, find no `code`, and hang sign-in to the 300s timeout.
        let mut data: Vec<u8> = Vec::with_capacity(8192);
        let mut tmp = [0u8; 4096];
        loop {
            match tokio::time::timeout(std::time::Duration::from_secs(5), sock.read(&mut tmp)).await {
                Ok(Ok(0)) => break, // EOF
                Ok(Ok(m)) => {
                    data.extend_from_slice(&tmp[..m]);
                    // Have the request line (CRLF), or hit a sane cap → stop reading and parse.
                    if data.windows(2).any(|w| w == b"\r\n") || data.len() >= 16 * 1024 {
                        break;
                    }
                }
                _ => break, // read error or a stalled client (per-read timeout) — parse what we have
            }
        }
        let req = String::from_utf8_lossy(&data);
        let path = req.lines().next().and_then(|l| l.split_whitespace().nth(1)).unwrap_or("/");
        let parsed = url::Url::parse(&format!("http://127.0.0.1{path}")).ok();
        let (mut code, mut state, mut err) = (None, None, None);
        if let Some(u) = &parsed {
            for (k, v) in u.query_pairs() {
                match k.as_ref() {
                    "code" => code = Some(v.to_string()),
                    "state" => state = Some(v.to_string()),
                    "error" => err = Some(v.to_string()),
                    _ => {}
                }
            }
        }
        if code.is_none() && err.is_none() {
            respond(&mut sock, "Waiting for the authorization redirect…").await; // favicon / preflight — keep listening
            continue;
        }
        if let Some(e) = err {
            respond(&mut sock, &format!("Authorization failed: {e}. You can close this tab.")).await;
            bail!("authorization server returned error: {e}");
        }
        if state.as_deref() != Some(expected_state) {
            respond(&mut sock, "State mismatch — sign-in aborted (possible CSRF).").await;
            bail!("OAuth state mismatch (possible CSRF) — aborted");
        }
        respond(&mut sock, "Signed in. You can close this tab and return to Aizen.").await;
        return Ok(code.unwrap());
    }
}

// ───────────────────────────── public entry points ─────────────────────────────

/// Run the full interactive OAuth sign-in for one server key and cache the token. `server_url` is the
/// MCP endpoint; `www_authenticate` is its `401` challenge if the caller already has it (else None →
/// discovery falls back to the well-known location).
pub async fn authorize(key: &str, server_url: &str, cfg: &OAuthConfig, www_authenticate: Option<String>) -> Result<TokenSet> {
    let client = http_client()?;
    let meta = discover(&client, server_url, www_authenticate.as_deref(), cfg).await?;

    // Bind the loopback FIRST so we register the exact redirect URI the AS will see.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.context("binding loopback callback port")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}/callback");

    let scopes = if !cfg.scopes.is_empty() { cfg.scopes.clone() } else { meta.scopes_supported.clone() };

    // Resolve the client id: a pinned one (Google/Atlassian) or dynamic registration.
    let (client_id, client_secret) = match &cfg.client_id {
        Some(id) => (id.clone(), cfg.client_secret.clone()),
        None => {
            let reg = meta.registration_endpoint.clone().context(
                "this server requires OAuth but supports no dynamic client registration — set \"oauth\": {\"client_id\": …} in mcp.json",
            )?;
            register_client(&client, &reg, &redirect_uri, &scopes).await?
        }
    };

    let (verifier, challenge) = pkce()?;
    let state = rand_b64url(24)?;

    let mut au = url::Url::parse(&meta.authorization_endpoint).context("bad authorization_endpoint")?;
    {
        let mut q = au.query_pairs_mut();
        q.append_pair("response_type", "code");
        q.append_pair("client_id", &client_id);
        q.append_pair("redirect_uri", &redirect_uri);
        q.append_pair("code_challenge", &challenge);
        q.append_pair("code_challenge_method", "S256");
        q.append_pair("state", &state);
        if !scopes.is_empty() {
            q.append_pair("scope", &scopes.join(" "));
        }
        if !meta.resource.is_empty() {
            q.append_pair("resource", &meta.resource);
        }
    }
    let auth_url = au.to_string();

    eprintln!("{}", console::style("Opening your browser to sign in… (if it doesn't open, paste this URL):").dim());
    eprintln!("{}", console::style(&auth_url).cyan());
    open_browser(&auth_url);

    let code = wait_for_code(listener, &state).await?;

    // Exchange the code for tokens.
    let mut form: Vec<(&str, &str)> = vec![
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("redirect_uri", &redirect_uri),
        ("client_id", &client_id),
        ("code_verifier", &verifier),
    ];
    if !meta.resource.is_empty() {
        form.push(("resource", &meta.resource));
    }
    if let Some(sec) = &client_secret {
        form.push(("client_secret", sec));
    }
    let raw = post_token(&client, &meta.token_endpoint, &form).await.context("exchanging authorization code for a token")?;

    let token = TokenSet {
        access_token: raw.access_token,
        refresh_token: raw.refresh_token,
        expires_at: raw.expires_in.map(|s| now() + s),
        token_endpoint: meta.token_endpoint,
        client_id,
        client_secret,
        scope: Some(scopes.join(" ")).filter(|s| !s.is_empty()),
        resource: Some(meta.resource).filter(|s| !s.is_empty()),
    };
    save_token(key, &token)?;
    Ok(token)
}

/// Refresh an expired/invalid access token using the cached refresh token; persist + return the new
/// set. The refresh token is preserved if the server doesn't rotate it.
pub async fn refresh(key: &str, t: &TokenSet) -> Result<TokenSet> {
    let rt = t.refresh_token.clone().context("no refresh token — run `aizen apps login` again")?;
    let client = http_client()?;
    let scope = t.scope.clone().unwrap_or_default();
    let resource = t.resource.clone().unwrap_or_default();
    let mut form: Vec<(&str, &str)> =
        vec![("grant_type", "refresh_token"), ("refresh_token", &rt), ("client_id", &t.client_id)];
    if !scope.is_empty() {
        form.push(("scope", &scope));
    }
    if !resource.is_empty() {
        form.push(("resource", &resource));
    }
    if let Some(sec) = &t.client_secret {
        form.push(("client_secret", sec));
    }
    let raw = post_token(&client, &t.token_endpoint, &form).await.context("refreshing the OAuth token")?;
    let token = TokenSet {
        access_token: raw.access_token,
        refresh_token: raw.refresh_token.or_else(|| t.refresh_token.clone()),
        expires_at: raw.expires_in.map(|s| now() + s),
        token_endpoint: t.token_endpoint.clone(),
        client_id: t.client_id.clone(),
        client_secret: t.client_secret.clone(),
        scope: t.scope.clone(),
        resource: t.resource.clone(),
    };
    save_token(key, &token)?;
    Ok(token)
}

// ───────────────────────────── tests (pure, offline) ─────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn b64url_no_padding_known_vectors() {
        assert_eq!(b64url(b""), "");
        assert_eq!(b64url(b"f"), "Zg");
        assert_eq!(b64url(b"fo"), "Zm8");
        assert_eq!(b64url(b"foo"), "Zm9v");
        assert_eq!(b64url(b"foob"), "Zm9vYg");
        // url-safe alphabet: bytes that would be + / in standard base64 become - _
        assert_eq!(b64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn pkce_matches_rfc7636_appendix_b_vector() {
        // RFC 7636 Appendix B: verifier → SHA-256 → base64url(no pad) == this challenge.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = b64url(&sha256(verifier.as_bytes()));
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
    }

    #[test]
    fn pkce_generates_distinct_in_range_verifiers() {
        let (v1, c1) = pkce().unwrap();
        let (v2, _) = pkce().unwrap();
        assert!(v1.len() >= 43 && v1.len() <= 128, "verifier length {} out of PKCE range", v1.len());
        assert_ne!(v1, v2, "two verifiers must differ (CSPRNG)");
        // challenge is deterministic from the verifier
        assert_eq!(c1, b64url(&sha256(v1.as_bytes())));
    }

    #[test]
    fn parse_resource_metadata_extracts_quoted_url() {
        let h = r#"Bearer error="invalid_token", resource_metadata="https://mcp.example.com/.well-known/oauth-protected-resource""#;
        assert_eq!(parse_resource_metadata(h).as_deref(), Some("https://mcp.example.com/.well-known/oauth-protected-resource"));
        assert_eq!(parse_resource_metadata("Bearer realm=\"x\"").as_deref(), None);
    }

    #[test]
    fn prm_candidates_use_header_then_wellknown_with_path() {
        let c = prm_candidates("https://mcp.linear.app/sse", Some(r#"Bearer resource_metadata="https://h/x""#));
        assert_eq!(c[0], "https://h/x", "WWW-Authenticate value is tried first");
        assert!(c.iter().any(|u| u == "https://mcp.linear.app/.well-known/oauth-protected-resource/sse"));
        assert!(c.iter().any(|u| u == "https://mcp.linear.app/.well-known/oauth-protected-resource"));
    }

    #[test]
    fn as_metadata_candidates_handle_root_and_path_issuers() {
        let root = as_metadata_candidates("https://auth.example.com");
        assert_eq!(root[0], "https://auth.example.com/.well-known/oauth-authorization-server");
        assert!(root.iter().any(|u| u == "https://auth.example.com/.well-known/openid-configuration"));

        let pathed = as_metadata_candidates("https://auth.example.com/tenant1");
        assert!(pathed.iter().any(|u| u == "https://auth.example.com/.well-known/oauth-authorization-server/tenant1"));
        assert!(pathed.iter().any(|u| u == "https://auth.example.com/tenant1/.well-known/oauth-authorization-server"));
    }

    #[test]
    fn origin_of_strips_path_keeps_port() {
        assert_eq!(origin_of("https://mcp.notion.com/v1/mcp").unwrap(), "https://mcp.notion.com");
        assert_eq!(origin_of("http://127.0.0.1:8080/mcp").unwrap(), "http://127.0.0.1:8080");
    }

    #[test]
    fn token_set_expiry_with_margin() {
        let base = TokenSet {
            access_token: "a".into(),
            refresh_token: Some("r".into()),
            expires_at: Some(now() + 3600),
            token_endpoint: "t".into(),
            client_id: "c".into(),
            client_secret: None,
            scope: None,
            resource: None,
        };
        assert!(!base.is_expired(), "an hour out → live");
        let soon = TokenSet { expires_at: Some(now() + 30), ..base.clone() };
        assert!(soon.is_expired(), "inside the {}s margin → treat as expired", EXPIRY_MARGIN_SECS);
        let never = TokenSet { expires_at: None, ..base };
        assert!(!never.is_expired(), "no stated expiry → not proactively expired");
    }

    #[test]
    fn parse_token_response_reads_fields_and_string_expiry() {
        let v = json!({"access_token":"AT","refresh_token":"RT","expires_in":3600});
        let r = parse_token_response(&v).unwrap();
        assert_eq!(r.access_token, "AT");
        assert_eq!(r.refresh_token.as_deref(), Some("RT"));
        assert_eq!(r.expires_in, Some(3600));
        // some servers send expires_in as a string
        let r2 = parse_token_response(&json!({"access_token":"X","expires_in":"7200"})).unwrap();
        assert_eq!(r2.expires_in, Some(7200));
        assert!(r2.refresh_token.is_none());
        assert!(parse_token_response(&json!({"no":"token"})).is_err());
    }
}
