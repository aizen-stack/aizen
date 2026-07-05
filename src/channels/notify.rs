//! Outbound notification adapters — Discord, Slack, and a generic webhook. Pure-Rust over reqwest,
//! ONE-WAY: the agent reports progress / results / alerts into the owner's channels. Unlike Telegram
//! (full two-way control via long-poll `getUpdates`), these are plain HTTP POST sinks — no daemon,
//! no public URL, no inbound. Powers the `notify` agent tool and the Discord/Slack/Webhook entries
//! in the `/apps` hub.
//!
//! **To add another POST-style channel**: add a `Channel` variant + its arms in the impl below;
//! the `notify` tool, `/apps` hub, and config all pick it up from `Channel::ALL`.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;

use crate::agent::tools::Tool;
use crate::core::cli_config::{self, CliConfig, NotifyConfig};

const SEND_TIMEOUT_SECS: u64 = 15;
/// Discord rejects a message body over 2000 chars (HTTP 400) → truncate with a marker.
const DISCORD_MAX: usize = 2000;

/// An outbound notification channel (a POST-only sink).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Channel {
    Discord,
    Slack,
    Webhook,
}

impl Channel {
    pub const ALL: &'static [Channel] = &[Channel::Discord, Channel::Slack, Channel::Webhook];

    /// Stable lowercase key (summaries + tests).
    pub fn key(&self) -> &'static str {
        match self {
            Channel::Discord => "discord",
            Channel::Slack => "slack",
            Channel::Webhook => "webhook",
        }
    }

    /// Display label.
    pub fn label(&self) -> &'static str {
        match self {
            Channel::Discord => "Discord",
            Channel::Slack => "Slack",
            Channel::Webhook => "Webhook",
        }
    }

    pub fn icon(&self) -> &'static str {
        match self {
            Channel::Discord => "🎮",
            Channel::Slack => "💬",
            Channel::Webhook => "🪝",
        }
    }

    pub fn blurb(&self) -> &'static str {
        match self {
            Channel::Discord => "post agent updates to a Discord channel (webhook URL)",
            Channel::Slack => "post agent updates to a Slack channel (incoming webhook)",
            Channel::Webhook => "POST agent updates as JSON to any URL (custom integrations)",
        }
    }

    /// Env var that overrides the stored URL (one-off / CI).
    pub fn env_var(&self) -> &'static str {
        match self {
            Channel::Discord => "NG_DISCORD_WEBHOOK",
            Channel::Slack => "NG_SLACK_WEBHOOK",
            Channel::Webhook => "NG_WEBHOOK_URL",
        }
    }

    /// Where to obtain the URL (shown during setup).
    pub fn setup_hint(&self) -> &'static str {
        match self {
            Channel::Discord => {
                "Discord → Server Settings → Integrations → Webhooks → New Webhook → Copy URL"
            }
            Channel::Slack => "Slack → api.slack.com/apps → Incoming Webhooks → Add → Copy the URL",
            Channel::Webhook => "Any endpoint that accepts a JSON POST — we send {\"text\": \"…\"}.",
        }
    }

    /// The JSON body for a text message on this channel (each platform names the field differently).
    pub fn payload(&self, text: &str) -> Value {
        match self {
            Channel::Discord => json!({ "content": truncate(text, DISCORD_MAX) }),
            Channel::Slack => json!({ "text": text }),
            Channel::Webhook => json!({ "text": text }),
        }
    }
}

/// Truncate to `max` *chars* (keeps UTF-8 valid), appending `…` when clipped.
fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let head: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

/// Resolve a channel's URL: env override wins over the config file. Empty/whitespace → `None`.
pub fn channel_url(ch: Channel, cfg: &CliConfig) -> Option<String> {
    if let Ok(v) = std::env::var(ch.env_var()) {
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    let n = cfg.notify.as_ref()?;
    let raw = match ch {
        Channel::Discord => n.discord_webhook.as_deref(),
        Channel::Slack => n.slack_webhook.as_deref(),
        Channel::Webhook => n.webhook_url.as_deref(),
    }?
    .trim();
    if raw.is_empty() {
        None
    } else {
        Some(raw.to_string())
    }
}

/// Write a channel's URL into a `NotifyConfig` (used by `/apps` setup/disable).
pub fn set_channel_url(n: &mut NotifyConfig, ch: Channel, url: Option<String>) {
    match ch {
        Channel::Discord => n.discord_webhook = url,
        Channel::Slack => n.slack_webhook = url,
        Channel::Webhook => n.webhook_url = url,
    }
}

/// Optional `(name, value)` auth header for the generic webhook (e.g. `Authorization: Bearer x`).
fn webhook_auth(cfg: &CliConfig) -> Option<(String, String)> {
    let raw = cfg.notify.as_ref()?.webhook_auth.as_deref()?.trim();
    let (name, value) = raw.split_once(':')?;
    let (name, value) = (name.trim(), value.trim());
    if name.is_empty() || value.is_empty() {
        None
    } else {
        Some((name.to_string(), value.to_string()))
    }
}

/// True if this channel has a URL (env or file).
pub fn is_configured(ch: Channel) -> bool {
    channel_url(ch, &cli_config::load()).is_some()
}

/// Channels that currently have a URL.
pub fn configured_channels() -> Vec<Channel> {
    let cfg = cli_config::load();
    Channel::ALL.iter().copied().filter(|c| channel_url(*c, &cfg).is_some()).collect()
}

/// True if any outbound channel is configured (decides whether to advertise the `notify` tool).
pub fn any_configured() -> bool {
    !configured_channels().is_empty()
}

/// POST a JSON body to a URL. 2xx = ok; otherwise an actionable error with a short body snippet.
async fn post(ch: Channel, url: &str, body: Value, auth: Option<(String, String)>) -> Result<()> {
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(SEND_TIMEOUT_SECS))
        .build()
        .context("building notify HTTP client")?;
    let mut req = http.post(url).json(&body);
    if let Some((name, value)) = auth {
        req = req.header(name, value);
    }
    let resp = req.send().await.with_context(|| format!("{} POST failed", ch.label()))?;
    let status = resp.status();
    if !status.is_success() {
        let snippet: String = resp.text().await.unwrap_or_default().chars().take(200).collect();
        bail!("{} returned HTTP {} — {}", ch.label(), status.as_u16(), snippet.trim());
    }
    Ok(())
}

/// Send `text` to one channel using the current config. Errors if that channel isn't configured.
pub async fn send_to(ch: Channel, text: &str) -> Result<()> {
    let cfg = cli_config::load();
    let url =
        channel_url(ch, &cfg).with_context(|| format!("{} not configured (set it in /apps)", ch.label()))?;
    let auth = if ch == Channel::Webhook { webhook_auth(&cfg) } else { None };
    post(ch, &url, ch.payload(text), auth).await
}

/// Send `text` to every configured channel; returns `(channel, result)` per channel attempted.
pub async fn broadcast(text: &str) -> Vec<(Channel, Result<()>)> {
    let mut out = Vec::new();
    for ch in configured_channels() {
        let r = send_to(ch, text).await;
        out.push((ch, r));
    }
    out
}

/// Bridge an async future to the sync `Tool::execute` path — the shared cancel-aware bridge
/// (valid on workers AND spawn_blocking threads; Esc aborts an in-flight webhook send).
fn block<T>(f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    crate::agent::tools::block_for_tool(f)
}

// ── agent tool ─────────────────────────────────────────────────────────────────

pub struct Notify;
impl Tool for Notify {
    fn name(&self) -> &str {
        "notify"
    }
    fn description(&self) -> &str {
        "Broadcast a short status / result / alert to the owner's connected channels (Discord, \
         Slack, custom webhook) at once. One-way — use when running unattended to report progress \
         or a finished result. For Telegram use telegram_send; to ASK yes/no use telegram_ask."
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
        let results = block(async { anyhow::Ok(broadcast(text).await) })?;
        if results.is_empty() {
            return Ok("error: no notification channels configured (add one in /apps)".to_string());
        }
        let mut ok: Vec<&str> = Vec::new();
        let mut errs: Vec<String> = Vec::new();
        for (ch, r) in results {
            match r {
                Ok(()) => ok.push(ch.key()),
                Err(e) => errs.push(format!("{} ({e})", ch.key())),
            }
        }
        let mut msg = String::new();
        if !ok.is_empty() {
            msg.push_str(&format!("sent to {}", ok.join(", ")));
        }
        if !errs.is_empty() {
            if !msg.is_empty() {
                msg.push_str("; ");
            }
            msg.push_str(&format!("failed: {}", errs.join(", ")));
        }
        Ok(msg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg_with(n: NotifyConfig) -> CliConfig {
        CliConfig { notify: Some(n), ..Default::default() }
    }

    #[test]
    fn channel_keys_and_payload_fields() {
        assert_eq!(Channel::Discord.key(), "discord");
        assert_eq!(Channel::Slack.key(), "slack");
        assert_eq!(Channel::Webhook.key(), "webhook");
        // each platform names the message field differently
        assert_eq!(Channel::Discord.payload("hi")["content"], "hi");
        assert_eq!(Channel::Slack.payload("hi")["text"], "hi");
        assert_eq!(Channel::Webhook.payload("hi")["text"], "hi");
    }

    #[test]
    fn discord_payload_truncates_over_2000() {
        let long = "x".repeat(5000);
        let p = Channel::Discord.payload(&long);
        let content = p["content"].as_str().unwrap();
        assert_eq!(content.chars().count(), DISCORD_MAX, "Discord content must be ≤ 2000 chars");
        assert!(content.ends_with('…'));
    }

    #[test]
    fn truncate_keeps_short_text_verbatim() {
        assert_eq!(truncate("hello", 2000), "hello");
        assert_eq!(truncate("über", 10), "über"); // multi-byte char preserved
    }

    #[test]
    fn channel_url_reads_each_field_and_skips_empty() {
        let n = NotifyConfig {
            discord_webhook: Some("https://discord.com/api/webhooks/1/abc".into()),
            slack_webhook: Some("   ".into()), // whitespace → None
            webhook_url: None,
            webhook_auth: None,
        };
        let cfg = cfg_with(n);
        assert_eq!(
            channel_url(Channel::Discord, &cfg).as_deref(),
            Some("https://discord.com/api/webhooks/1/abc")
        );
        assert_eq!(channel_url(Channel::Slack, &cfg), None);
        assert_eq!(channel_url(Channel::Webhook, &cfg), None);
        // no notify block at all → all None
        assert_eq!(channel_url(Channel::Discord, &CliConfig::default()), None);
    }

    #[test]
    fn set_channel_url_round_trips() {
        let mut n = NotifyConfig::default();
        set_channel_url(&mut n, Channel::Slack, Some("https://hooks.slack.com/x".into()));
        assert_eq!(n.slack_webhook.as_deref(), Some("https://hooks.slack.com/x"));
        set_channel_url(&mut n, Channel::Slack, None);
        assert_eq!(n.slack_webhook, None);
    }

    #[test]
    fn webhook_auth_parses_header_pair() {
        let cfg = cfg_with(NotifyConfig {
            webhook_auth: Some("Authorization: Bearer tok123".into()),
            ..Default::default()
        });
        assert_eq!(webhook_auth(&cfg), Some(("Authorization".into(), "Bearer tok123".into())));
        // malformed / empty → None
        assert_eq!(webhook_auth(&cfg_with(NotifyConfig { webhook_auth: Some("no-colon".into()), ..Default::default() })), None);
        assert_eq!(webhook_auth(&CliConfig::default()), None);
    }

    #[test]
    fn notify_tool_is_serial_and_nondestructive() {
        assert_eq!(Notify.name(), "notify");
        assert!(!Notify.is_concurrency_safe()); // block_in_place forbids the parallel path
        assert!(!Notify.is_destructive());
    }
}
