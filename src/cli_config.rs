//! Persistent CLI endpoint config (`ng config`) — base URL / API key / default model stored at
//! `~/.aizen/cli-config.json`, so the network commands work without re-passing env vars.
//!
//! Resolution precedence for every network command: explicit `--flag` > `NG_*` env var > this
//! file. (clap merges flag+env into the arg; we fall back to the file when that's still absent.)
//! The key is stored in plaintext (standard for a CLI credential file, like `~/.aws/credentials`)
//! but the file is tightened to owner-only (0600) on Unix at write time — see `save`.

use crate::config::nextgen_home;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CliConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Context window (tokens) for `model`, learned from the provider's `/models` (when it reports
    /// one) or set manually. Drives the `% context` HUD. `None` ⇒ HUD uses a name heuristic.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_context_window: Option<usize>,
    /// Auto-compact threshold as a percent of the context window (the REPL summarizes older turns
    /// when usage crosses it). `None` ⇒ default 80%. `Some(0)` ⇒ auto-compact disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compact_threshold_pct: Option<u8>,
    /// Auto-learn skills: after a multi-step task the REPL distills a reusable procedure into a
    /// skill. `None` ⇒ default ON. `Some(false)` ⇒ disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_skill_learn: Option<bool>,
    /// Auto-learn memory: after each turn the REPL passively learns durable user/project facts from
    /// the user's message (FREE regex extraction → sanitize → threat-scan → confidence route → store).
    /// Core promotion always stays human-gated. `None` ⇒ default ON. `Some(false)` ⇒ disabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_auto_learn: Option<bool>,
    /// "Yolo" mode: auto-approve destructive tools (file edits / shell) in the TUI without prompting
    /// for each one. `None`/`Some(false)` ⇒ ask before every destructive op (safe default).
    /// `Some(true)` ⇒ run them without asking. Toggle live with `/yolo`; `NG_YES` env also forces it on.
    /// The hard command blocklist (`cmd_guard`) still applies underneath — yolo skips the prompt, never the floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_approve: Option<bool>,
    /// "Smart" approval: auto-run read-only-shaped shell commands (`ls`/`cat`/`rg`/`git status`/
    /// `cargo check` …) without a prompt, while writes/network/installs/deletes still ask. Composes
    /// with `auto_approve` (yolo wins). `None`/`Some(false)` ⇒ ask for every destructive op (manual).
    /// Toggle live with `/smart`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub smart_approve: Option<bool>,
    /// Optional pricing for the `/cost` estimate: USD per 1,000,000 tokens, input/output. When both
    /// are set AND the provider reports real token usage, `/cost` shows an estimated session $.
    /// `None` ⇒ `/cost` shows tokens only (we never fabricate a price).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_in: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub price_out: Option<f64>,
    /// TUI icon style: `"emoji"` (default), `"nerd"` (Nerd Font glyphs), or `"off"`. `None` ⇒ emoji.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icons: Option<String>,
    /// Active persona name (a card under `~/.aizen/personas/`). `None` ⇒ default assistant voice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona: Option<String>,
    /// Persona evolution: when a persona is active, record episodes + periodically reflect them into
    /// durable insights (the `<self>` layer). `None` ⇒ default ON. `Some(false)` ⇒ frozen character.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persona_evolve: Option<bool>,
    /// Telegram bot integration (the `ng serve` daemon + telegram_send/telegram_ask tools).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telegram: Option<TelegramConfig>,
    /// Outbound notification channels (Discord / Slack / generic webhook) surfaced in `/apps` and
    /// driving the `notify` agent tool. One-way HTTP POST sinks — see `notify.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify: Option<NotifyConfig>,
    /// Anthropic prompt-cache breakpoints (cuts billed INPUT tokens on multi-turn sessions). `None`
    /// ⇒ AUTO (on only when the model name looks Anthropic — claude/opus/sonnet/…); `Some(true)`/
    /// `Some(false)` force it. A no-op (zero extra bytes) for providers that don't support caching.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_cache: Option<bool>,
    /// Cap on the model's OUTPUT tokens per call (a runaway-completion safety knob, not an input
    /// saving). `None` ⇒ provider default (no cap sent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// First-run flag: `Some(true)` once the welcome/onboarding intro has been shown, so a brand-new
    /// user sees it exactly once. `None` ⇒ never onboarded (a fresh install → show the intro).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onboarded: Option<bool>,
    /// Skill marketplace base URL for `skill_search`/`skill_install` (and `ng skill search/install`).
    /// `None` ⇒ the default `https://agentskill.sh`. Override to point at a private registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_registry: Option<String>,
    /// Max time-machine checkpoints retained (oldest auto-pruned past this on each save, never the
    /// active one). `None` ⇒ default 50. `Some(0)` ⇒ unlimited (keep everything).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timemachine_keep: Option<usize>,
    /// Discord two-way bot (the `ng discord serve` gateway daemon). Distinct from the one-way Discord
    /// webhook under [`NotifyConfig`] — this is a full bot that receives messages and replies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord: Option<DiscordConfig>,
}

/// Discord BOT (two-way) config — a bot token (from the Developer Portal) + an allowlist of channel
/// and user ids. Empty `allowed_channel_ids` denies everyone (secure default). Env `NG_DISCORD_BOT_TOKEN`
/// overrides the stored token.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct DiscordConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Channel ids the bot will respond in. Empty = respond nowhere (deny-by-default).
    #[serde(default)]
    pub allowed_channel_ids: Vec<u64>,
    /// Optional extra restriction: when non-empty, only these user ids may talk to the bot.
    #[serde(default)]
    pub allowed_user_ids: Vec<u64>,
}

impl DiscordConfig {
    /// The effective bot token: `NG_DISCORD_BOT_TOKEN` env wins over the config file.
    pub fn resolved_token(&self) -> Option<String> {
        std::env::var("NG_DISCORD_BOT_TOKEN").ok().filter(|s| !s.trim().is_empty()).or_else(|| self.token.clone())
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// Bot token from @BotFather. Prefer the `NG_TELEGRAM_TOKEN` env var over storing it here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Chat ids allowed to talk to / approve the agent. Messages from any other chat are dropped.
    #[serde(default)]
    pub allowed_chat_ids: Vec<i64>,
}

impl TelegramConfig {
    /// The effective token: `NG_TELEGRAM_TOKEN` env wins over the config file.
    pub fn resolved_token(&self) -> Option<String> {
        std::env::var("NG_TELEGRAM_TOKEN").ok().filter(|s| !s.trim().is_empty()).or_else(|| self.token.clone())
    }
}

/// Outbound notification channels — one-way HTTP POST sinks. Each URL can also be supplied via env
/// (`NG_DISCORD_WEBHOOK`, `NG_SLACK_WEBHOOK`, `NG_WEBHOOK_URL`), which overrides the stored value.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct NotifyConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord_webhook: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slack_webhook: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
    /// Optional `Header: value` line sent with the generic webhook POST (e.g. `Authorization: Bearer …`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub webhook_auth: Option<String>,
}

/// `~/.aizen/cli-config.json`.
pub fn config_path() -> PathBuf {
    nextgen_home().join("cli-config.json")
}

/// Load the config, or an empty one if the file is missing/unreadable/corrupt (never fails). A
/// CORRUPT file is surfaced (once) instead of silently vanishing the user's endpoint+key — and
/// `save` preserves it as `.bak` before overwriting, so the settings stay recoverable.
pub fn load() -> CliConfig {
    let path = config_path();
    let s = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return CliConfig::default(), // missing/unreadable → empty (normal on first run)
    };
    match serde_json::from_str(&s) {
        Ok(cfg) => cfg,
        Err(e) => {
            use std::sync::atomic::{AtomicBool, Ordering};
            static WARNED: AtomicBool = AtomicBool::new(false);
            if !WARNED.swap(true, Ordering::Relaxed) {
                eprintln!(
                    "warning: {} is corrupt ({e}); using defaults. A .bak copy is kept before any \
                     re-save — run `ng config` to repair.",
                    path.display()
                );
            }
            CliConfig::default()
        }
    }
}

/// Persist the config (creates `~/.aizen/` if needed). The file holds the gateway api_key, so it is
/// tightened to owner-only (0600, and the home dir to 0700) on Unix — matching the OAuth/MCP token
/// caches. No-op on Windows (user-profile ACL governs).
pub fn save(cfg: &CliConfig) -> Result<()> {
    let path = config_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        crate::config::harden_dir(parent);
    }
    // If the file on disk is currently corrupt, preserve it as `.bak` before we clobber it — so a
    // hand-edit typo or a partial write doesn't silently destroy the rest of the user's settings.
    if let Ok(cur) = std::fs::read_to_string(&path) {
        if serde_json::from_str::<CliConfig>(&cur).is_err() {
            let _ = std::fs::copy(&path, path.with_extension("json.bak"));
        }
    }
    let json = serde_json::to_string_pretty(cfg)?;
    std::fs::write(&path, json + "\n").with_context(|| format!("writing {}", path.display()))?;
    crate::config::harden_file(&path);
    Ok(())
}

/// Mask a secret for display: first 6 chars + length, never the full key.
pub fn mask(key: &str) -> String {
    let n = key.chars().count();
    if n <= 8 {
        "***".to_string()
    } else {
        let head: String = key.chars().take(6).collect();
        format!("{head}***({n} chars)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_disk() {
        let _g = crate::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);

        assert!(load().base_url.is_none(), "missing file → empty config");
        save(&CliConfig {
            base_url: Some("https://x/v1".into()),
            api_key: Some("sk-secret".into()),
            model: Some("sonnet-4-6".into()),
            model_context_window: Some(200_000),
            ..Default::default()
        })
        .unwrap();
        let got = load();
        assert_eq!(got.base_url.as_deref(), Some("https://x/v1"));
        assert_eq!(got.api_key.as_deref(), Some("sk-secret"));
        assert_eq!(got.model.as_deref(), Some("sonnet-4-6"));
        assert_eq!(got.model_context_window, Some(200_000));

        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mask_hides_the_secret() {
        assert_eq!(mask("short"), "***");
        let m = mask("sk-f49abcdef0123456789");
        assert!(m.starts_with("sk-f49"));
        assert!(!m.contains("0123456789"));
        assert!(m.contains("chars"));
    }
}
