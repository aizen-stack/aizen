//! Telegram bot integration — pure-Rust, DIY over reqwest + serde (NO teloxide → lean binary,
//! auditable token handling). Powers the `ng serve` daemon, the `telegram_send`/`telegram_ask`
//! agent tools, and phone-approval of destructive ops. Long-poll only (`getUpdates`) — no webhook,
//! no public URL, works behind NAT.
//!
//! Approval routing: when running under `ng serve` (a single poll loop owns `getUpdates`), an
//! approval request registers a `oneshot` in a global PENDING map; the poll loop delivers the
//! callback → resolves it. Standalone (`ng agent`, no daemon) there is no other poller, so the
//! request self-polls `getUpdates` for its own callback. Both deny on a 5-minute timeout.

use anyhow::{bail, Context, Result};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::oneshot;

use crate::agent::tools::Tool;
use crate::core::cli_config::{self, TelegramConfig};

const API_BASE: &str = "https://api.telegram.org/bot";
const APPROVAL_TIMEOUT_SECS: u64 = 300;
/// getUpdates long-poll seconds (HTTP client timeout must exceed this).
pub const POLL_TIMEOUT_SECS: u64 = 30;

// ── wire types (the subset of the Bot API we use) ──────────────────────────────

#[derive(Debug, Deserialize)]
pub struct Update {
    pub update_id: i64,
    #[serde(default)]
    pub message: Option<Message>,
    #[serde(default)]
    pub callback_query: Option<CallbackQuery>,
}

#[derive(Debug, Deserialize)]
pub struct Message {
    pub message_id: i64,
    pub chat: Chat,
    #[serde(default)]
    pub text: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Chat {
    pub id: i64,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    pub id: String,
    #[serde(default)]
    pub data: Option<String>,
    #[serde(default)]
    pub message: Option<Message>,
    pub from: User,
}

#[derive(Debug, Deserialize)]
pub struct User {
    pub id: i64,
}

// ── client ─────────────────────────────────────────────────────────────────────

pub struct Client {
    http: reqwest::Client,
    token: String,
}

impl Client {
    pub fn new(token: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(POLL_TIMEOUT_SECS + 40))
            .build()
            .context("building telegram HTTP client")?;
        Ok(Self { http, token })
    }

    fn url(&self, method: &str) -> String {
        format!("{API_BASE}{}/{method}", self.token)
    }

    async fn call<T: for<'de> Deserialize<'de>>(&self, method: &str, body: Value) -> Result<T> {
        let resp = self
            .http
            .post(self.url(method))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("telegram {method} request failed"))?;
        let v: Value = resp.json().await.with_context(|| format!("parsing {method} response"))?;
        if !v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
            let desc = v.get("description").and_then(|d| d.as_str()).unwrap_or("unknown error");
            bail!("telegram {method} failed: {desc}");
        }
        let result = v.get("result").cloned().unwrap_or(Value::Null);
        serde_json::from_value(result).with_context(|| format!("parsing {method} result"))
    }

    pub async fn get_updates(&self, offset: i64, timeout: u64) -> Result<Vec<Update>> {
        self.call(
            "getUpdates",
            json!({"offset": offset, "timeout": timeout, "allowed_updates": ["message", "callback_query"]}),
        )
        .await
    }

    /// Send a plain-text message (no parse_mode → a stray `*`/`_` never breaks delivery).
    pub async fn send_message(&self, chat_id: i64, text: &str) -> Result<Message> {
        self.call(
            "sendMessage",
            json!({"chat_id": chat_id, "text": text, "disable_web_page_preview": true}),
        )
        .await
    }

    /// Send a message with a single inline-keyboard row of (label, callback_data) buttons.
    pub async fn send_keyboard(&self, chat_id: i64, text: &str, buttons: &[(&str, String)]) -> Result<Message> {
        let row: Vec<Value> =
            buttons.iter().map(|(label, data)| json!({"text": label, "callback_data": data})).collect();
        self.call(
            "sendMessage",
            json!({"chat_id": chat_id, "text": text, "reply_markup": {"inline_keyboard": [row]}}),
        )
        .await
    }

    pub async fn answer_callback(&self, callback_id: &str, text: &str) -> Result<()> {
        let _: Value = self
            .call("answerCallbackQuery", json!({"callback_query_id": callback_id, "text": text}))
            .await?;
        Ok(())
    }

    pub async fn edit_text(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
        let _: Value = self
            .call("editMessageText", json!({"chat_id": chat_id, "message_id": message_id, "text": text}))
            .await?;
        Ok(())
    }

    /// `getMe` — used by `ng telegram test` to validate the token.
    pub async fn get_me(&self) -> Result<String> {
        let v: Value = self.call("getMe", json!({})).await?;
        Ok(v.get("username").and_then(|u| u.as_str()).unwrap_or("?").to_string())
    }
}

// ── config plumbing ────────────────────────────────────────────────────────────

/// Build a client + config if Telegram is set up (token + at least one allowed chat).
pub fn configured() -> Option<(Client, TelegramConfig)> {
    let cfg = cli_config::load().telegram?;
    let token = cfg.resolved_token()?;
    let client = Client::new(token).ok()?;
    Some((client, cfg))
}

/// Cheap check (no client build) used to decide whether to advertise the telegram tools.
pub fn is_configured() -> bool {
    cli_config::load()
        .telegram
        .map(|t| t.resolved_token().is_some() && !t.allowed_chat_ids.is_empty())
        .unwrap_or(false)
}

pub fn first_chat(cfg: &TelegramConfig) -> Option<i64> {
    cfg.allowed_chat_ids.first().copied()
}

/// Allowlist: an empty list denies everyone (secure default — setup populates it).
pub fn is_allowed(cfg: &TelegramConfig, chat_id: i64) -> bool {
    cfg.allowed_chat_ids.contains(&chat_id)
}

// ── approval registry (daemon path) ──────────────────────────────────────────────

static PENDING: Lazy<Mutex<HashMap<String, oneshot::Sender<bool>>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static DAEMON_ACTIVE: AtomicBool = AtomicBool::new(false);
static APPROVAL_SEQ: AtomicU64 = AtomicU64::new(0);

pub fn set_daemon_active(v: bool) {
    DAEMON_ACTIVE.store(v, Ordering::SeqCst);
}
/// True while `ng serve` owns the single poll loop (so approvals route through PENDING, not a
/// conflicting self-poll). Public so the agent approval gate can decide to route to the phone.
pub fn daemon_is_active() -> bool {
    DAEMON_ACTIVE.load(Ordering::SeqCst)
}
fn next_approval_id() -> String {
    format!("{}-{}", std::process::id(), APPROVAL_SEQ.fetch_add(1, Ordering::SeqCst))
}

/// Called by the serve poll loop when a callback arrives → resolve a pending approval.
/// Returns true if an approval was waiting on this id.
pub fn resolve_approval(id: &str, approved: bool) -> bool {
    if let Some(tx) = PENDING.lock().unwrap().remove(id) {
        let _ = tx.send(approved);
        true
    } else {
        false
    }
}

/// Parse our approval callback_data: `ng/appr/<id>/y` | `.../n` → (id, approved).
pub fn parse_callback(data: &str) -> Option<(String, bool)> {
    let rest = data.strip_prefix("ng/appr/")?;
    let (id, yn) = rest.rsplit_once('/')?;
    match yn {
        "y" => Some((id.to_string(), true)),
        "n" => Some((id.to_string(), false)),
        _ => None,
    }
}

/// Ask the owner to approve something via Telegram. `Some(approved)` if Telegram handled it,
/// `None` if not configured / no allowed chat. Deny on 5-minute timeout.
pub async fn request_approval(prompt: &str) -> Option<bool> {
    let (client, cfg) = configured()?;
    let chat = first_chat(&cfg)?;
    let id = next_approval_id();
    let text = format!("🔐 Approve this action?\n\n{prompt}");
    let buttons = [("✅ Approve", format!("ng/appr/{id}/y")), ("❌ Deny", format!("ng/appr/{id}/n"))];
    let msg = client.send_keyboard(chat, &text, &buttons).await.ok()?;

    let approved = if daemon_is_active() {
        let (tx, rx) = oneshot::channel();
        PENDING.lock().unwrap().insert(id.clone(), tx);
        let r = tokio::time::timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS), rx).await;
        PENDING.lock().unwrap().remove(&id); // cleanup on timeout
        matches!(r, Ok(Ok(true)))
    } else {
        self_poll_for_approval(&client, &id).await
    };

    let verdict = if approved { "✅ Approved" } else { "❌ Denied / timed out" };
    let _ = client.edit_text(chat, msg.message_id, &format!("{text}\n\n{verdict}")).await;
    Some(approved)
}

/// Standalone path: no daemon poll loop, so poll `getUpdates` ourselves for our callback.
async fn self_poll_for_approval(client: &Client, id: &str) -> bool {
    let start = tokio::time::Instant::now();
    let mut offset = 0i64;
    let mut warned_conflict = false;
    while start.elapsed() < Duration::from_secs(APPROVAL_TIMEOUT_SECS) {
        let updates = match client.get_updates(offset, 20).await {
            Ok(u) => u,
            Err(e) => {
                // 409 Conflict = another process (almost always a running `ng serve` daemon) already
                // owns getUpdates for this bot token. Telegram allows only ONE poller per token, so
                // the two steal each other's callbacks and approvals get lost. Surface it ONCE
                // (instead of a silent 1s retry storm) and back off — the owner should approve in the
                // daemon, or stop one of the two processes. (A cross-process daemon lockfile so the
                // standalone never self-polls when a daemon owns the token is the complete fix.)
                if !warned_conflict && e.to_string().contains("409") {
                    warned_conflict = true;
                    eprintln!(
                        "[telegram] getUpdates 409 Conflict — another process (likely `aizen serve`) is \
                         already polling this bot token; this standalone approval may not receive the \
                         callback. Approve in that process, or stop it."
                    );
                }
                tokio::time::sleep(Duration::from_secs(if warned_conflict { 3 } else { 1 })).await;
                continue;
            }
        };
        for u in &updates {
            offset = offset.max(u.update_id + 1);
        }
        for u in updates {
            if let Some(cb) = u.callback_query {
                if let Some((cid, approved)) = cb.data.as_deref().and_then(parse_callback) {
                    if cid == id {
                        let _ = client.answer_callback(&cb.id, if approved { "Approved" } else { "Denied" }).await;
                        return approved;
                    }
                }
            }
        }
    }
    false
}

/// Bridge an async future to the sync `Tool::execute` path — the shared cancel-aware bridge
/// (valid on workers AND spawn_blocking threads; Esc aborts an in-flight send or a pending
/// 5-minute approval wait instead of blocking the turn on it).
fn block<T>(f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    crate::agent::tools::block_for_tool(f)
}

// ── agent tools ──────────────────────────────────────────────────────────────────

pub struct TelegramSend;
impl Tool for TelegramSend {
    fn name(&self) -> &str {
        "telegram_send"
    }
    fn description(&self) -> &str {
        "Send a notification to the owner's Telegram (progress, result, alert). Use when running \
         unattended so the owner sees what happened. Not for asking a question → use telegram_ask."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let text = args.get("text").and_then(|v| v.as_str()).context("missing required string arg 'text'")?;
        block(async {
            let (client, cfg) = configured().context("telegram not configured (run `aizen telegram setup`)")?;
            let chat = first_chat(&cfg).context("no allowed_chat_ids configured")?;
            client.send_message(chat, text).await?;
            anyhow::Ok(())
        })?;
        Ok("sent to telegram".to_string())
    }
}

pub struct TelegramAsk;
impl Tool for TelegramAsk {
    fn name(&self) -> &str {
        "telegram_ask"
    }
    fn description(&self) -> &str {
        "Ask the owner to APPROVE or DENY an action via Telegram (inline ✓/✗ buttons); returns \
         their answer. Use before a risky/irreversible step when running unattended. Denies on a \
         5-minute timeout."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"question": {"type": "string"}},
            "required": ["question"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let q = args.get("question").and_then(|v| v.as_str()).context("missing required string arg 'question'")?;
        match block(async { anyhow::Ok(request_approval(q).await) })? {
            Some(true) => Ok("approved".to_string()),
            Some(false) => Ok("denied (or timed out)".to_string()),
            None => Ok("error: telegram not configured (run `aizen telegram setup`)".to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_callback_roundtrip() {
        assert_eq!(parse_callback("ng/appr/123-4/y"), Some(("123-4".to_string(), true)));
        assert_eq!(parse_callback("ng/appr/abc/n"), Some(("abc".to_string(), false)));
        assert_eq!(parse_callback("ng/appr/x/maybe"), None);
        assert_eq!(parse_callback("other/y"), None);
    }

    #[test]
    fn allowlist_denies_unknown_and_empty() {
        let mut cfg = TelegramConfig::default();
        assert!(!is_allowed(&cfg, 42), "empty allowlist denies everyone");
        cfg.allowed_chat_ids = vec![7, 9];
        assert!(is_allowed(&cfg, 7));
        assert!(!is_allowed(&cfg, 8));
    }

    #[test]
    fn resolve_unknown_approval_is_false() {
        assert!(!resolve_approval("no-such-id", true));
    }

    #[test]
    fn tools_are_serial_and_nondestructive() {
        // block_in_place forbids the parallel scoped-thread path.
        assert!(!TelegramSend.is_concurrency_safe());
        assert!(!TelegramAsk.is_concurrency_safe());
        // telegram_ask is the approval mechanism; it is not itself approval-gated.
        assert!(!TelegramSend.is_destructive());
        assert!(!TelegramAsk.is_destructive());
    }
}
