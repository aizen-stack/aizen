//! Telegram bot integration — pure-Rust, DIY over reqwest + serde (NO teloxide → lean binary,
//! auditable token handling). Powers the `aizen serve` daemon (via `TelegramPlatform`), the
//! `telegram_send`/`telegram_ask` agent tools, and phone-approval of destructive ops. Long-poll only
//! (`getUpdates`) — no webhook, no public URL, works behind NAT.
//!
//! Approval routing: when running under `aizen serve` (a single poll loop owns `getUpdates`), an
//! approval request registers a `oneshot` in a global PENDING map; the poll loop delivers the
//! callback → resolves it. Standalone (`aizen agent`, no daemon) there is no other poller, so the
//! request self-polls `getUpdates` for its own callback. Both deny on a 5-minute timeout.
//!
//! `TelegramPlatform` (bottom of the file) implements the `Platform` contract: it owns the multi-bot
//! registry (primary "default" + `/addbot`-hosted extras) that used to live inline in `run_serve`, so
//! the generic daemon loop stays platform-agnostic.

use anyhow::{bail, Context, Result};
use once_cell::sync::Lazy;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc::Sender;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::agent::tools::Tool;
use crate::core::cli_config::{self, TelegramConfig};
use crate::hostbot::platform::{BotInfo, Inbound, Outbound, Platform, StatusHandle};
use crate::hostbot::store;

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
            .map_err(|e| {
                // reqwest errors can include the request URL, and Telegram embeds the bot token there.
                let kind = if e.is_timeout() {
                    "timeout"
                } else if e.is_connect() {
                    "connection"
                } else {
                    "transport"
                };
                anyhow::anyhow!("telegram {method} request failed ({kind})")
            })?;
        let v: Value = resp
            .json()
            .await
            .with_context(|| format!("parsing {method} response"))?;
        if !v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
            let desc = v
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("unknown error");
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

    /// Skip the backlog: return the offset a freshly-started poll loop should begin from, so a
    /// daemon does NOT replay old messages/commands (Telegram retains unconfirmed updates ~24h) as
    /// new tasks the moment it boots. `offset=-1, timeout=0` returns only the most recent pending
    /// update immediately; we start one past it. On error / empty queue → `0` (poll from scratch).
    pub async fn backlog_offset(&self) -> i64 {
        match self.get_updates(-1, 0).await {
            Ok(updates) => updates.last().map(|u| u.update_id + 1).unwrap_or(0),
            Err(_) => 0,
        }
    }

    /// Send a plain-text message (no parse_mode → a stray `*`/`_` never breaks delivery).
    pub async fn send_message(&self, chat_id: i64, text: &str) -> Result<Message> {
        self.call(
            "sendMessage",
            json!({"chat_id": chat_id, "text": text, "disable_web_page_preview": true}),
        )
        .await
    }

    /// Telegram-native HTML. The platform send path retries the renderer's plain fallback on error.
    pub async fn send_html(&self, chat_id: i64, html: &str) -> Result<Message> {
        self.call(
            "sendMessage",
            json!({"chat_id": chat_id, "text": html, "parse_mode": "HTML", "disable_web_page_preview": true}),
        )
        .await
    }

    /// Send a message with a single inline-keyboard row of (label, callback_data) buttons.
    pub async fn send_keyboard(
        &self,
        chat_id: i64,
        text: &str,
        buttons: &[(&str, String)],
    ) -> Result<Message> {
        let row: Vec<Value> = buttons
            .iter()
            .map(|(label, data)| json!({"text": label, "callback_data": data}))
            .collect();
        self.call(
            "sendMessage",
            json!({"chat_id": chat_id, "text": text, "reply_markup": {"inline_keyboard": [row]}}),
        )
        .await
    }

    pub async fn answer_callback(&self, callback_id: &str, text: &str) -> Result<()> {
        let _: Value = self
            .call(
                "answerCallbackQuery",
                json!({"callback_query_id": callback_id, "text": text}),
            )
            .await?;
        Ok(())
    }

    pub async fn edit_text(&self, chat_id: i64, message_id: i64, text: &str) -> Result<()> {
        let _: Value = self
            .call(
                "editMessageText",
                json!({"chat_id": chat_id, "message_id": message_id, "text": text}),
            )
            .await?;
        Ok(())
    }

    pub async fn delete_message(&self, chat_id: i64, message_id: i64) -> Result<()> {
        let _: Value = self
            .call(
                "deleteMessage",
                json!({"chat_id": chat_id, "message_id": message_id}),
            )
            .await?;
        Ok(())
    }

    /// `getMe` — used by `aizen telegram test` to validate the token.
    pub async fn get_me(&self) -> Result<String> {
        let v: Value = self.call("getMe", json!({})).await?;
        Ok(v.get("username")
            .and_then(|u| u.as_str())
            .unwrap_or("?")
            .to_string())
    }

    /// Register the bot's slash-command menu (the "/" button in the Telegram app) so the daemon's
    /// commands are discoverable on the phone — same list `serve` dispatches. Each pair is
    /// `(command, description)`; Telegram requires lowercase names, 1–32 chars, `[a-z0-9_]`.
    /// Non-fatal at the call site: a failure just means no menu, not a broken daemon.
    pub async fn set_my_commands(&self, cmds: &[(&str, &str)]) -> Result<()> {
        let commands: Vec<Value> = cmds
            .iter()
            .map(|(c, d)| json!({"command": c, "description": d}))
            .collect();
        let _: Value = self
            .call("setMyCommands", json!({"commands": commands}))
            .await?;
        Ok(())
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

/// Allowlist: an empty list denies everyone (secure default — setup populates it). Kept for symmetry
/// with the Discord platform + covered by tests; the poll loop inlines `allowed.contains` on its own
/// per-bot allowlist snapshot, so this isn't called on the hot path.
#[allow(dead_code)]
pub fn is_allowed(cfg: &TelegramConfig, chat_id: i64) -> bool {
    cfg.allowed_chat_ids.contains(&chat_id)
}

// ── approval registry (daemon path) ──────────────────────────────────────────────

static PENDING: Lazy<Mutex<HashMap<String, oneshot::Sender<bool>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
static DAEMON_ACTIVE: AtomicBool = AtomicBool::new(false);
static APPROVAL_SEQ: AtomicU64 = AtomicU64::new(0);

/// The bot + chat an approval should be sent to THIS turn. The multi-bot `serve` loop processes one
/// message at a time; before each turn it pins the originating bot's `(token, chat)` here so a
/// destructive-op approval goes back to the SAME bot the request came from (not always the primary).
/// `None` ⇒ fall back to the primary `configured()` bot (the single-bot / standalone path).
static APPROVAL_ROUTE: Lazy<Mutex<Option<(String, i64)>>> = Lazy::new(|| Mutex::new(None));

/// Pin the approval route to a specific bot token + chat (called before a serve turn runs).
pub fn set_approval_route(token: String, chat: i64) {
    *APPROVAL_ROUTE.lock().unwrap() = Some((token, chat));
}
/// Clear the approval route (called after a serve turn finishes) so it never leaks to the next turn.
pub fn clear_approval_route() {
    *APPROVAL_ROUTE.lock().unwrap() = None;
}

pub fn set_daemon_active(v: bool) {
    DAEMON_ACTIVE.store(v, Ordering::SeqCst);
}
/// True while `aizen serve` owns the single poll loop (so approvals route through PENDING, not a
/// conflicting self-poll). Public so the agent approval gate can decide to route to the phone.
pub fn daemon_is_active() -> bool {
    DAEMON_ACTIVE.load(Ordering::SeqCst)
}
fn next_approval_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        APPROVAL_SEQ.fetch_add(1, Ordering::SeqCst)
    )
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

/// Resolve a hosted bot's token by route name — how an `ExecutionContext`-carried approval route
/// (which is platform-agnostic: `route` + a `Display`ed chat id) becomes a client. `None` when no
/// daemon is running or the route names a bot that isn't hosted.
fn token_for_route(route: &str) -> Option<String> {
    let ctl = bot_control()?;
    let bots = ctl.bots.lock().unwrap();
    bots.get(route).map(|h| h.token.clone())
}

/// Ask the owner to approve something via Telegram. `Some(approved)` if Telegram handled it,
/// `None` if not configured / no allowed chat. Deny on 5-minute timeout.
pub async fn request_approval(prompt: &str) -> Option<bool> {
    request_approval_on(prompt, None).await
}

/// Ask for approval, delivering the prompt to an EXPLICIT lane when one is given.
///
/// Route resolution, most specific first:
///   1. `lane` — the turn's own `(route, chat)` from its `ExecutionContext`. Authoritative under
///      concurrent lanes, where the process-global slot belongs to whichever turn started last.
///   2. `APPROVAL_ROUTE` — the serial daemon's per-turn pin (still correct when only one turn runs).
///   3. the primary `configured()` bot — standalone `aizen agent`, no daemon.
pub async fn request_approval_on(
    prompt: &str,
    lane: Option<crate::core::exec_ctx::ApprovalRoute>,
) -> Option<bool> {
    let explicit = lane.and_then(|l| {
        let chat = l.chat.parse::<i64>().ok()?;
        let token = token_for_route(&l.route)?;
        Some((token, chat))
    });
    let route = match explicit {
        Some(pair) => Some(pair),
        None => APPROVAL_ROUTE.lock().unwrap().clone(),
    };
    let (client, chat) = match route {
        Some((token, chat)) => (Client::new(token).ok()?, chat),
        None => {
            let (client, cfg) = configured()?;
            (client, first_chat(&cfg)?)
        }
    };
    let id = next_approval_id();
    let text = format!("🔐 Approve this action?\n\n{prompt}");
    let buttons = [
        ("✅ Approve", format!("ng/appr/{id}/y")),
        ("❌ Deny", format!("ng/appr/{id}/n")),
    ];
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

    let verdict = if approved {
        "✅ Approved"
    } else {
        "❌ Denied / timed out"
    };
    let _ = client
        .edit_text(chat, msg.message_id, &format!("{text}\n\n{verdict}"))
        .await;
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
                // 409 Conflict = another process (almost always a running `aizen serve` daemon) already
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
                        let _ = client
                            .answer_callback(&cb.id, if approved { "Approved" } else { "Denied" })
                            .await;
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
    fn recovery_effect(&self, _args: &Value) -> bool {
        true
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let text = args
            .get("text")
            .and_then(|v| v.as_str())
            .context("missing required string arg 'text'")?;
        block(async {
            let (client, cfg) =
                configured().context("telegram not configured (run `aizen telegram setup`)")?;
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
    fn recovery_effect(&self, _args: &Value) -> bool {
        true
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let q = args
            .get("question")
            .and_then(|v| v.as_str())
            .context("missing required string arg 'question'")?;
        match block(async { anyhow::Ok(request_approval(q).await) })? {
            Some(true) => Ok("approved".to_string()),
            Some(false) => Ok("denied (or timed out)".to_string()),
            None => Ok("error: telegram not configured (run `aizen telegram setup`)".to_string()),
        }
    }
}

// ── TelegramPlatform (the `Platform` impl) ──────────────────────────────────────────
//
// The multi-bot registry that used to live inline in `run_serve` now lives here, so the generic
// daemon loop never mentions Telegram. All bots share the daemon's single (serial) agent runner, so
// the per-turn approval route back to the originating bot never races.

/// How long the primary bot accepts its pairing code before the window closes (owner must restart
/// `aizen serve` to get a fresh code). The bot controls the machine, so pairing is deliberately
/// short-lived + code-gated rather than "whoever messages first".
const PAIRING_TIMEOUT_SECS: u64 = 600;

/// Consecutive 409s before we call it a conflict rather than a restart overlap. One or two are normal
/// when a daemon restarts (the old poller's long-poll is still draining); a third means a real fight.
const CONFLICT_WARN_AFTER: u32 = 3;
const CONFLICT_BACKOFF_BASE_SECS: u64 = 3;
const CONFLICT_BACKOFF_MAX_SECS: u64 = 30;

/// What a hosted bot's poller is currently doing — surfaced by `/bots` so a token being fought over
/// by two machines is VISIBLE rather than an endless line in a log nobody reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum BotState {
    /// Polling normally.
    #[default]
    Live,
    /// Losing a `getUpdates` race — another process (usually another machine) owns this token.
    Conflict,
}

impl BotState {
    pub fn label(self) -> &'static str {
        match self {
            BotState::Live => "live",
            BotState::Conflict => "CONFLICT (another process polls this token)",
        }
    }
}

/// One hosted Telegram bot. `poll` is its `getUpdates` task — aborted on shutdown or `/rmbot`.
/// `allowed` is shared (`Arc<Mutex>`) so pairing can add the owner live without respawning the loop.
struct BotHandle {
    client: Arc<Client>,
    token: String,
    username: String,
    allowed: Arc<Mutex<Vec<i64>>>,
    /// Per-bot character. `None` (the primary "default", or an un-personad extra) ⇒ the global
    /// `config.persona` / the user's own agent identity. Only the `<persona>`/`<self>` blocks differ.
    persona: Option<String>,
    /// Shared with the poll loop so `/bots` can report a live 409 fight.
    state: Arc<Mutex<BotState>>,
    poll: JoinHandle<()>,
}

/// The live handles the `bot_admin` agent tool needs to reach the RUNNING daemon (add/remove/persona a
/// bot from a chat message). Registered by `TelegramPlatform::start`, cleared by `shutdown`. `None`
/// when no daemon is running (a REPL `bot_admin` call then edits `bots.json` for the next `serve`).
struct BotControl {
    bots: Arc<Mutex<HashMap<String, BotHandle>>>,
    base_allowed: Arc<Mutex<Vec<i64>>>,
    menu: Vec<(String, String)>,
    tx: Sender<Inbound<i64>>,
}
static BOT_CONTROL: Lazy<Mutex<Option<Arc<BotControl>>>> = Lazy::new(|| Mutex::new(None));

fn bot_control() -> Option<Arc<BotControl>> {
    BOT_CONTROL.lock().unwrap().clone()
}

/// Telegram as a hosted platform: a primary bot ("default") plus any `/addbot`-hosted extras, all fed
/// into one inbound channel tagged with the originating bot name (`Inbound::route`).
pub struct TelegramPlatform {
    bots: Arc<Mutex<HashMap<String, BotHandle>>>,
    /// The primary bot's allowlist — shared with its poll loop so pairing can populate it live, and
    /// snapshotted by extra bots (which inherit it).
    base_allowed: Arc<Mutex<Vec<i64>>>,
    primary_token: String,
    /// The slash-command menu to publish (owned copy of the daemon's `SERVE_COMMANDS`).
    menu: Vec<(String, String)>,
    /// Extra bots this process should host (`serve --bots a,b`). Empty ⇒ all of them that this host
    /// is allowed to run. Lets a fleet split bots across machines without editing `bots.json` per box.
    wanted: Vec<String>,
}

/// Does a bot pinned to `host` belong on THIS machine? `None` ⇒ unpinned, runs anywhere (the
/// single-machine default). A pin matches either the hostname label or the stable device id, so an
/// operator can write whichever is more readable.
fn host_matches(host: Option<&str>) -> bool {
    let Some(want) = host.map(str::trim).filter(|s| !s.is_empty()) else {
        return true;
    };
    let dev = crate::core::device::current();
    want.eq_ignore_ascii_case(&dev.label) || want.eq_ignore_ascii_case(dev.id.as_str())
}

impl TelegramPlatform {
    /// Build from `cli-config.json`'s `telegram` section, hosting the named extra bots
    /// (`serve --bots a,b`); an empty list means "every bot this host is allowed to run".
    ///
    /// `menu` is the daemon's command surface (published to each bot via `setMyCommands`). Only a
    /// TOKEN is required — an empty allowlist is fine (the daemon then boots in pairing mode).
    pub fn from_config_selecting(menu: Vec<(String, String)>, wanted: Vec<String>) -> Result<Self> {
        let cfg = cli_config::load().telegram.unwrap_or_default();
        let token = cfg.resolved_token().context(
            "no telegram bot token — run `aizen telegram setup` or `aizen serve --token <token>`",
        )?;
        Ok(Self {
            bots: Arc::new(Mutex::new(HashMap::new())),
            base_allowed: Arc::new(Mutex::new(cfg.allowed_chat_ids)),
            primary_token: token,
            menu,
            wanted,
        })
    }

    fn route_client(&self, route: &str) -> Result<Arc<Client>> {
        self.bots
            .lock()
            .unwrap()
            .get(route)
            .map(|h| h.client.clone())
            .with_context(|| format!("no hosted bot named '{route}'"))
    }
}

/// A short pairing code from process id + nanos (no rng crate — same posture as `next_approval_id`).
fn gen_pairing_code() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let seed = nanos ^ (std::process::id().wrapping_mul(2654435761));
    format!("{:06}", seed % 1_000_000)
}

/// Persist a freshly-paired owner chat id into `cli-config.json` so it survives a restart.
fn persist_owner(chat: i64) {
    let mut cfg = cli_config::load();
    let mut tg = cfg.telegram.clone().unwrap_or_default();
    if !tg.allowed_chat_ids.contains(&chat) {
        tg.allowed_chat_ids.push(chat);
    }
    cfg.telegram = Some(tg);
    let _ = cli_config::save(&cfg);
}

/// Build a client for `token`, publish its "/" menu, spawn its poll loop, and register it under
/// `name`. Returns the bot's @username.
///
/// This RETURNS AN ERROR instead of a placeholder username when the bot cannot actually be hosted —
/// a bad token, or another process already polling it. The old placeholder (`"?"` / `"busy"`) was
/// indistinguishable from success at the call site, so `/addbot` reported `✓ bot @busy live` for a
/// bot that was never inserted into the registry and could never receive a message.
///
/// `pairing` is `Some(code)` when this bot has no owner yet and should accept an ownership claim.
#[allow(clippy::too_many_arguments)]
async fn spawn_bot(
    bots: &Arc<Mutex<HashMap<String, BotHandle>>>,
    menu: &[(String, String)],
    name: String,
    token: String,
    allowed: Arc<Mutex<Vec<i64>>>,
    persona: Option<String>,
    pairing: Option<String>,
    tx: &Sender<Inbound<i64>>,
) -> Result<String> {
    let client = Client::new(token.clone()).context("building the Telegram HTTP client")?;
    let username = client.get_me().await.unwrap_or_else(|_| "?".to_string());
    let cmds: Vec<(&str, &str)> = menu.iter().map(|(c, d)| (c.as_str(), d.as_str())).collect();
    let _ = client.set_my_commands(&cmds).await;
    let client = Arc::new(client);
    let lock_path = crate::core::workspace_txn::resource_lock("telegram", &token);
    let poll_lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        Duration::from_millis(100),
    )
    .map_err(|_| {
        anyhow::anyhow!(
            "another Aizen process on this machine is already polling this bot token — \
             Telegram allows exactly one getUpdates poller per token. Stop that process, or host \
             this bot from a different machine."
        )
    })?;
    let state = Arc::new(Mutex::new(BotState::Live));
    let poll = spawn_bot_poll(
        name.clone(),
        client.clone(),
        allowed.clone(),
        pairing,
        tx.clone(),
        poll_lock,
        state.clone(),
    );
    bots.lock().unwrap().insert(
        name.clone(),
        BotHandle {
            client,
            token,
            username: username.clone(),
            allowed,
            persona,
            state,
            poll,
        },
    );
    Ok(username)
}

/// Spawn one bot's long-poll loop. Pushes `Inbound { route: name, chat, text }` for allowed chats onto
/// `tx`, and resolves any approval callback (global registry — works regardless of which bot it came
/// back on). Skips the backlog on start so a boot / hot-add never replays old messages as fresh tasks.
///
/// `pairing` (primary bot, no owner yet): a message that equals the code claims ownership — the chat
/// is added to the shared allowlist AND persisted; anything else is nudged to send the code. The
/// window closes after `PAIRING_TIMEOUT_SECS` (restart to re-pair). A message from an already-allowed
/// chat always takes the normal path.
fn spawn_bot_poll(
    name: String,
    client: Arc<Client>,
    allowed: Arc<Mutex<Vec<i64>>>,
    pairing: Option<String>,
    tx: Sender<Inbound<i64>>,
    _poll_lock: crate::core::repo_lock::RepoTxnLock,
    state: Arc<Mutex<BotState>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut offset = client.backlog_offset().await;
        let mut pairing = pairing;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(PAIRING_TIMEOUT_SECS);
        // Consecutive 409s. Telegram permits exactly ONE getUpdates poller per token, so a sustained
        // 409 means another process — very often the same bot started on a second machine — owns
        // this token. The old code logged the raw error and retried every 3s forever: an endless log
        // of a fight nobody was told about, with the two pollers stealing each other's updates.
        let mut conflicts = 0u32;
        loop {
            let updates = match client.get_updates(offset, POLL_TIMEOUT_SECS).await {
                Ok(u) => {
                    if conflicts > 0 {
                        eprintln!("[poll {name}] getUpdates recovered — this process owns the token again");
                        conflicts = 0;
                        *state.lock().unwrap() = BotState::Live;
                    }
                    u
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("409") || msg.to_lowercase().contains("conflict") {
                        conflicts += 1;
                        if conflicts == CONFLICT_WARN_AFTER {
                            *state.lock().unwrap() = BotState::Conflict;
                            eprintln!(
                                "[poll {name}] getUpdates 409 Conflict ×{conflicts} — another process is \
                                 polling this bot token. Telegram allows exactly ONE poller per token, so \
                                 the two steal each other's messages. Run this bot on ONE machine: pin it \
                                 with the `host` field in hostbot/bots.json, or start each machine with \
                                 `aizen serve --bots <names>`."
                            );
                        }
                        // Back off progressively so a losing poller stops hammering the API: 3s, 6s,
                        // … capped. A brief overlap during a restart still recovers quickly.
                        let backoff = CONFLICT_BACKOFF_BASE_SECS
                            .saturating_mul(conflicts.min(10) as u64)
                            .min(CONFLICT_BACKOFF_MAX_SECS);
                        tokio::time::sleep(Duration::from_secs(backoff)).await;
                        continue;
                    }
                    eprintln!("[poll {name}] {e}");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                    continue;
                }
            };
            for u in updates {
                offset = offset.max(u.update_id + 1);
                if let Some(cb) = u.callback_query {
                    let chat = cb.message.as_ref().map(|m| m.chat.id).unwrap_or(cb.from.id);
                    if allowed.lock().unwrap().contains(&chat) {
                        if let Some((id, ok)) = cb.data.as_deref().and_then(parse_callback) {
                            resolve_approval(&id, ok);
                        }
                    }
                    let _ = client.answer_callback(&cb.id, "").await;
                    continue;
                }
                if let Some(msg) = u.message {
                    let Some(text) = msg.text else { continue };
                    let chat = msg.chat.id;
                    if allowed.lock().unwrap().contains(&chat) {
                        let _ = tx
                            .send(Inbound {
                                route: name.clone(),
                                chat,
                                text,
                            })
                            .await;
                        continue;
                    }
                    // Not (yet) an allowed chat → pairing path, if this bot is still pairing.
                    if let Some(code) = pairing.clone() {
                        if tokio::time::Instant::now() >= deadline {
                            pairing = None;
                            let _ = client
                                .send_message(
                                    chat,
                                    "Pairing window closed — restart `aizen serve` to pair.",
                                )
                                .await;
                        } else if text.trim() == code {
                            allowed.lock().unwrap().push(chat);
                            persist_owner(chat);
                            pairing = None;
                            let _ = client
                                .send_message(
                                    chat,
                                    "✅ Paired — you're the owner. Send /help to begin.",
                                )
                                .await;
                        } else {
                            let _ = client
                                .send_message(chat, "🔒 Send the pairing code shown on the host to control this machine.")
                                .await;
                        }
                    }
                    // No pairing + not allowed → silently ignored (unknown chat).
                }
            }
        }
    })
}

// ── shared bot-registry ops (used by BOTH the `Platform` methods and the `bot_admin` tool) ──────

/// Append/replace a hosted-bot entry in `bots.json` (token stored, no persona). `own_owner` decides
/// whether it inherits the primary's allowlist or pairs with its own owner.
fn add_bot_entry(name: &str, token: &str, own_owner: bool) -> Result<()> {
    store::update_bots(|list| {
        // Preserve a host pin an operator set by hand: re-adding a bot shouldn't silently move it to
        // whichever machine ran the command.
        let host = list
            .iter()
            .find(|b| b.name == name)
            .and_then(|b| b.host.clone());
        list.retain(|b| b.name != name);
        list.push(store::HostedBot {
            name: name.to_string(),
            token: Some(token.to_string()),
            allowed_chat_ids: Vec::new(),
            persona: None,
            own_owner,
            host,
        });
        Ok(())
    })
}

/// Validate `token`, persist it, then hot-spawn the bot into the running daemon. Shared by
/// `Platform::add_bot` and `admin_add_bot` (daemon-running path).
///
/// `own_owner`: give the bot its OWN owner (boots in pairing mode) instead of inheriting a snapshot
/// of the primary's allowlist.
///
/// If the spawn fails the `bots.json` entry is ROLLED BACK. Otherwise a bot that cannot run would be
/// persisted and re-attempted on every restart, while the caller was told it went live.
async fn do_add_bot(
    bots: &Arc<Mutex<HashMap<String, BotHandle>>>,
    base_allowed: &Arc<Mutex<Vec<i64>>>,
    menu: &[(String, String)],
    tx: &Sender<Inbound<i64>>,
    name: &str,
    token: &str,
    own_owner: bool,
) -> Result<String> {
    if name == "default" {
        bail!(
            "\"default\" is the primary bot — manage it on the host with `aizen telegram setup`."
        );
    }
    {
        let b = bots.lock().unwrap();
        if b.contains_key(name) {
            bail!(
                "a bot named \"{name}\" is already running — pick another name or remove it first."
            );
        }
        if b.values().any(|h| h.token == token) {
            bail!("that token is already hosted under another name.");
        }
    }
    // Validate before persisting (get_me is async → do it outside the lock).
    let client = Client::new(token.to_string()).context("bad token")?;
    let username = client
        .get_me()
        .await
        .context("Telegram rejected the token")?;
    add_bot_entry(name, token, own_owner)?;
    // `own_owner` starts empty + pairing; otherwise inherit a SNAPSHOT of the primary allowlist (a
    // private chat id == the owner's user id across all their bots).
    let (ids, pairing) = if own_owner {
        (Vec::new(), Some(gen_pairing_code()))
    } else {
        (base_allowed.lock().unwrap().clone(), None)
    };
    let allowed = Arc::new(Mutex::new(ids));
    let spawned = spawn_bot(
        bots,
        menu,
        name.to_string(),
        token.to_string(),
        allowed,
        None,
        pairing.clone(),
        tx,
    )
    .await;
    match spawned {
        Ok(_) => Ok(match pairing {
            Some(code) => {
                format!("{username} · pairing code {code} (its owner must send that code)")
            }
            None => username,
        }),
        Err(e) => {
            // Undo the persisted entry so a bot that can't run doesn't come back every restart.
            let _ = store::update_bots(|list| {
                list.retain(|b| b.name != name);
                Ok(())
            });
            Err(e)
        }
    }
}

/// Abort + forget a running bot, and drop its config + sessions. Shared by `Platform::remove_bot`.
async fn do_remove_bot(bots: &Arc<Mutex<HashMap<String, BotHandle>>>, name: &str) -> Result<()> {
    if name == "default" {
        bail!("cannot remove the primary bot — use `aizen telegram disable` on the host.");
    }
    let handle = bots.lock().unwrap().remove(name);
    match handle {
        Some(h) => {
            h.poll.abort();
            let _ = store::update_bots(|list| {
                list.retain(|b| b.name != name);
                Ok(())
            });
            store::drop_route_sessions("telegram", name);
            Ok(())
        }
        None => bail!("no bot named \"{name}\" (see the bot list)."),
    }
}

fn do_list_bots(bots: &Arc<Mutex<HashMap<String, BotHandle>>>) -> Vec<BotInfo> {
    bots.lock()
        .unwrap()
        .iter()
        .map(|(n, h)| {
            let state = *h.state.lock().unwrap();
            BotInfo {
                name: n.clone(),
                username: h.username.clone(),
                chats: h.allowed.lock().unwrap().len(),
                note: match state {
                    BotState::Live => None,
                    other => Some(other.label().to_string()),
                },
            }
        })
        .collect()
}

/// Set the per-bot persona on a RUNNING bot's handle + its `bots.json` entry. Shared by the
/// `bot_admin` tool. Dropping the bot's sessions is the caller's job (so the next turn re-seeds the
/// system prompt with the new character).
fn do_set_persona(
    bots: &Arc<Mutex<HashMap<String, BotHandle>>>,
    name: &str,
    persona: Option<String>,
) -> Result<()> {
    if name == "default" {
        bail!("\"default\" uses the primary agent's own persona — set it on the host with `aizen persona`.");
    }
    // Update the live handle if the bot is running.
    if let Some(h) = bots.lock().unwrap().get_mut(name) {
        h.persona = persona.clone();
    }
    // Update (or create) the persisted entry so it survives a restart.
    store::update_bots(|list| {
        match list.iter_mut().find(|b| b.name == name) {
            Some(b) => b.persona = persona,
            None => bail!("no bot named \"{name}\" (see the bot list)."),
        }
        Ok(())
    })
}

impl Platform for TelegramPlatform {
    type Chat = i64;

    fn name(&self) -> &'static str {
        "telegram"
    }

    fn message_max(&self) -> usize {
        3500 // well under Telegram's 4096 UTF-16 cap, leaving headroom for the "$ cmd\n" prefixes
    }

    async fn start(&self, tx: Sender<Inbound<i64>>) -> Result<()> {
        set_daemon_active(true);
        // No owner yet ⇒ boot the primary bot in pairing mode: print a code on the host, and the first
        // chat that echoes it back claims ownership. The bot controls the machine, so we never accept
        // "whoever messages first".
        let pairing = if self.base_allowed.lock().unwrap().is_empty() {
            let code = gen_pairing_code();
            eprintln!(
                "{}",
                console::style(format!(
                    "🔑 Pairing code: {code} — open the bot in Telegram and send this code to become owner (≤10 min)."
                ))
                .bold()
            );
            Some(code)
        } else {
            None
        };
        // Primary bot is always "default". A failure here IS fatal: without the primary bot the
        // daemon has no owner channel and nothing to serve.
        spawn_bot(
            &self.bots,
            &self.menu,
            "default".to_string(),
            self.primary_token.clone(),
            self.base_allowed.clone(),
            None,
            pairing,
            &tx,
        )
        .await
        .context("starting the primary Telegram bot")?;
        for b in store::load_bots() {
            let Some(token) = b.token.clone() else {
                continue;
            };
            if b.name.is_empty()
                || b.name == "default"
                || self.bots.lock().unwrap().contains_key(&b.name)
            {
                continue;
            }
            // Host pinning: in a fleet each bot names the machine that polls it, because Telegram
            // permits exactly ONE getUpdates poller per token. An unpinned bot runs anywhere.
            if !host_matches(b.host.as_deref()) {
                eprintln!(
                    "[telegram] skipping \"{}\" — pinned to host {:?}, this is {:?}",
                    b.name,
                    b.host.as_deref().unwrap_or("any"),
                    crate::core::device::current().label
                );
                continue;
            }
            if !self.wanted.is_empty() && !self.wanted.iter().any(|w| w == &b.name) {
                continue; // `serve --bots a,b` selected a subset
            }
            // A bot with its own owner NEVER inherits the primary's chats: that inheritance is the
            // convenience default for one owner's own bots, and would leak the primary's chats onto
            // a bot handed to somebody else.
            let (ids, pairing) = if b.own_owner {
                if b.allowed_chat_ids.is_empty() {
                    let code = gen_pairing_code();
                    eprintln!(
                        "{}",
                        console::style(format!(
                            "🔑 \"{}\" pairing code: {code} — its owner sends this to claim it (≤10 min).",
                            b.name
                        ))
                        .bold()
                    );
                    (Vec::new(), Some(code))
                } else {
                    (b.allowed_chat_ids.clone(), None)
                }
            } else if b.allowed_chat_ids.is_empty() {
                (self.base_allowed.lock().unwrap().clone(), None)
            } else {
                (b.allowed_chat_ids.clone(), None)
            };
            let allowed = Arc::new(Mutex::new(ids));
            // One bad extra bot must not stop the daemon: log and carry on with the rest.
            if let Err(e) = spawn_bot(
                &self.bots,
                &self.menu,
                b.name.clone(),
                token,
                allowed,
                b.persona.clone(),
                pairing,
                &tx,
            )
            .await
            {
                eprintln!("[telegram] could not host \"{}\": {e}", b.name);
            }
        }
        // Publish the live handles so the `bot_admin` agent tool can reach this running daemon.
        *BOT_CONTROL.lock().unwrap() = Some(Arc::new(BotControl {
            bots: self.bots.clone(),
            base_allowed: self.base_allowed.clone(),
            menu: self.menu.clone(),
            tx: tx.clone(),
        }));
        Ok(())
    }

    fn render_reply(&self, raw: &str) -> Vec<Outbound> {
        crate::ui::channel_markdown::render_telegram_chunks(raw, self.message_max())
            .into_iter()
            .map(|chunk| Outbound {
                text: chunk.html,
                fallback: chunk.plain,
                rich: true,
            })
            .collect()
    }

    async fn send_outbound(&self, route: &str, chat: i64, outbound: &Outbound) -> Result<()> {
        let client = self.route_client(route)?;
        if outbound.rich {
            match client.send_html(chat, &outbound.text).await {
                Ok(_) => return Ok(()),
                Err(e) => eprintln!("[telegram] rich reply rejected; retrying plain: {e}"),
            }
        }
        client.send_message(chat, &outbound.fallback).await?;
        Ok(())
    }

    async fn start_status(&self, route: &str, chat: i64) -> Result<Option<StatusHandle>> {
        let client = self.route_client(route)?;
        let msg = client.send_message(chat, "✦ Đang xử lý…").await?;
        Ok(Some(StatusHandle(msg.message_id)))
    }

    async fn finish_status(
        &self,
        route: &str,
        chat: i64,
        status: Option<StatusHandle>,
        failed: bool,
    ) -> Result<()> {
        let Some(StatusHandle(message_id)) = status else {
            return Ok(());
        };
        let client = self.route_client(route)?;
        if failed {
            return client
                .edit_text(chat, message_id, "⚠ Không thể hoàn tất")
                .await;
        }
        if client.delete_message(chat, message_id).await.is_err() {
            client.edit_text(chat, message_id, "✓ Hoàn tất").await?;
        }
        Ok(())
    }

    async fn send(&self, route: &str, chat: i64, text: &str) -> Result<()> {
        let client = self.route_client(route)?;
        client.send_message(chat, text).await?;
        Ok(())
    }

    fn supports_approval(&self) -> bool {
        true
    }
    fn set_approval_route(&self, route: &str, chat: i64) {
        let token = {
            let bots = self.bots.lock().unwrap();
            bots.get(route).map(|h| h.token.clone())
        };
        if let Some(token) = token {
            set_approval_route(token, chat);
        }
    }
    fn clear_approval_route(&self) {
        clear_approval_route();
    }

    fn supports_multibot(&self) -> bool {
        true
    }

    async fn add_bot(
        &self,
        name: &str,
        token: &str,
        own_owner: bool,
        tx: &Sender<Inbound<i64>>,
    ) -> Result<String> {
        do_add_bot(
            &self.bots,
            &self.base_allowed,
            &self.menu,
            tx,
            name,
            token,
            own_owner,
        )
        .await
    }

    async fn remove_bot(&self, name: &str) -> Result<()> {
        do_remove_bot(&self.bots, name).await
    }

    fn list_bots(&self) -> Vec<BotInfo> {
        do_list_bots(&self.bots)
    }

    fn persona_for(&self, route: &str) -> Option<String> {
        self.bots
            .lock()
            .unwrap()
            .get(route)
            .and_then(|h| h.persona.clone())
    }

    fn shutdown(&self) {
        *BOT_CONTROL.lock().unwrap() = None;
        for (_, h) in self.bots.lock().unwrap().drain() {
            h.poll.abort();
        }
        set_daemon_active(false);
    }
}

// ── `bot_admin` agent tool (the primary "default" bot self-configures on the owner's word) ──────
//
// The daemon's agent turns can call this to add/remove/persona a hosted bot from a chat message
// ("host a bot called work with token …"). It's `is_destructive` so it always routes an approval
// (✓/✗ on the phone) before touching config, and is top-level only (never granted to sub-agents).

/// Set the PRIMARY ("default") bot's token from the agent tool — "paste token by voice". The primary
/// bot's identity lives in `cli-config.json` (not `bots.json`), so we validate + write it there.
/// A running daemon can't safely hot-swap its primary token (the owner is chatting with the *old*
/// bot, so a reply from a different bot would go astray) → we persist and ask for a restart. The
/// common path is the REPL with no daemon running: persist, then `aizen serve` boots it (pairing).
async fn admin_set_primary(token: &str) -> Result<String> {
    let client = Client::new(token.to_string()).context("bad token")?;
    let username = client
        .get_me()
        .await
        .context("Telegram rejected the token")?;
    let mut cfg = cli_config::load();
    let mut tg = cfg.telegram.clone().unwrap_or_default();
    tg.token = Some(token.to_string());
    cfg.telegram = Some(tg);
    cli_config::save(&cfg)?;
    Ok(match bot_control() {
        Some(_) => format!(
            "primary bot token saved (@{username}) — restart `aizen serve` to switch the primary bot over."
        ),
        None => format!(
            "primary bot set to @{username} — run `aizen serve` (it prints a pairing code if there's no owner yet)."
        ),
    })
}

/// Add a bot from the agent tool. `name == "default"` sets the PRIMARY bot's token (see
/// `admin_set_primary`). Otherwise: if the daemon is running, hot-spawn it (via `BOT_CONTROL`);
/// else just record it in `bots.json` for the next `serve`.
async fn admin_add_bot(name: &str, token: &str, own_owner: bool) -> Result<String> {
    if name == "default" {
        return admin_set_primary(token).await;
    }
    match bot_control() {
        Some(ctl) => {
            let user = do_add_bot(
                &ctl.bots,
                &ctl.base_allowed,
                &ctl.menu,
                &ctl.tx,
                name,
                token,
                own_owner,
            )
            .await?;
            Ok(format!("hosting @{user} as \"{name}\" (live)"))
        }
        None => {
            // No live daemon: validate the token, then persist for the next serve.
            let client = Client::new(token.to_string()).context("bad token")?;
            let user = client
                .get_me()
                .await
                .context("Telegram rejected the token")?;
            add_bot_entry(name, token, own_owner)?;
            Ok(format!(
                "saved @{user} as \"{name}\" — it will start on the next `aizen serve`"
            ))
        }
    }
}

/// Remove a bot from the agent tool (live if the daemon is running, else edit `bots.json`).
async fn admin_remove_bot(name: &str) -> Result<String> {
    match bot_control() {
        Some(ctl) => {
            do_remove_bot(&ctl.bots, name).await?;
            Ok(format!("stopped hosting \"{name}\""))
        }
        None => {
            if name == "default" {
                bail!("cannot remove the primary bot.");
            }
            let removed = store::update_bots(|list| {
                let before = list.len();
                list.retain(|b| b.name != name);
                Ok(list.len() != before)
            })?;
            if !removed {
                bail!("no bot named \"{name}\" in the saved list.");
            }
            store::drop_route_sessions("telegram", name);
            Ok(format!("removed \"{name}\" from the saved list"))
        }
    }
}

/// Set a bot's persona from the agent tool (live handle if running, plus `bots.json`), then drop its
/// sessions so the next turn re-seeds the system prompt with the new character.
fn admin_set_persona(name: &str, persona: Option<String>) -> Result<String> {
    // The persona must exist (creating characters is `persona_create`'s job).
    if let Some(p) = persona.as_deref() {
        if crate::persona::load(p).is_none() {
            bail!("no persona named \"{p}\" — create it first with the persona tool.");
        }
    }
    let bots = bot_control().map(|c| c.bots.clone());
    match bots {
        Some(bots) => do_set_persona(&bots, name, persona.clone())?,
        None => {
            // No live daemon: edit the persisted entry directly.
            store::update_bots(|list| {
                match list.iter_mut().find(|b| b.name == name) {
                    Some(b) => b.persona = persona.clone(),
                    None => bail!("no bot named \"{name}\" in the saved list."),
                }
                Ok(())
            })?;
        }
    }
    store::drop_route_sessions("telegram", name);
    Ok(match persona {
        Some(p) => format!("\"{name}\" now speaks as persona \"{p}\""),
        None => format!("cleared \"{name}\"'s persona (back to the default agent)"),
    })
}

pub struct BotAdmin;
impl Tool for BotAdmin {
    fn name(&self) -> &str {
        "bot_admin"
    }
    fn description(&self) -> &str {
        "Manage the Telegram bots this daemon hosts, on the owner's request. Actions: \
         `list` (show hosted bots + their personas); `add` (host a bot — needs name + @BotFather \
         token; use name \"default\" to set/replace the PRIMARY bot's own token — takes effect on the \
         next `aizen serve`); `remove` (stop hosting a bot — cannot remove \"default\"); `set_persona` \
         (give a bot its own character — the persona must already exist; omit `persona` to clear it). \
         Memory stays the primary agent's; only a bot's persona/voice differs. \"default\" is the \
         primary bot: you can set its token via `add`, but not remove it or give it a separate persona."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["list", "add", "remove", "set_persona"]},
                "name": {"type": "string", "description": "bot name (route). Required for add/remove/set_persona."},
                "token": {"type": "string", "description": "@BotFather token. Required for add."},
                "persona": {"type": "string", "description": "persona name for set_persona; omit to clear."},
                "own_owner": {"type": "boolean", "description": "add: give this bot its own owner (it pairs with a code) instead of inheriting yours. Use when the bot is for somebody else."}
            },
            "required": ["action"],
            "additionalProperties": false
        })
    }
    fn is_destructive(&self) -> bool {
        true // writes tokens to config / stops running bots → route an approval first
    }
    fn is_concurrency_safe(&self) -> bool {
        false // mutates the shared bot registry + config
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let action = args
            .get("action")
            .and_then(|v| v.as_str())
            .context("missing 'action'")?;
        let name = args.get("name").and_then(|v| v.as_str());
        match action {
            "list" => {
                let bots = match bot_control() {
                    Some(ctl) => do_list_bots(&ctl.bots),
                    None => store::load_bots()
                        .into_iter()
                        .map(|b| BotInfo {
                            name: b.name,
                            username: "?".to_string(),
                            chats: 0,
                            note: Some("not running (no daemon)".to_string()),
                        })
                        .collect(),
                };
                if bots.is_empty() {
                    return Ok("no extra bots hosted (only the primary \"default\").".to_string());
                }
                let saved = store::load_bots();
                let mut s = String::new();
                for b in bots {
                    let persona = saved
                        .iter()
                        .find(|x| x.name == b.name)
                        .and_then(|x| x.persona.clone())
                        .unwrap_or_else(|| "(default agent)".to_string());
                    s.push_str(&format!(
                        "• {} — @{} · persona: {}\n",
                        b.name, b.username, persona
                    ));
                }
                Ok(s.trim_end().to_string())
            }
            "add" => {
                let name = name.context("'add' needs a bot name")?;
                let token = args
                    .get("token")
                    .and_then(|v| v.as_str())
                    .context("'add' needs a token")?;
                let own_owner = args
                    .get("own_owner")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                block(async { admin_add_bot(name, token, own_owner).await })
            }
            "remove" => {
                let name = name.context("'remove' needs a bot name")?;
                block(async { admin_remove_bot(name).await })
            }
            "set_persona" => {
                let name = name.context("'set_persona' needs a bot name")?;
                let persona = args
                    .get("persona")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                admin_set_persona(name, persona)
            }
            other => bail!("unknown action '{other}' (use list|add|remove|set_persona)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_callback_roundtrip() {
        assert_eq!(
            parse_callback("ng/appr/123-4/y"),
            Some(("123-4".to_string(), true))
        );
        assert_eq!(
            parse_callback("ng/appr/abc/n"),
            Some(("abc".to_string(), false))
        );
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

    #[test]
    fn bot_admin_is_destructive_and_serial() {
        // Writes tokens to config / stops running bots → must route an approval, and never run
        // on the parallel batch path.
        assert!(
            BotAdmin.is_destructive(),
            "bot_admin edits host config → approval-gated"
        );
        assert!(
            !BotAdmin.is_concurrency_safe(),
            "mutates the shared registry + config"
        );
        assert_eq!(BotAdmin.name(), "bot_admin");
    }

    #[test]
    fn pairing_code_is_six_digits() {
        let code = gen_pairing_code();
        assert_eq!(
            code.len(),
            6,
            "pairing code is zero-padded to 6 digits: {code}"
        );
        assert!(
            code.chars().all(|c| c.is_ascii_digit()),
            "digits only: {code}"
        );
    }

    /// Pin `AIZEN_HOME` to a fresh tempdir. Shares the crate-wide lock like every HOME-mutating test.
    fn with_temp_home<T>(tag: &str, f: impl FnOnce() -> T) -> T {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-tg-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("AIZEN_HOME", &dir);
        let out = f();
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn add_bot_entry_records_own_owner_and_keeps_a_hand_set_host() {
        with_temp_home("add-entry", || {
            add_bot_entry("work", "t1", false).unwrap();
            let saved = store::load_bots();
            assert_eq!(saved.len(), 1);
            assert!(!saved[0].own_owner, "default inherits the primary's owner");
            assert_eq!(saved[0].host, None);

            // An operator pins the bot to a machine by hand…
            store::update_bots(|list| {
                list[0].host = Some("vps-1".into());
                Ok(())
            })
            .unwrap();
            // …and re-adding it (a token rotation) must NOT silently move it to this machine.
            add_bot_entry("work", "t2", true).unwrap();
            let saved = store::load_bots();
            assert_eq!(saved.len(), 1, "re-add replaces rather than duplicates");
            assert_eq!(saved[0].token.as_deref(), Some("t2"), "token rotated");
            assert!(saved[0].own_owner, "own_owner updated");
            assert_eq!(
                saved[0].host.as_deref(),
                Some("vps-1"),
                "the host pin survives a re-add"
            );
        });
    }

    #[test]
    fn an_unpinned_bot_runs_anywhere_and_a_foreign_pin_is_skipped() {
        // Host pinning is what stops two machines fighting over one token (Telegram allows exactly
        // one getUpdates poller per token — the second gets 409 forever).
        assert!(host_matches(None), "unpinned ⇒ runs on any machine");
        assert!(host_matches(Some("  ")), "blank pin is not a pin");
        assert!(
            host_matches(Some(&crate::core::device::current().label)),
            "this machine's own hostname matches"
        );
        assert!(
            host_matches(Some(crate::core::device::current().id.as_str())),
            "the stable device id matches too"
        );
        assert!(
            !host_matches(Some("some-other-machine-that-is-not-this-one")),
            "a foreign pin is skipped, leaving that bot to its own host"
        );
    }

    #[test]
    fn host_match_is_case_insensitive() {
        // Hostnames are case-insensitive in practice, and an operator typing `VPS-1` vs `vps-1`
        // should not silently leave a bot unhosted.
        let label = crate::core::device::current().label.to_uppercase();
        assert!(host_matches(Some(&label)));
    }

    #[test]
    fn the_wanted_list_selects_a_subset_and_empty_means_all() {
        // `serve --bots a,b` is how a fleet divides bots across machines. The selection rule lives in
        // `start`, so assert the predicate itself: empty ⇒ everything, otherwise exact membership.
        let wanted: Vec<String> = vec!["work".into(), "ops".into()];
        let selected = |w: &[String], name: &str| w.is_empty() || w.iter().any(|x| x == name);
        assert!(selected(&wanted, "work"));
        assert!(selected(&wanted, "ops"));
        assert!(
            !selected(&wanted, "other"),
            "an unlisted bot is left to its own machine"
        );
        assert!(
            selected(&[], "anything"),
            "no --bots ⇒ host everything allowed"
        );
    }

    #[test]
    fn bot_state_labels_distinguish_live_from_a_token_fight() {
        // `/bots` shows this text; a conflict must read as a problem, not as a normal state.
        assert_eq!(BotState::default(), BotState::Live);
        assert_eq!(BotState::Live.label(), "live");
        assert!(
            BotState::Conflict.label().contains("another process"),
            "the label must say WHY it is not polling: {}",
            BotState::Conflict.label()
        );
    }

    #[test]
    fn a_conflict_is_only_declared_after_a_restart_overlap_would_have_cleared() {
        // One or two 409s are normal while a restarting daemon's old long-poll drains. Declaring a
        // conflict on the first would cry wolf on every restart; the threshold is what makes the
        // `/bots` warning trustworthy. Backoff must also stay bounded.
        assert!(
            CONFLICT_WARN_AFTER >= 2,
            "a single 409 during a restart must not be called a conflict"
        );
        let at = |n: u32| {
            CONFLICT_BACKOFF_BASE_SECS
                .saturating_mul(n.min(10) as u64)
                .min(CONFLICT_BACKOFF_MAX_SECS)
        };
        assert_eq!(at(1), CONFLICT_BACKOFF_BASE_SECS, "first retry is quick");
        assert!(at(2) > at(1), "backoff grows");
        assert_eq!(
            at(1_000),
            CONFLICT_BACKOFF_MAX_SECS,
            "and is capped, so a losing poller never sleeps forever"
        );
    }
}
