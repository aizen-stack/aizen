//! Discord BOT integration — two-way, pure-Rust DIY: REST (reqwest) to send, the gateway WebSocket
//! (tokio-tungstenite over rustls) to receive. NO serenity (keeps the lean static binary), mirrors
//! `telegram.rs`. Powers `aizen discord serve` (via `DiscordPlatform`). The gateway v10 handshake is
//! implemented by hand per the documented protocol: HELLO → heartbeat loop (with **Heartbeat-ACK
//! zombie detection** + handling of server-initiated op 1 heartbeat-request / op 7 reconnect / op 9
//! invalid-session), IDENTIFY (with the privileged MESSAGE_CONTENT intent), MESSAGE_CREATE dispatch,
//! reconnect-with-backoff. Replies go over REST (independent of the gateway), so the heartbeat keeps
//! flowing while the agent works on a separate task. (RESUME-after-drop is the remaining refinement —
//! a reconnect re-IDENTIFYs fresh.)
//!
//! `DiscordPlatform` (bottom of the file) implements the `Platform` contract: one gateway, one channel
//! namespace ("default" route), no inline approval buttons + no multi-bot (both inherit the trait's
//! "unsupported" defaults), so the generic daemon loop stays platform-agnostic.

use anyhow::{bail, Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::{self, Sender};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::core::cli_config::{self, DiscordConfig};
use crate::hostbot::platform::{Inbound, Outbound, Platform};

const API_BASE: &str = "https://discord.com/api/v10";
const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";
/// Discord message hard limit (a longer body → HTTP 400). The runner chunks to just under this.
pub const MESSAGE_MAX: usize = 1900;
/// Gateway intents: GUILDS (1<<0) | GUILD_MESSAGES (1<<9) | DIRECT_MESSAGES (1<<12) |
/// MESSAGE_CONTENT (1<<15). MESSAGE_CONTENT is **privileged** — enable it in the bot's Developer
/// Portal settings, else `content` arrives empty (and IDENTIFY may be rejected with close 4014).
const INTENTS: u64 = (1 << 0) | (1 << 9) | (1 << 12) | (1 << 15);

// Gateway opcodes (https://discord.com/developers/docs/topics/gateway-events#receive-events).
const OP_DISPATCH: u64 = 0;
const OP_HEARTBEAT: u64 = 1; // (recv) Discord asks us to beat NOW · (send) our heartbeat
const OP_IDENTIFY: u64 = 2;
const OP_RECONNECT: u64 = 7; // Discord asks us to reconnect (resume/re-identify)
const OP_INVALID_SESSION: u64 = 9; // our session is invalid → reconnect fresh
const OP_HELLO: u64 = 10; // first frame, carries heartbeat_interval
const OP_HEARTBEAT_ACK: u64 = 11; // ack of our heartbeat — its ABSENCE means a zombied link

// ── REST client ──────────────────────────────────────────────────────────────────

pub struct Client {
    http: reqwest::Client,
    token: String,
}

impl Client {
    pub fn new(token: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .context("building discord HTTP client")?;
        Ok(Self { http, token })
    }

    fn auth(&self) -> String {
        format!("Bot {}", self.token)
    }

    /// POST a plain-text message to a channel.
    pub async fn send_message(&self, channel_id: u64, content: &str) -> Result<()> {
        let url = format!("{API_BASE}/channels/{channel_id}/messages");
        let resp = self
            .http
            .post(&url)
            .header("Authorization", self.auth())
            .json(&json!({"content": content}))
            .send()
            .await
            .context("discord sendMessage")?;
        if !resp.status().is_success() {
            let s = resp.status();
            let b: String = resp.text().await.unwrap_or_default().chars().take(200).collect();
            bail!("discord send failed: HTTP {} {}", s.as_u16(), b.trim());
        }
        Ok(())
    }

    /// `GET /users/@me` → the bot's username (validates the token; used by `aizen discord test`).
    pub async fn get_me(&self) -> Result<String> {
        let url = format!("{API_BASE}/users/@me");
        let resp =
            self.http.get(&url).header("Authorization", self.auth()).send().await.context("discord getMe")?;
        if !resp.status().is_success() {
            bail!("discord rejected the token (HTTP {})", resp.status().as_u16());
        }
        let v: Value = resp.json().await.context("parsing /users/@me")?;
        Ok(v.get("username").and_then(|u| u.as_str()).unwrap_or("?").to_string())
    }
}

// ── config plumbing ────────────────────────────────────────────────────────────

/// Build a client + config if the bot is set up (token + at least one allowed channel).
pub fn configured() -> Option<(Client, DiscordConfig)> {
    let cfg = cli_config::load().discord?;
    let token = cfg.resolved_token()?;
    if cfg.allowed_channel_ids.is_empty() {
        return None;
    }
    let client = Client::new(token).ok()?;
    Some((client, cfg))
}

/// Cheap check (no client build).
pub fn is_configured() -> bool {
    cli_config::load()
        .discord
        .map(|d| d.resolved_token().is_some() && !d.allowed_channel_ids.is_empty())
        .unwrap_or(false)
}

/// Allowlist: channel must be listed AND (no user restriction OR user is listed). Empty channel
/// list denies everyone (secure default).
pub fn is_allowed(cfg: &DiscordConfig, channel: u64, user: u64) -> bool {
    cfg.allowed_channel_ids.contains(&channel)
        && (cfg.allowed_user_ids.is_empty() || cfg.allowed_user_ids.contains(&user))
}

// ── gateway (receive) ────────────────────────────────────────────────────────────

/// One inbound message the gateway hands to the runner loop.
pub struct Incoming {
    pub channel_id: u64,
    pub user_id: u64,
    pub content: String,
}

/// A permanent gateway close (bad token / a required intent not enabled). Reconnecting on these just
/// re-IDENTIFYs every few seconds, which Discord rate-limits and will eventually ban the token for —
/// so `run_gateway` gives up instead of looping. Carried as an `anyhow` cause and downcast below.
#[derive(Debug)]
struct FatalClose(String);
impl std::fmt::Display for FatalClose {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for FatalClose {}

/// Connect + run the gateway receive loop, forwarding allowed messages to `tx`. Reconnects with
/// EXPONENTIAL backoff (1s→60s, reset after a healthy session) on transient drops, but STOPS on a
/// permanent close (bad token / missing intent) so we never IDENTIFY-storm Discord into a ban.
pub async fn run_gateway(token: String, cfg: DiscordConfig, tx: mpsc::Sender<Incoming>) {
    let mut backoff = 1u64;
    loop {
        let start = tokio::time::Instant::now();
        if let Err(e) = gateway_once(&token, &cfg, &tx).await {
            eprintln!("[discord gateway] {e}");
            if e.downcast_ref::<FatalClose>().is_some() {
                eprintln!("[discord gateway] permanent failure — not reconnecting (fix the token / enable the intent, then restart).");
                return;
            }
            // A session that stayed up a while was healthy → reconnect promptly; a fast-flapping link
            // backs off so we don't hammer IDENTIFY.
            if start.elapsed() >= Duration::from_secs(60) {
                backoff = 1;
            }
            tokio::time::sleep(Duration::from_secs(backoff)).await;
            backoff = (backoff * 2).min(60);
        } else {
            backoff = 1;
        }
    }
}

async fn gateway_once(token: &str, cfg: &DiscordConfig, tx: &mpsc::Sender<Incoming>) -> Result<()> {
    let (mut ws, _) =
        tokio_tungstenite::connect_async(GATEWAY_URL).await.context("connecting to discord gateway")?;

    // 1) HELLO → heartbeat_interval (ms). Read frames until we get it — under a hard timeout so a
    // gateway that completes the WS upgrade but never sends HELLO (broken proxy / misbehaving edge)
    // can't stall the daemon forever: on elapse we bail and `run_gateway`'s backoff reconnects.
    let mut interval_ms = 41_250u64;
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let frame = ws.next().await.context("gateway closed before HELLO")?.context("gateway read")?;
            if let WsMessage::Text(t) = frame {
                if let Ok(v) = serde_json::from_str::<Value>(t.as_str()) {
                    if v.get("op").and_then(|o| o.as_u64()) == Some(OP_HELLO) {
                        interval_ms = v
                            .get("d")
                            .and_then(|d| d.get("heartbeat_interval"))
                            .and_then(|v| v.as_u64())
                            .unwrap_or(interval_ms);
                        break;
                    }
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    })
    .await
    .context("timed out waiting for gateway HELLO (20s)")??;

    // 2) IDENTIFY.
    let identify = json!({
        "op": OP_IDENTIFY,
        "d": {
            "token": token,
            "intents": INTENTS,
            "properties": {"os": std::env::consts::OS, "browser": "aizen", "device": "aizen"}
        }
    });
    ws.send(WsMessage::text(identify.to_string())).await.context("sending IDENTIFY")?;

    // 3) heartbeat + dispatch loop. Replies go via REST elsewhere, so this loop only beats + reads.
    let mut hb = tokio::time::interval(Duration::from_millis(interval_ms.max(1000)));
    hb.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut last_seq: Option<u64> = None;
    // Zombie-link guard: each beat sets this true; the matching Heartbeat-ACK (op 11) clears it. If a
    // beat comes due while still awaiting the previous ACK, the connection is dead-but-not-closed —
    // force a reconnect (per the gateway spec) instead of silently going deaf.
    let mut awaiting_ack = false;
    loop {
        tokio::select! {
            _ = hb.tick() => {
                if awaiting_ack {
                    bail!("no Heartbeat ACK since the last beat — zombied gateway link, reconnecting");
                }
                ws.send(WsMessage::text(json!({"op": OP_HEARTBEAT, "d": last_seq}).to_string()))
                    .await.context("sending heartbeat")?;
                awaiting_ack = true;
            }
            frame = ws.next() => {
                let frame = frame.context("gateway closed")?.context("gateway read")?;
                let text = match frame {
                    WsMessage::Text(t) => t.as_str().to_string(),
                    WsMessage::Ping(p) => { ws.send(WsMessage::Pong(p)).await.ok(); continue; }
                    WsMessage::Close(frame) => {
                        let code = frame.as_ref().map(|f| u16::from(f.code));
                        // 4004 auth failed · 4010-4013 bad shard/intents/version · 4014 disallowed
                        // (privileged) intent — all permanent; reconnecting would IDENTIFY-storm.
                        if matches!(code, Some(4004 | 4010 | 4011 | 4012 | 4013 | 4014)) {
                            return Err(anyhow::Error::new(FatalClose(format!(
                                "gateway closed with permanent code {code:?} — bad token or a required intent (e.g. MESSAGE_CONTENT) is not enabled in the Developer Portal"
                            ))));
                        }
                        bail!("gateway sent Close (code {code:?}) — reconnecting");
                    }
                    _ => continue,
                };
                let v: Value = match serde_json::from_str(&text) { Ok(v) => v, Err(_) => continue };
                if let Some(s) = v.get("s").and_then(|s| s.as_u64()) {
                    last_seq = Some(s);
                }
                match v.get("op").and_then(|o| o.as_u64()) {
                    // Discord can demand an immediate heartbeat (out of band) — answer at once.
                    Some(OP_HEARTBEAT) => {
                        ws.send(WsMessage::text(json!({"op": OP_HEARTBEAT, "d": last_seq}).to_string()))
                            .await.context("sending requested heartbeat")?;
                        awaiting_ack = true;
                    }
                    Some(OP_HEARTBEAT_ACK) => awaiting_ack = false,
                    // Server-initiated reconnect / invalidated session → drop and reconnect fresh
                    // (run_gateway loops with a backoff + a new IDENTIFY).
                    Some(OP_RECONNECT) => bail!("gateway requested reconnect (op 7)"),
                    Some(OP_INVALID_SESSION) => bail!("gateway invalidated the session (op 9) — reconnecting"),
                    Some(OP_DISPATCH) if v.get("t").and_then(|t| t.as_str()) == Some("MESSAGE_CREATE") => {
                        if let Some(inc) = parse_message_create(v.get("d")) {
                            if is_allowed(cfg, inc.channel_id, inc.user_id) {
                                let _ = tx.send(inc).await;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

/// Parse a MESSAGE_CREATE payload → `Incoming`. Skips bot authors (incl. our own messages) and
/// empty content. Discord ids are snowflake STRINGS in JSON → parsed to `u64`.
fn parse_message_create(d: Option<&Value>) -> Option<Incoming> {
    let d = d?;
    if d.get("author").and_then(|a| a.get("bot")).and_then(|b| b.as_bool()).unwrap_or(false) {
        return None;
    }
    let channel_id = d.get("channel_id")?.as_str()?.parse().ok()?;
    let user_id = d.get("author")?.get("id")?.as_str()?.parse().ok()?;
    let content = d.get("content")?.as_str()?.trim().to_string();
    if content.is_empty() {
        return None;
    }
    Some(Incoming { channel_id, user_id, content })
}

// ── DiscordPlatform (the `Platform` impl) ──────────────────────────────────────────
//
// One gateway feeding one inbound channel; every message uses the "default" route (Discord has no
// multi-bot hosting). Inline approval buttons aren't implemented, so `supports_approval` stays false
// (its trait default) → the agent's approval gate auto-denies destructive ops (they're skipped),
// exactly as before this refactor.

/// Discord as a hosted platform. `gateway` is the receive task — aborted on shutdown.
pub struct DiscordPlatform {
    client: Arc<Client>,
    cfg: DiscordConfig,
    token: String,
    gateway: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl DiscordPlatform {
    /// Build from `cli-config.json`'s `discord` section. Fails if the bot isn't set up.
    pub fn from_config() -> Result<Self> {
        let (client, cfg) =
            configured().context("Discord bot not configured — run `aizen discord setup`")?;
        let token = cfg.resolved_token().context("no bot token")?;
        Ok(Self {
            client: Arc::new(client),
            cfg,
            token,
            gateway: std::sync::Mutex::new(None),
        })
    }
}

impl Platform for DiscordPlatform {
    type Chat = u64;

    fn name(&self) -> &'static str {
        "discord"
    }

    fn message_max(&self) -> usize {
        MESSAGE_MAX
    }

    async fn start(&self, tx: Sender<Inbound<u64>>) -> Result<()> {
        // Bridge the gateway's `Incoming` onto the generic `Inbound` channel (route is always
        // "default" — Discord has no sub-bots).
        let (gtx, mut grx) = mpsc::channel::<Incoming>(64);
        let token = self.token.clone();
        let cfg = self.cfg.clone();
        let gw = tokio::spawn(async move { run_gateway(token, cfg, gtx).await });
        let bridge = tokio::spawn(async move {
            while let Some(inc) = grx.recv().await {
                let _ = tx
                    .send(Inbound { route: "default".to_string(), chat: inc.channel_id, text: inc.content })
                    .await;
            }
        });
        // Track the gateway task so shutdown can abort it; the bridge ends when the gateway's sender
        // drops, so it needs no separate handle.
        *self.gateway.lock().unwrap() = Some(gw);
        drop(bridge); // detached — self-terminates when `grx` closes
        Ok(())
    }

    fn render_reply(&self, raw: &str) -> Vec<Outbound> {
        let shown = crate::ui::markdown::render_plain_blocks(raw);
        chunk_plain(&shown, MESSAGE_MAX).into_iter().map(Outbound::plain).collect()
    }

    async fn send(&self, _route: &str, chat: u64, text: &str) -> Result<()> {
        self.client.send_message(chat, text).await
    }

    fn shutdown(&self) {
        if let Some(gw) = self.gateway.lock().unwrap().take() {
            gw.abort();
        }
    }
}

fn chunk_plain(s: &str, max: usize) -> Vec<String> {
    if s.encode_utf16().count() <= max { return vec![s.to_string()]; }
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut units = 0usize;
    for ch in s.chars() {
        let u = ch.len_utf16();
        if units + u > max && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            units = 0;
        }
        cur.push(ch);
        units += u;
    }
    if !cur.is_empty() { out.push(cur); }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intents_include_message_content() {
        assert_eq!(INTENTS & (1 << 15), 1 << 15, "MESSAGE_CONTENT must be requested");
        assert_eq!(INTENTS & (1 << 9), 1 << 9, "GUILD_MESSAGES must be requested");
    }

    #[test]
    fn gateway_opcodes_match_discord_v10() {
        // The loop's zombie detection + control-frame handling depend on these exact opcode numbers.
        assert_eq!((OP_DISPATCH, OP_HEARTBEAT, OP_IDENTIFY), (0, 1, 2));
        assert_eq!((OP_RECONNECT, OP_INVALID_SESSION, OP_HELLO, OP_HEARTBEAT_ACK), (7, 9, 10, 11));
    }

    #[test]
    fn allowlist_denies_empty_and_unlisted() {
        let mut cfg = DiscordConfig::default();
        assert!(!is_allowed(&cfg, 100, 7), "empty channel list denies everyone");
        cfg.allowed_channel_ids = vec![100, 200];
        assert!(is_allowed(&cfg, 100, 7), "listed channel, no user restriction → allowed");
        assert!(!is_allowed(&cfg, 300, 7), "unlisted channel → denied");
        cfg.allowed_user_ids = vec![7];
        assert!(is_allowed(&cfg, 100, 7), "listed channel + listed user → allowed");
        assert!(!is_allowed(&cfg, 100, 8), "listed channel but unlisted user → denied");
    }

    #[test]
    fn parse_skips_bots_and_empty_parses_snowflakes() {
        let bot = json!({"channel_id":"1","author":{"id":"2","bot":true},"content":"hi"});
        assert!(parse_message_create(Some(&bot)).is_none(), "bot author skipped");
        let empty = json!({"channel_id":"1","author":{"id":"2"},"content":"   "});
        assert!(parse_message_create(Some(&empty)).is_none(), "empty content skipped");
        let ok = json!({"channel_id":"123","author":{"id":"456"},"content":"hello"});
        let inc = parse_message_create(Some(&ok)).expect("valid message parses");
        assert_eq!((inc.channel_id, inc.user_id, inc.content.as_str()), (123, 456, "hello"));
    }
}
