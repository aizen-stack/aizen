//! Persistent CLI endpoint config (`ng config`) — base URL / API key / default model stored at
//! `~/.aizen/cli-config.json`, so the network commands work without re-passing env vars.
//!
//! Resolution precedence for every network command: explicit `--flag` > `AIZEN_*` env var > this
//! file. (clap merges flag+env into the arg; we fall back to the file when that's still absent.)
//! The key is stored in plaintext (standard for a CLI credential file, like `~/.aws/credentials`)
//! but the file is tightened to owner-only (0600) on Unix at write time — see `save`.

use crate::core::approval::ApprovalMode;
use crate::core::config::nextgen_home;
use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::RwLock;

/// How strongly final answers should use terminal-native visual structure.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ResponseVisuals {
    #[default]
    Auto,
    Always,
    Off,
}

impl fmt::Display for ResponseVisuals {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self { Self::Auto => "auto", Self::Always => "always", Self::Off => "off" })
    }
}

impl FromStr for ResponseVisuals {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Ok(Self::Auto),
            "always" => Ok(Self::Always),
            "off" => Ok(Self::Off),
            _ => Err("response visuals must be one of: auto, always, off".to_string()),
        }
    }
}

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
    /// `AIZEN_NO_EAGER` env kill-switch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eager_tools: Option<bool>,
    /// Reasoning-effort passthrough for reasoning models ("low"/"medium"/"high"; provider
    /// validates). `None` ⇒ field omitted from requests entirely.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    /// Auto-detect reasoning effort per-turn (keyword + complexity — see `core::effort`). `None` ⇒
    /// ON (default). `Some(true)` forces ON, `Some(false)` disables it (then only the fixed
    /// `reasoning_effort` above, if any, is used). The per-turn effort is NEVER persisted here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_effort: Option<bool>,
    /// "Ultimate mode": pin max reasoning effort + prefer launching workflows (orchestrate-by-default).
    /// The aizen analogue of Claude Code's `ultracode` (xhigh + standing orchestration permission).
    /// `None`/`Some(false)` ⇒ off; `Some(true)` ⇒ on. Toggle live with `/ultimate`, or force via
    /// `AIZEN_ULTIMATE`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ultimate: Option<bool>,
    /// Adaptive difficulty→effort routing (P3, opt-in): when on, the per-turn complexity heuristic may
    /// climb past `high` to `xhigh` for the very hardest turns. `None`/`Some(false)` ⇒ off (the
    /// heuristic caps at `high`, unchanged default). Force via `AIZEN_ADAPTIVE_EFFORT`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub adaptive_effort: Option<bool>,
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
    /// Unified approval level for agent tools. `None` ⇒ `ask` (safe default). `smart` auto-runs
    /// read-only-shaped shell commands; `yolo` pre-authorizes destructive tools after the hard floor.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub approval_mode: Option<ApprovalMode>,
    /// Legacy `/yolo` config field. Read for migration only; normalized into `approval_mode` on save.
    #[serde(default, skip_serializing)]
    pub auto_approve: Option<bool>,
    /// Legacy `/smart` config field. Read for migration only; normalized into `approval_mode` on save.
    #[serde(default, skip_serializing)]
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
    /// Final-answer visuals: `auto` (when useful), `always` (substantial replies), or `off`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_visuals: Option<ResponseVisuals>,
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
    /// Maximum files in one Time Machine snapshot. `None` ⇒ 100,000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timemachine_max_files: Option<u64>,
    /// Maximum aggregate Git blob bytes in one snapshot. `None` ⇒ 2 GiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timemachine_max_bytes: Option<u64>,
    /// Maximum size of one file/blob in a snapshot. `None` ⇒ 512 MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timemachine_max_file_bytes: Option<u64>,
    /// Discord two-way bot (the `ng discord serve` gateway daemon). Distinct from the one-way Discord
    /// webhook under [`NotifyConfig`] — this is a full bot that receives messages and replies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord: Option<DiscordConfig>,
    /// Reach layer (platform-aware web access) optional keys — everything works keyless; these only
    /// raise limits / unlock extras. See `agent::reach` and `/reach`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reach: Option<ReachConfig>,
    /// Hermes-style tool bundles to **disable** on the top-level agent (e.g. `web`, `browser`,
    /// `delegation`, `mcp`). Shrinks the tool schema sent to the model each turn. Sub-agent
    /// registries are unaffected. See `agent::toolsets::CATALOG`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_toolsets: Option<Vec<String>>,
    /// Optional **whitelist**: when non-empty, only these bundles are advertised (plus any tool
    /// whose bundle is unknown). Overrides `disabled_toolsets` for listed ids. Rare; prefer disable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_toolsets: Option<Vec<String>>,
    /// Per-ROLE model routing: point harness chores (compaction summaries), sub-agents, and the
    /// self-review oracle at different OpenAI-compatible endpoints/models than the main loop —
    /// cheap-fast for chores, stronger for review. Every field optional; unset ⇒ the main model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roles: Option<RolesConfig>,
    /// Model → endpoint registry: when a sub-agent (or role) is pinned to a model name, we look it
    /// up here to find the base_url/api_key that model actually lives on — so a specialist pinned to
    /// e.g. `gpt-4o` runs on ITS gateway, not the parent's. Without a match, only the model name
    /// changes and the endpoint stays the caller's (which only works when they share a gateway).
    /// Keyed by exact model id. See [`endpoint_for_model`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_endpoints: Option<Vec<ModelEndpoint>>,
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

impl RolesConfig {
    /// Any role configured? Used to drop the whole `roles` object when its last sub-field is cleared.
    pub fn has_any(&self) -> bool {
        self.summarizer.is_some()
            || self.subagent_default.is_some()
            || self.oracle.is_some()
            || self.apply.is_some()
    }
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

/// One entry in the model → endpoint registry. `model` is the exact model id a sub-agent/role can
/// be pinned to; `base_url`/`api_key_ref` say where that model lives. Any field except `model` is
/// optional — an absent field inherits the caller's endpoint (so a same-gateway alias needs only
/// `model`). `api_key_ref` follows the `env:VAR` indirection convention (the key never touches disk).
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ModelEndpoint {
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// `env:VAR` (preferred — the key never touches disk) or a literal key (masked in displays).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_ref: Option<String>,
}

/// Resolve the full endpoint a `model` should run on: if the model-endpoint registry has an entry
/// for it, that entry's base_url/api_key override `caller` (per field; absent fields inherit
/// `caller`); otherwise only the model name changes and the endpoint stays `caller`. Env override:
/// `NG_MODEL_<UPPER>_BASE_URL` / `NG_MODEL_<UPPER>_API_KEY` (the model id uppercased, non-alnum →
/// `_`) wins over the config entry, matching `resolve_role`'s env-first precedence.
///
/// This is what makes "assign a model to a sub-agent" work across providers: the model carries its
/// own gateway with it instead of being sent to the parent's.
pub fn endpoint_for_model(model: &str, caller: &ResolvedEndpoint) -> ResolvedEndpoint {
    let cfg = load();
    let entry = cfg
        .model_endpoints
        .as_ref()
        .and_then(|list| list.iter().find(|e| e.model == model).cloned());
    // Env key: NG_MODEL_<sanitized-upper>_{BASE_URL,API_KEY}. Sanitize so `gpt-4o` → `GPT_4O`.
    let up: String = model
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect();
    let key_from_ref = |e: &ModelEndpoint| {
        e.api_key_ref.as_ref().and_then(|k| match k.strip_prefix("env:") {
            Some(var) => env_nonempty(var),
            None => Some(k.clone()),
        })
    };
    ResolvedEndpoint {
        model: model.to_string(),
        base_url: env_nonempty(&format!("NG_MODEL_{up}_BASE_URL"))
            .or_else(|| entry.as_ref().and_then(|e| e.base_url.clone()))
            .unwrap_or_else(|| caller.base_url.clone()),
        api_key: env_nonempty(&format!("NG_MODEL_{up}_API_KEY"))
            .or_else(|| entry.as_ref().and_then(key_from_ref))
            .unwrap_or_else(|| caller.api_key.clone()),
    }
}

/// The sub-agent's default endpoint: `roles.subagent_default` (env `NG_SUBAGENT_DEFAULT_*` >
/// config > `main`), THEN routed through the model-endpoint registry so even the role-default model
/// carries its own gateway. When `subagent_default` sets only `.model`, `endpoint_for_model` finds
/// that model's endpoint; when it sets base_url/api_key too, those are the caller `endpoint_for_model`
/// inherits from (registry entry for the same model can still override). The task tool uses this as
/// its fallback endpoint (below an explicit `model` arg / a card's `def.model`, both of which route
/// through `endpoint_for_model` themselves).
pub fn subagent_endpoint(main: &ResolvedEndpoint) -> ResolvedEndpoint {
    let role = resolve_role("subagent_default", main);
    endpoint_for_model(&role.model, &role)
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
    /// Tavily search key. Web search is now KEYED-ONLY (DuckDuckGo scraping was dropped — it sat
    /// behind an anomaly wall too often to be a reliable floor). Tavily is the primary web-search
    /// backend; without it (and without a Jina key) `web_search` returns an actionable "add a key"
    /// error rather than silently degrading. Get one free at tavily.com.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tavily_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jina_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub github_token: Option<String>,
}

impl ReachConfig {
    /// Effective Tavily key: `AIZEN_TAVILY_API_KEY` > `TAVILY_API_KEY` > config file.
    pub fn resolved_tavily_key(&self) -> Option<String> {
        branded_env("TAVILY_API_KEY").or_else(|| env_nonempty("TAVILY_API_KEY")).or_else(|| self.tavily_api_key.clone())
    }
    /// Effective Jina key: `AIZEN_JINA_API_KEY` > `JINA_API_KEY` > config file.
    pub fn resolved_jina_key(&self) -> Option<String> {
        branded_env("JINA_API_KEY").or_else(|| env_nonempty("JINA_API_KEY")).or_else(|| self.jina_api_key.clone())
    }
    /// Effective GitHub token: `AIZEN_GITHUB_TOKEN` > the conventional `GITHUB_TOKEN` > config file.
    pub fn resolved_github_token(&self) -> Option<String> {
        branded_env("GITHUB_TOKEN").or_else(|| env_nonempty("GITHUB_TOKEN")).or_else(|| self.github_token.clone())
    }
}

fn env_nonempty(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|s| !s.trim().is_empty())
}

/// Read a brand-prefixed user-facing setting env var: `AIZEN_<suffix>`. Empty/whitespace ⇒ `None`.
pub fn branded_env(suffix: &str) -> Option<String> {
    env_nonempty(&format!("AIZEN_{suffix}"))
}

/// Presence check for a brand-prefixed boolean toggle env var: `AIZEN_<suffix>` set (to anything) ⇒ true.
pub fn branded_flag(suffix: &str) -> bool {
    std::env::var_os(format!("AIZEN_{suffix}")).is_some()
}

impl CliConfig {
    /// Resolve the persisted approval level, accepting the pre-unification boolean fields as a
    /// migration fallback. The new enum always wins; if both legacy toggles are true, yolo keeps the
    /// old runtime precedence over smart.
    pub fn persisted_approval_mode(&self) -> ApprovalMode {
        self.approval_mode.unwrap_or_else(|| {
            if self.auto_approve.unwrap_or(false) {
                ApprovalMode::Yolo
            } else if self.smart_approve.unwrap_or(false) {
                ApprovalMode::Smart
            } else {
                ApprovalMode::Ask
            }
        })
    }

    /// Store one canonical approval field and clear the legacy migration inputs.
    pub fn set_approval_mode(&mut self, mode: ApprovalMode) {
        self.approval_mode = Some(mode);
        self.auto_approve = None;
        self.smart_approve = None;
    }

    pub fn response_visuals(&self) -> ResponseVisuals {
        self.response_visuals.unwrap_or_default()
    }

    fn normalize_approval(&mut self) {
        let mode = self.persisted_approval_mode();
        self.set_approval_mode(mode);
    }
}

/// Effective approval for interactive/persisted callers. `AIZEN_YES` is the explicit environment
/// escape hatch and forces yolo without mutating the saved preference.
pub fn approval_mode() -> ApprovalMode {
    if branded_flag("YES") {
        ApprovalMode::Yolo
    } else {
        load().persisted_approval_mode()
    }
}

/// Per-turn reasoning-effort override, set by the REPL for one user turn and read by the LLM client
/// when it builds a request. Kept OUT of `CliConfig` on purpose — effort is a per-turn decision, not
/// a persisted setting. The nesting distinguishes three states:
/// - outer `None`  ⇒ NO override active this turn (subagents/summarizer/oracle ⇒ client uses `cfg.reasoning_effort`).
/// - `Some(None)`  ⇒ override active, but "omit" (send no `reasoning_effort` on the wire).
/// - `Some(Some(e))` ⇒ override active, send effort `e`.
static EFFORT_OVERRIDE: Lazy<RwLock<Option<Option<String>>>> = Lazy::new(|| RwLock::new(None));

/// Arm the per-turn effort override (called once per turn from the REPL, before the turn runs).
pub fn set_effort_override(v: Option<String>) {
    *EFFORT_OVERRIDE.write().unwrap_or_else(|e| e.into_inner()) = Some(v);
}

/// Disarm the per-turn override (called at turn end, so effort never leaks into the next turn).
pub fn clear_effort_override() {
    *EFFORT_OVERRIDE.write().unwrap_or_else(|e| e.into_inner()) = None;
}

/// Read the current per-turn override. Outer `None` ⇒ no override armed (use the config default).
pub fn effort_override() -> Option<Option<String>> {
    EFFORT_OVERRIDE.read().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Resolve the `reasoning_effort` to stamp on an outgoing request: the per-turn override when one
/// is armed (main REPL turns), else the caller-supplied config default (subagents / summarizer /
/// oracle, which never arm an override). `None` ⇒ omit the field (request stays byte-identical).
pub fn resolved_reasoning_effort(config_default: Option<String>) -> Option<String> {
    match effort_override() {
        Some(inner) => inner,
        None => config_default,
    }
}

/// RAII guard that temporarily DISARMS the per-turn effort override for its lifetime, restoring the
/// prior state on drop. The problem: the override is a process-global armed at the top of a REPL
/// turn (e.g. ultimate mode pins `max`) and only cleared when the whole turn ends. A `task`/workflow
/// sub-agent runs SYNCHRONOUSLY inside that turn (`block_in_place` + `block_on`), so every
/// `chat_with_tools` call it makes would read the still-armed parent tier via
/// `resolved_reasoning_effort` — the parent's `max` leaks down the whole fan-out. Wrapping the
/// sub-agent dispatch in this guard makes `resolved_reasoning_effort` fall through to the caller's
/// own `cfg.reasoning_effort` (exactly what the doc above promises for subagents), then restores the
/// parent's override for the rest of the turn. Serial-by-construction: the guard is held across a
/// blocking dispatch, so no concurrent parent turn can observe the disarmed window.
pub struct EffortOverrideSuppressed(Option<Option<String>>);

/// Disarm the per-turn effort override until the returned guard drops. See `EffortOverrideSuppressed`.
#[must_use = "the override stays disarmed only while the guard is held"]
pub fn suppress_effort_override() -> EffortOverrideSuppressed {
    let prior = effort_override();
    *EFFORT_OVERRIDE.write().unwrap_or_else(|e| e.into_inner()) = None;
    EffortOverrideSuppressed(prior)
}

impl Drop for EffortOverrideSuppressed {
    fn drop(&mut self) {
        // Restore the exact prior override state (outer None = disarmed, Some(inner) = armed).
        *EFFORT_OVERRIDE.write().unwrap_or_else(|e| e.into_inner()) = self.0.take();
    }
}

/// Is per-turn effort auto-detection ON? `AIZEN_AUTO_EFFORT` env wins (`1`/`true`/`on`/`yes` ⇒ on,
/// anything else ⇒ off); otherwise the `auto_effort` config field, defaulting to ON when unset.
pub fn auto_effort_enabled() -> bool {
    if let Ok(v) = std::env::var("AIZEN_AUTO_EFFORT") {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes");
    }
    load().auto_effort.unwrap_or(true)
}

/// Is "ultimate mode" ON? `AIZEN_ULTIMATE` env wins (`1`/`true`/`on`/`yes` ⇒ on); otherwise the
/// `ultimate` config field, defaulting to OFF. Mirrors `auto_effort_enabled` (env-forced, else config).
pub fn ultimate_enabled() -> bool {
    if let Ok(v) = std::env::var("AIZEN_ULTIMATE") {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes");
    }
    load().ultimate.unwrap_or(false)
}

/// Is adaptive difficulty→effort routing ON (P3)? `AIZEN_ADAPTIVE_EFFORT` env wins; otherwise the
/// `adaptive_effort` config field, defaulting to OFF (so the heuristic caps at `high` by default).
pub fn adaptive_effort_enabled() -> bool {
    if let Ok(v) = std::env::var("AIZEN_ADAPTIVE_EFFORT") {
        return matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "on" | "yes");
    }
    load().adaptive_effort.unwrap_or(false)
}

/// Discord BOT (two-way) config — a bot token (from the Developer Portal) + an allowlist of channel
/// and user ids. Empty `allowed_channel_ids` denies everyone (secure default). Env `AIZEN_DISCORD_BOT_TOKEN`
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
    /// The effective bot token: `AIZEN_DISCORD_BOT_TOKEN` env wins over the config file.
    pub fn resolved_token(&self) -> Option<String> {
        branded_env("DISCORD_BOT_TOKEN").or_else(|| self.token.clone())
    }
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct TelegramConfig {
    /// Bot token from @BotFather. Prefer the `AIZEN_TELEGRAM_TOKEN` env var over storing it here.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// Chat ids allowed to talk to / approve the agent. Messages from any other chat are dropped.
    #[serde(default)]
    pub allowed_chat_ids: Vec<i64>,
}

impl TelegramConfig {
    /// The effective token: `AIZEN_TELEGRAM_TOKEN` env wins over the config file.
    pub fn resolved_token(&self) -> Option<String> {
        branded_env("TELEGRAM_TOKEN").or_else(|| self.token.clone())
    }
}


/// Outbound notification channels — one-way HTTP POST sinks. Each URL can also be supplied via env
/// (`AIZEN_DISCORD_WEBHOOK`, `AIZEN_SLACK_WEBHOOK`, `AIZEN_WEBHOOK_URL`), which overrides the stored value.
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
    let lock_path = crate::core::workspace_txn::store_lock("cli_config", "global");
    let _lock = crate::core::repo_lock::RepoTxnLock::acquire_exclusive(
        &lock_path,
        std::time::Duration::from_secs(5),
    )?;
    save_unlocked(cfg, &path)
}

fn save_unlocked(cfg: &CliConfig, path: &std::path::Path) -> Result<()> {
    let mut canonical = cfg.clone();
    canonical.normalize_approval();
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
    let json = serde_json::to_string_pretty(&canonical)?;
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
    fn model_endpoint_registry_routes_endpoint_with_model() {
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-mep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);
        let caller = ResolvedEndpoint {
            base_url: "https://parent/v1".into(),
            api_key: "parent-key".into(),
            model: "parent-model".into(),
        };
        // No registry entry → only the model name changes; endpoint stays the caller's.
        let r = endpoint_for_model("gpt-4o", &caller);
        assert_eq!(r.model, "gpt-4o");
        assert_eq!(r.base_url, "https://parent/v1", "no entry ⇒ caller's endpoint");
        assert_eq!(r.api_key, "parent-key");
        // Register gpt-4o on its own gateway with an env-indirected key.
        save(&CliConfig {
            model_endpoints: Some(vec![ModelEndpoint {
                model: "gpt-4o".into(),
                base_url: Some("https://openai/v1".into()),
                api_key_ref: Some("env:NG_TEST_OAI_KEY".into()),
            }]),
            ..Default::default()
        })
        .unwrap();
        std::env::set_var("NG_TEST_OAI_KEY", "oai-secret");
        let r = endpoint_for_model("gpt-4o", &caller);
        assert_eq!(
            (r.model.as_str(), r.base_url.as_str(), r.api_key.as_str()),
            ("gpt-4o", "https://openai/v1", "oai-secret"),
            "the model carries its own gateway + key"
        );
        // A model with no entry still inherits the caller (registry is per-model, not global).
        assert_eq!(endpoint_for_model("other", &caller).base_url, "https://parent/v1");
        std::env::remove_var("NG_TEST_OAI_KEY");
        // Env override: NG_MODEL_<sanitized-upper>_BASE_URL beats the config entry.
        std::env::set_var("NG_MODEL_GPT_4O_BASE_URL", "https://env-override/v1");
        assert_eq!(endpoint_for_model("gpt-4o", &caller).base_url, "https://env-override/v1");
        std::env::remove_var("NG_MODEL_GPT_4O_BASE_URL");
        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subagent_endpoint_folds_role_default_through_registry() {
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-subep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);
        let main = ResolvedEndpoint {
            base_url: "https://parent/v1".into(),
            api_key: "parent-key".into(),
            model: "parent-model".into(),
        };
        // No config → the sub-agent default is just the parent endpoint.
        let r = subagent_endpoint(&main);
        assert_eq!(
            (r.model.as_str(), r.base_url.as_str()),
            ("parent-model", "https://parent/v1")
        );
        // roles.subagent_default pins ONLY a model; the model-endpoint registry supplies its gateway.
        save(&CliConfig {
            roles: Some(RolesConfig {
                subagent_default: Some(RoleModelConfig {
                    model: Some("cheap-fast".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            model_endpoints: Some(vec![ModelEndpoint {
                model: "cheap-fast".into(),
                base_url: Some("https://cheap/v1".into()),
                api_key_ref: Some("cheap-literal-key".into()),
            }]),
            ..Default::default()
        })
        .unwrap();
        let r = subagent_endpoint(&main);
        assert_eq!(
            (r.model.as_str(), r.base_url.as_str(), r.api_key.as_str()),
            ("cheap-fast", "https://cheap/v1", "cheap-literal-key"),
            "role-default model is routed through the registry to carry its own gateway"
        );
        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn roles_config_has_any_detects_empty() {
        assert!(!RolesConfig::default().has_any());
        assert!(RolesConfig {
            subagent_default: Some(RoleModelConfig {
                model: Some("m".into()),
                ..Default::default()
            }),
            ..Default::default()
        }
        .has_any());
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
    fn response_visuals_defaults_parses_and_round_trips() {
        let legacy: CliConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.response_visuals(), ResponseVisuals::Auto);
        for (raw, mode) in [("auto", ResponseVisuals::Auto), ("always", ResponseVisuals::Always), ("off", ResponseVisuals::Off)] {
            assert_eq!(raw.parse::<ResponseVisuals>().unwrap(), mode);
            let json = serde_json::to_string(&CliConfig { response_visuals: Some(mode), ..Default::default() }).unwrap();
            assert_eq!(serde_json::from_str::<CliConfig>(&json).unwrap().response_visuals(), mode);
        }
        assert!("sometimes".parse::<ResponseVisuals>().is_err());
    }

    #[test]
    fn migrates_legacy_approval_booleans_to_one_enum() {
        let smart: CliConfig = serde_json::from_str(r#"{"smart_approve":true}"#).unwrap();
        assert_eq!(smart.persisted_approval_mode(), ApprovalMode::Smart);

        let yolo: CliConfig = serde_json::from_str(r#"{"auto_approve":true}"#).unwrap();
        assert_eq!(yolo.persisted_approval_mode(), ApprovalMode::Yolo);

        let both: CliConfig =
            serde_json::from_str(r#"{"auto_approve":true,"smart_approve":true}"#).unwrap();
        assert_eq!(both.persisted_approval_mode(), ApprovalMode::Yolo);

        let new_wins: CliConfig = serde_json::from_str(
            r#"{"approval_mode":"ask","auto_approve":true,"smart_approve":true}"#,
        )
        .unwrap();
        assert_eq!(new_wins.persisted_approval_mode(), ApprovalMode::Ask);
    }

    #[test]
    fn save_normalizes_legacy_approval_fields() {
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("ng-approval-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("NEXTGEN_HOME", &dir);

        save(&CliConfig { smart_approve: Some(true), ..Default::default() }).unwrap();
        let raw = std::fs::read_to_string(config_path()).unwrap();
        assert!(raw.contains("\"approval_mode\": \"smart\""), "{raw}");
        assert!(!raw.contains("smart_approve"), "{raw}");
        assert!(!raw.contains("auto_approve"), "{raw}");
        assert_eq!(load().persisted_approval_mode(), ApprovalMode::Smart);

        std::env::remove_var("NEXTGEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn suppress_effort_override_isolates_subagents_then_restores() {
        // Serialize against any other test that touches the process-global override.
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Start clean.
        clear_effort_override();
        assert_eq!(resolved_reasoning_effort(Some("low".into())), Some("low".into()),
            "disarmed → caller's config default is used");

        // Parent turn arms the override (e.g. ultimate pins `max`).
        set_effort_override(Some("max".into()));
        assert_eq!(resolved_reasoning_effort(Some("low".into())), Some("max".into()),
            "armed → parent tier wins over the caller default");

        // A sub-agent dispatch suppresses it: inside the guard, the caller's own default wins again.
        {
            let _s = suppress_effort_override();
            assert_eq!(resolved_reasoning_effort(Some("low".into())), Some("low".into()),
                "suppressed → sub-agent resolves its own cfg.reasoning_effort, not the parent's max");
            assert_eq!(resolved_reasoning_effort(None), None,
                "suppressed with no caller default → omit the field");
        }
        // Guard dropped → the parent's armed override is restored for the rest of the turn.
        assert_eq!(resolved_reasoning_effort(Some("low".into())), Some("max".into()),
            "drop restores the parent tier");

        // The nested "omit" state (Some(None)) must also round-trip through suppression.
        set_effort_override(None); // armed-but-omit
        {
            let _s = suppress_effort_override();
            assert_eq!(resolved_reasoning_effort(Some("high".into())), Some("high".into()));
        }
        assert_eq!(resolved_reasoning_effort(Some("high".into())), None,
            "restored to armed-but-omit (Some(None)), so the field is omitted again");

        clear_effort_override();
    }

    #[test]
    fn mask_hides_the_secret() {
        assert_eq!(mask("short"), "***");
        let m = mask("sk-f49abcdef0123456789");
        assert!(m.starts_with("sk-f49"));
        assert!(!m.contains("0123456789"));
        assert!(m.contains("chars"));
    }

    #[test]
    fn branded_env_reads_aizen_only() {
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // A suffix nothing else in the process reads, so setting these can't disturb other tests.
        let (brand, legacy) = ("AIZEN_BRANDED_UT", "NG_BRANDED_UT");
        for v in [brand, legacy] {
            std::env::remove_var(v);
        }
        // Neither set → None / false.
        assert_eq!(branded_env("BRANDED_UT"), None);
        assert!(!branded_flag("BRANDED_UT"));
        // The pre-rebrand `NG_*` name is NOT read anymore (fully rebranded to Aizen).
        std::env::set_var(legacy, "legacy");
        assert_eq!(branded_env("BRANDED_UT"), None);
        assert!(!branded_flag("BRANDED_UT"));
        // `AIZEN_*` is honored.
        std::env::set_var(brand, "brand");
        assert_eq!(branded_env("BRANDED_UT").as_deref(), Some("brand"));
        assert!(branded_flag("BRANDED_UT"));
        // An empty/whitespace value is ignored.
        std::env::set_var(brand, "   ");
        assert_eq!(branded_env("BRANDED_UT"), None);
        for v in [brand, legacy] {
            std::env::remove_var(v);
        }
    }
}
