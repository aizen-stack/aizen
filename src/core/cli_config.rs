//! Persistent CLI endpoint config (`ng config`) — base URL / API key / default model stored at
//! `~/.aizen/cli-config.json`, so the network commands work without re-passing env vars.
//!
//! Resolution precedence for every network command: explicit `--flag` > `NG_*` env var > this
//! file. (clap merges flag+env into the arg; we fall back to the file when that's still absent.)
//! The key is stored in plaintext (standard for a CLI credential file, like `~/.aws/credentials`)
//! but the file is tightened to owner-only (0600) on Unix at write time — see `save`.

use crate::core::config::nextgen_home;
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
    /// System-prompt tier override: `"full"` or `"strict"` (the compact numbered-rules prompt for
    /// small/local models). `None` ⇒ auto by model-id heuristic (`agent::prompt_tier_for`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tier: Option<String>,
    /// Eager tool execution during streaming: read-only tool calls start the moment their streamed
    /// arguments complete. `None` ⇒ ON. `Some(false)` ⇒ wait for the full response (also the
    /// `NG_NO_EAGER` env kill-switch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eager_tools: Option<bool>,
    /// Reasoning-effort passthrough for reasoning models ("low"/"medium"/"high"; provider
    /// validates). `None` ⇒ field omitted from requests entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Fold NEW LSP diagnostics into edit-tool results (only meaningful while LSP is on).
    /// `None` ⇒ ON. Toggle live with `/lsp edits on|off`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsp_edit_diagnostics: Option<bool>,
    /// One extra self-review turn before Done on runs that edited files (diff vs request; uses
    /// `roles.oracle` when configured). `None` ⇒ OFF (costs a turn per editing task).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub self_review: Option<bool>,
    /// Concurrent sub-agent cap for parallel read-only `task` dispatches. `None` ⇒ 3 (clamp 1..=5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_parallel_subagents: Option<usize>,
    /// Register the `workflow` fan-out tool. `None` ⇒ auto (only when specialist agents are
    /// installed — the schema costs ~350 tok/turn). `Some(true/false)` forces it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow_tool: Option<bool>,
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
    /// Reach layer (platform-aware web access) optional keys — everything works keyless; these only
    /// raise limits / unlock extras. See `agent::reach` and `/reach`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reach: Option<ReachConfig>,
    /// Per-ROLE model routing: point harness chores (compaction summaries), sub-agents, and the
    /// self-review oracle at different OpenAI-compatible endpoints/models than the main loop —
    /// cheap-fast for chores, stronger for review. Every field optional; unset ⇒ the main model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<RolesConfig>,
}

/// One role's endpoint override. Any subset of fields; the rest inherit the main endpoint.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RoleModelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// `env:VAR` (preferred — the key never touches disk) or a literal key (masked in displays).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_ref: Option<String>,
}

/// The routable roles. `summarizer` = compaction/handoff summaries; `subagent_default` = the task
/// tool's fallback model; `oracle` = the self-review reviewer (stronger model recommended);
/// `apply` = reserved for a future fast-apply edit model (config-only today).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RolesConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summarizer: Option<RoleModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_default: Option<RoleModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oracle: Option<RoleModelConfig>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply: Option<RoleModelConfig>,
}

/// A fully-resolved (base_url, api_key, model) triple for one call.
#[derive(Debug, Clone)]
pub struct ResolvedEndpoint {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

/// Resolve a role's endpoint. Per-field precedence: `NG_<ROLE>_MODEL/BASE_URL/API_KEY` env >
/// `roles.<role>.*` config > the main endpoint. `api_key_ref` supports `env:VAR` indirection.
/// Unknown role name ⇒ the main endpoint unchanged.
pub fn resolve_role(role: &str, main: &ResolvedEndpoint) -> ResolvedEndpoint {
    let up = role.to_ascii_uppercase();
    let cfg = load();
    let rc = cfg.roles.as_ref().and_then(|r| match role {
        "summarizer" => r.summarizer.clone(),
        "subagent_default" => r.subagent_default.clone(),
        "oracle" => r.oracle.clone(),
        "apply" => r.apply.clone(),
        _ => None,
    });
    let key_from_ref = |r: &RoleModelConfig| {
        r.api_key_ref.as_ref().and_then(|k| match k.strip_prefix("env:") {
            Some(var) => env_nonempty(var),
            None => Some(k.clone()),
        })
    };
    ResolvedEndpoint {
        model: env_nonempty(&format!("NG_{up}_MODEL"))
            .or_else(|| rc.as_ref().and_then(|r| r.model.clone()))
            .unwrap_or_else(|| main.model.clone()),
        base_url: env_nonempty(&format!("NG_{up}_BASE_URL"))
            .or_else(|| rc.as_ref().and_then(|r| r.base_url.clone()))
            .unwrap_or_else(|| main.base_url.clone()),
        api_key: env_nonempty(&format!("NG_{up}_API_KEY"))
            .or_else(|| rc.as_ref().and_then(key_from_ref))
            .unwrap_or_else(|| main.api_key.clone()),
    }
}

/// Is any override configured for `role` (config or env)? Consumers that should stay OFF without
/// an explicit oracle (e.g. self-review's oracle mode) check this instead of comparing endpoints.
pub fn role_configured(role: &str) -> bool {
    let up = role.to_ascii_uppercase();
    if env_nonempty(&format!("NG_{up}_MODEL")).is_some() {
        return true;
    }
    let cfg = load();
    cfg.roles
        .as_ref()
        .and_then(|r| match role {
            "summarizer" => r.summarizer.as_ref(),
            "subagent_default" => r.subagent_default.as_ref(),
            "oracle" => r.oracle.as_ref(),
            "apply" => r.apply.as_ref(),
            _ => None,
        })
        .is_some_and(|r| r.model.is_some() || r.base_url.is_some() || r.api_key_ref.is_some())
}

/// Optional credentials for the reach layer. All channels have a keyless path; a key only upgrades:
/// Jina key → higher r.jina.ai quota + unlocks s.jina.ai search fallback; GitHub token → 5000
/// requests/h instead of 60. Env always wins (see `resolved_*`).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ReachConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jina_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_token: Option<String>,
}

impl ReachConfig {
    /// Effective Jina key: `NG_JINA_API_KEY` > `JINA_API_KEY` > config file.
    pub fn resolved_jina_key(&self) -> Option<String> {
        env_nonempty("NG_JINA_API_KEY").or_else(|| env_nonempty("JINA_API_KEY")).or_else(|| self.jina_api_key.clone())
    }
    /// Effective GitHub token: `NG_GITHUB_TOKEN` > the conventional `GITHUB_TOKEN` > config file.
    pub fn resolved_github_token(&self) -> Option<String> {
        env_nonempty("NG_GITHUB_TOKEN").or_else(|| env_nonempty("GITHUB_TOKEN")).or_else(|| self.github_token.clone())
    }
}

fn env_nonempty(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|s| !s.trim().is_empty())
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
                     re-save — run `aizen config` to repair.",
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
        crate::core::config::harden_dir(parent);
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
    crate::core::config::harden_file(&path);
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
    fn role_routing_resolves_with_fallback() {
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-roles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);
        let main = ResolvedEndpoint {
            base_url: "https://main/v1".into(),
            api_key: "mk".into(),
            model: "main-model".into(),
        };
        // No config → the main endpoint unchanged; role reads as unconfigured.
        let r = resolve_role("summarizer", &main);
        assert_eq!(
            (r.model.as_str(), r.base_url.as_str(), r.api_key.as_str()),
            ("main-model", "https://main/v1", "mk")
        );
        assert!(!role_configured("oracle"));
        // Config: model-only summarizer (endpoint inherits) + env-indirected oracle key.
        save(&CliConfig {
            roles: Some(RolesConfig {
                summarizer: Some(RoleModelConfig { model: Some("cheap".into()), ..Default::default() }),
                oracle: Some(RoleModelConfig {
                    api_key_ref: Some("env:NG_TEST_ORACLE_KEY".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
        let r = resolve_role("summarizer", &main);
        assert_eq!(r.model, "cheap");
        assert_eq!(r.base_url, "https://main/v1", "unset fields inherit the main endpoint");
        assert!(role_configured("summarizer"));
        assert!(role_configured("oracle"), "an api_key_ref alone counts as configured");
        std::env::set_var("NG_TEST_ORACLE_KEY", "ok-secret");
        assert_eq!(resolve_role("oracle", &main).api_key, "ok-secret", "env: indirection resolves");
        std::env::remove_var("NG_TEST_ORACLE_KEY");
        // Env beats config.
        std::env::set_var("NG_SUMMARIZER_MODEL", "env-model");
        assert_eq!(resolve_role("summarizer", &main).model, "env-model");
        std::env::remove_var("NG_SUMMARIZER_MODEL");
        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn round_trips_through_disk() {
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
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
