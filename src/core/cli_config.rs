//! Persistent CLI endpoint config (`aizen config`) — base URL / API key / default model stored at
//! `~/.aizen/cli-config.json`, so the network commands work without re-passing env vars.
//!
//! Resolution precedence for every network command: explicit `--flag` > `AIZEN_*` env var > this
//! file. (clap merges flag+env into the arg; we fall back to the file when that's still absent.)
//! The key is stored in plaintext (standard for a CLI credential file, like `~/.aws/credentials`)
//! but the file is tightened to owner-only (0600) on Unix at write time — see `save`.

use crate::core::approval::ApprovalMode;
use crate::core::config::aizen_home;
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
        f.write_str(match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Off => "off",
        })
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
    /// Named main-endpoint profiles for manual provider failover. The active profile is copied into
    /// the root `base_url`/`api_key`/`model` fields, so legacy callers keep one resolution path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub providers: Option<Vec<ProviderProfile>>,
    /// Name of the profile whose values currently occupy the root endpoint fields. `None` means the
    /// root endpoint was configured directly rather than selected from the registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_provider: Option<String>,
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
    /// Concurrent sub-agent cap for parallel read-only `task` dispatches and `workflow` fan-out.
    /// `None` ⇒ machine-derived from the core count (band 2..=16); `Some(n)` pins it. Env
    /// `AIZEN_MAX_SUBAGENTS` overrides both. All clamped to a 64 disaster-ceiling — no hard 5 any more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_parallel_subagents: Option<usize>,
    /// Register the `workflow` fan-out tool. `None` ⇒ ON (the default batch-orchestration surface);
    /// `Some(false)` opts out to save its ~350-token schema on every top-level turn.
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
    /// Dense (semantic) retrieval model for memory recall: a directory name under
    /// `~/.aizen/models/`, or an absolute path to a model2vec model dir. `None` ⇒ auto-detect
    /// (see `memory::embed::discover_local_model`). `AIZEN_EMBED_MODEL` overrides this.
    ///
    /// Only meaningful on a `--features dense` build; a default build carries no semantic backend, so
    /// the field is stored but inert there rather than silently changing retrieval.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embed_model: Option<String>,
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
    /// Telegram bot integration (the `aizen serve` daemon + telegram_send/telegram_ask tools).
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
    /// Skill marketplace base URL for `skill_search`/`skill_install` (and `aizen skill search/install`).
    /// `None` ⇒ the default `https://agentskill.sh`. Override to point at a private registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_registry: Option<String>,
    /// Silent background release check at REPL startup (cached 24h; only ever prints a one-line
    /// notice). `None` ⇒ ON. `Some(false)` ⇒ never contacts the release channel. `AIZEN_NO_UPDATE_CHECK`
    /// also disables it without editing this file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub update_check: Option<bool>,
    /// Max time-machine checkpoints retained (oldest auto-pruned past this on each save, never the
    /// active one). `None` ⇒ default 50. `Some(0)` ⇒ unlimited (keep everything).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timemachine_keep: Option<usize>,
    /// How many low-value SAFETY-NET checkpoints (`before agent edits`, one per agent run) to retain,
    /// independent of `timemachine_keep`. Retention drops the oldest safety-nets past this floor before
    /// it ever touches a descriptive `phase: …` milestone, so `aizen time list` stays readable.
    /// `None` ⇒ default 5.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timemachine_keep_safety: Option<usize>,
    /// Maximum files in one Time Machine snapshot. `None` ⇒ 100,000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timemachine_max_files: Option<u64>,
    /// Maximum aggregate Git blob bytes in one snapshot. `None` ⇒ 2 GiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timemachine_max_bytes: Option<u64>,
    /// Maximum size of one file/blob in a snapshot. `None` ⇒ 512 MiB.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timemachine_max_file_bytes: Option<u64>,
    /// Discord two-way bot (the `aizen discord serve` gateway daemon). Distinct from the one-way Discord
    /// webhook under [`NotifyConfig`] — this is a full bot that receives messages and replies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discord: Option<DiscordConfig>,
    /// Reach layer (platform-aware web access) optional keys — everything works keyless; these only
    /// raise limits / unlock extras. See `agent::reach` and `/reach`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reach: Option<ReachConfig>,
    /// OS-level sandbox for model/repo-influenced child processes: mode, network default, roots,
    /// pass-through env, resource limits. `None` ⇒ all defaults (`auto`, network deny). See
    /// `sandbox::policy::SandboxSettings` and `docs/SANDBOX.md`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<crate::sandbox::policy::SandboxSettings>,
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
    /// Provider/model assignments for installed specialist cards. This is the normal UI path; card
    /// endpoint frontmatter remains an advanced legacy fallback for compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_routes: Option<Vec<AgentRoute>>,
}

/// One role's endpoint override. Any subset of fields; the rest inherit the main endpoint.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct RoleModelConfig {
    /// Saved provider profile to use. Its endpoint and default model are inherited before the legacy
    /// per-field overrides below are applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// `env:VAR` (preferred — the key never touches disk) or a literal key (masked in displays).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_ref: Option<String>,
    /// Per-role reasoning effort ("low"/"medium"/"high"/"xhigh"/"max"). Overrides the process-global
    /// fixed tier for this role — `None` means inherit the child's own logic (suppress_effort_override
    /// already prevents the parent's pinned `xhigh`/`max` from leaking into delegated children).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
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

/// One named main endpoint. Profiles are a manual failover surface: activating one copies this full
/// tuple into the root config fields used by every existing caller.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ProviderProfile {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_context_window: Option<usize>,
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
    let profile = rc
        .as_ref()
        .and_then(|r| r.provider.as_deref())
        .and_then(|name| cfg.provider(name))
        .cloned();
    let key_from_ref = |r: &RoleModelConfig| {
        r.api_key_ref
            .as_ref()
            .and_then(|k| match k.strip_prefix("env:") {
                Some(var) => env_nonempty(var),
                None => Some(k.clone()),
            })
    };
    ResolvedEndpoint {
        model: env_nonempty(&format!("AIZEN_{up}_MODEL"))
            .or_else(|| rc.as_ref().and_then(|r| r.model.clone()))
            .or_else(|| profile.as_ref().map(|p| p.model.clone()))
            .unwrap_or_else(|| main.model.clone()),
        base_url: env_nonempty(&format!("AIZEN_{up}_BASE_URL"))
            .or_else(|| rc.as_ref().and_then(|r| r.base_url.clone()))
            .or_else(|| profile.as_ref().map(|p| p.base_url.clone()))
            .unwrap_or_else(|| main.base_url.clone()),
        api_key: env_nonempty(&format!("AIZEN_{up}_API_KEY"))
            .or_else(|| rc.as_ref().and_then(key_from_ref))
            .or_else(|| profile.as_ref().map(|p| p.api_key.clone()))
            .unwrap_or_else(|| main.api_key.clone()),
    }
}

/// A specialist's normal provider assignment. `model=None` means use the selected provider's default
/// model; `provider=None` means inherit the sub-agent default role.
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentRoute {
    pub agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
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
/// `AIZEN_MODEL_<UPPER>_BASE_URL` / `AIZEN_MODEL_<UPPER>_API_KEY` (the model id uppercased, non-alnum →
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
    // Env key: AIZEN_MODEL_<sanitized-upper>_{BASE_URL,API_KEY}. Sanitize so `gpt-4o` → `GPT_4O`.
    let up: String = model
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    let key_from_ref = |e: &ModelEndpoint| {
        e.api_key_ref
            .as_ref()
            .and_then(|k| match k.strip_prefix("env:") {
                Some(var) => env_nonempty(var),
                None => Some(k.clone()),
            })
    };
    ResolvedEndpoint {
        model: model.to_string(),
        base_url: env_nonempty(&format!("AIZEN_MODEL_{up}_BASE_URL"))
            .or_else(|| entry.as_ref().and_then(|e| e.base_url.clone()))
            .unwrap_or_else(|| caller.base_url.clone()),
        api_key: env_nonempty(&format!("AIZEN_MODEL_{up}_API_KEY"))
            .or_else(|| entry.as_ref().and_then(key_from_ref))
            .unwrap_or_else(|| caller.api_key.clone()),
    }
}

/// The sub-agent's default endpoint: `roles.subagent_default` (env `AIZEN_SUBAGENT_DEFAULT_*` >
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

/// Resolve the local provider/model assignment for one specialist from one config snapshot.
pub fn resolve_agent_route(cfg: &CliConfig, agent: &str) -> Option<(AgentRoute, ResolvedEndpoint)> {
    let route = cfg.agent_route(agent)?.clone();
    let provider = route.provider.as_deref()?;
    let profile = cfg.provider(provider)?;
    let endpoint = ResolvedEndpoint {
        base_url: profile.base_url.clone(),
        api_key: profile.api_key.clone(),
        model: route.model.clone().unwrap_or_else(|| profile.model.clone()),
    };
    Some((route, endpoint))
}

/// Resolve the local provider/model assignment for one specialist from persisted config.
pub fn agent_route(agent: &str) -> Option<(AgentRoute, ResolvedEndpoint)> {
    let cfg = load();
    resolve_agent_route(&cfg, agent)
}

/// Resolve whether editing runs should spend the one-shot self-review turn.
/// An explicit value always wins; otherwise a configured oracle role opts in because it supplies the
/// reviewer endpoint. No oracle role keeps the feature off by default.
pub fn self_review_enabled(cfg: &CliConfig) -> bool {
    cfg.self_review.unwrap_or_else(|| role_configured("oracle"))
}

/// Whether the REPL may run its silent, cached release check at startup.
/// The env kill-switch wins so a locked-down environment needs no config edit; otherwise an explicit
/// config value wins, and the unset default is on.
pub fn update_check_enabled(cfg: &CliConfig) -> bool {
    if matches!(
        std::env::var("AIZEN_NO_UPDATE_CHECK")
            .ok()
            .as_deref()
            .map(str::trim),
        Some("1") | Some("true") | Some("yes") | Some("on")
    ) {
        return false;
    }
    cfg.update_check.unwrap_or(true)
}

/// Is any override configured for `role` (config or env)? Consumers that should stay OFF without
/// an explicit oracle (e.g. self-review's oracle mode) check this instead of comparing endpoints.
pub fn role_configured(role: &str) -> bool {
    let up = role.to_ascii_uppercase();
    if env_nonempty(&format!("AIZEN_{up}_MODEL")).is_some() {
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
        .is_some_and(|r| {
            r.provider.is_some()
                || r.model.is_some()
                || r.base_url.is_some()
                || r.api_key_ref.is_some()
        })
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
        branded_env("TAVILY_API_KEY")
            .or_else(|| env_nonempty("TAVILY_API_KEY"))
            .or_else(|| self.tavily_api_key.clone())
    }
    /// Effective Jina key: `AIZEN_JINA_API_KEY` > `JINA_API_KEY` > config file.
    pub fn resolved_jina_key(&self) -> Option<String> {
        branded_env("JINA_API_KEY")
            .or_else(|| env_nonempty("JINA_API_KEY"))
            .or_else(|| self.jina_api_key.clone())
    }
    /// Effective GitHub token: `AIZEN_GITHUB_TOKEN` > the conventional `GITHUB_TOKEN` > config file.
    pub fn resolved_github_token(&self) -> Option<String> {
        branded_env("GITHUB_TOKEN")
            .or_else(|| env_nonempty("GITHUB_TOKEN"))
            .or_else(|| self.github_token.clone())
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

impl ProviderProfile {
    pub fn normalized(name: &str, base_url: &str, api_key: &str, model: &str) -> Result<Self> {
        let name = name.trim();
        if name.is_empty() {
            anyhow::bail!("provider name must not be empty");
        }
        if name.chars().any(char::is_whitespace) {
            anyhow::bail!("provider name must not contain whitespace");
        }
        let base_url = base_url.trim().trim_end_matches('/');
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            anyhow::bail!("provider base URL must start with http:// or https://");
        }
        let api_key = api_key.trim();
        // Codex OAuth profiles store tokens out-of-band; api_key may be a placeholder.
        let codex = crate::llm::oauth_codex::is_codex_base_url(base_url);
        if api_key.is_empty() && !codex {
            anyhow::bail!("provider API key must not be empty");
        }
        let api_key = if api_key.is_empty() && codex {
            "codex-oauth"
        } else {
            api_key
        };
        let model = model.trim();
        if model.is_empty() {
            anyhow::bail!("provider model must not be empty");
        }
        Ok(Self {
            name: name.to_string(),
            base_url: base_url.to_string(),
            api_key: api_key.to_string(),
            model: model.to_string(),
            model_context_window: None,
        })
    }

    pub fn matches_name(&self, name: &str) -> bool {
        self.name.eq_ignore_ascii_case(name.trim())
    }
}

impl CliConfig {
    /// Find a named main-endpoint profile case-insensitively.
    pub fn provider(&self, name: &str) -> Option<&ProviderProfile> {
        self.providers
            .as_ref()
            .and_then(|list| list.iter().find(|p| p.matches_name(name)))
    }

    /// Add or replace a named main-endpoint profile. Names are case-insensitively unique.
    pub fn upsert_provider(&mut self, profile: ProviderProfile) -> Result<()> {
        let list = self.providers.get_or_insert_with(Vec::new);
        if let Some(existing) = list.iter_mut().find(|p| p.matches_name(&profile.name)) {
            if !existing.name.eq(&profile.name) {
                anyhow::bail!("provider name already exists: {}", existing.name);
            }
            *existing = profile;
        } else {
            list.push(profile);
        }
        Ok(())
    }

    /// Activate a profile by copying its complete endpoint tuple into the legacy root fields.
    pub fn activate_provider(&mut self, name: &str) -> Result<()> {
        let profile = self
            .provider(name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown provider profile: {name}"))?;
        self.base_url = Some(profile.base_url);
        self.api_key = Some(profile.api_key);
        self.model = Some(profile.model);
        self.model_context_window = profile.model_context_window;
        self.active_provider = Some(profile.name);
        Ok(())
    }

    /// Remove a profile. The currently active root endpoint remains live when its profile is removed.
    pub fn remove_provider(&mut self, name: &str) -> Result<ProviderProfile> {
        let list = self
            .providers
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("unknown provider profile: {name}"))?;
        let index = list
            .iter()
            .position(|p| p.matches_name(name))
            .ok_or_else(|| anyhow::anyhow!("unknown provider profile: {name}"))?;
        let removed = list.remove(index);
        if self
            .active_provider
            .as_deref()
            .is_some_and(|active| active.eq_ignore_ascii_case(&removed.name))
        {
            self.active_provider = None;
        }
        if list.is_empty() {
            self.providers = None;
        }
        Ok(removed)
    }

    /// Keep the active profile's stored values aligned after direct config edits.
    pub fn sync_active_provider(&mut self) {
        let Some(active) = self.active_provider.clone() else {
            return;
        };
        let Some(profile) = self
            .providers
            .as_mut()
            .and_then(|list| list.iter_mut().find(|p| p.matches_name(&active)))
        else {
            self.active_provider = None;
            return;
        };
        profile.base_url = self.base_url.clone().unwrap_or_default();
        profile.api_key = self.api_key.clone().unwrap_or_default();
        profile.model = self.model.clone().unwrap_or_default();
        profile.model_context_window = self.model_context_window;
    }

    /// A direct endpoint edit no longer belongs to the selected profile once its URL changes.
    pub fn detach_provider_if_url_changed(&mut self, previous_url: Option<&str>) {
        let current = self
            .base_url
            .as_deref()
            .map(|url| url.trim_end_matches('/'));
        let previous = previous_url.map(|url| url.trim_end_matches('/'));
        if current != previous {
            self.active_provider = None;
        }
    }

    /// Return the named route for a specialist, case-insensitively by slug.
    pub fn agent_route(&self, agent: &str) -> Option<&AgentRoute> {
        self.agent_routes
            .as_ref()?
            .iter()
            .find(|r| r.agent.eq_ignore_ascii_case(agent))
    }

    /// Upsert a specialist route. The empty route removes the entry.
    pub fn set_agent_route(
        &mut self,
        agent: &str,
        provider: Option<String>,
        model: Option<String>,
    ) -> Result<()> {
        let agent = agent.trim();
        if agent.is_empty() {
            anyhow::bail!("agent route name must not be empty");
        }
        if let Some(ref name) = provider {
            if self.provider(name).is_none() {
                anyhow::bail!("unknown provider profile: {name}");
            }
        }
        let list = self.agent_routes.get_or_insert_with(Vec::new);
        list.retain(|r| !r.agent.eq_ignore_ascii_case(agent));
        if provider.is_some() || model.is_some() {
            list.push(AgentRoute {
                agent: agent.to_string(),
                provider,
                model,
            });
        }
        if list.is_empty() {
            self.agent_routes = None;
        }
        Ok(())
    }

    /// Rename a provider and all role/specialist references in one transaction.
    pub fn rename_provider(&mut self, old: &str, new: &str) -> Result<()> {
        let new = new.trim();
        if new.is_empty() || new.chars().any(char::is_whitespace) {
            anyhow::bail!("provider name must not be empty or contain whitespace");
        }
        let old_profile = self
            .provider(old)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown provider profile: {old}"))?;
        if !old_profile.matches_name(new) && self.provider(new).is_some() {
            anyhow::bail!("provider name already exists: {new}");
        }
        if let Some(list) = self.providers.as_mut() {
            if let Some(p) = list.iter_mut().find(|p| p.matches_name(old)) {
                p.name = new.to_string();
            }
        }
        if self
            .active_provider
            .as_deref()
            .is_some_and(|n| n.eq_ignore_ascii_case(old))
        {
            self.active_provider = Some(new.to_string());
        }
        if let Some(roles) = self.roles.as_mut() {
            for slot in [
                &mut roles.summarizer,
                &mut roles.subagent_default,
                &mut roles.oracle,
                &mut roles.apply,
            ] {
                if slot
                    .as_ref()
                    .and_then(|r| r.provider.as_deref())
                    .is_some_and(|n| n.eq_ignore_ascii_case(old))
                {
                    if let Some(r) = slot.as_mut() {
                        r.provider = Some(new.to_string());
                    }
                }
            }
        }
        if let Some(routes) = self.agent_routes.as_mut() {
            for route in routes {
                if route
                    .provider
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(old))
                {
                    route.provider = Some(new.to_string());
                }
            }
        }
        Ok(())
    }

    /// Profiles referenced by roles or specialist routes, used to explain safe deletion.
    pub fn provider_references(&self, name: &str) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(roles) = &self.roles {
            for (label, slot) in [
                ("subagent_default", &roles.subagent_default),
                ("summarizer", &roles.summarizer),
                ("oracle", &roles.oracle),
                ("apply", &roles.apply),
            ] {
                if slot
                    .as_ref()
                    .and_then(|r| r.provider.as_deref())
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
                {
                    out.push(format!("role:{label}"));
                }
            }
        }
        if let Some(routes) = &self.agent_routes {
            for route in routes {
                if route
                    .provider
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
                {
                    out.push(format!("agent:{}", route.agent));
                }
            }
        }
        out
    }

    /// Replace every role/specialist reference to one provider with another saved provider.
    pub fn replace_provider_references(&mut self, old: &str, new: &str) -> Result<()> {
        if self.provider(new).is_none() {
            anyhow::bail!("unknown replacement provider profile: {new}");
        }
        if let Some(roles) = self.roles.as_mut() {
            for slot in [
                &mut roles.summarizer,
                &mut roles.subagent_default,
                &mut roles.oracle,
                &mut roles.apply,
            ] {
                if slot
                    .as_ref()
                    .and_then(|r| r.provider.as_deref())
                    .is_some_and(|n| n.eq_ignore_ascii_case(old))
                {
                    if let Some(r) = slot.as_mut() {
                        r.provider = Some(new.to_string());
                    }
                }
            }
        }
        if let Some(routes) = self.agent_routes.as_mut() {
            for route in routes {
                if route
                    .provider
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(old))
                {
                    route.provider = Some(new.to_string());
                }
            }
        }
        Ok(())
    }

    /// Clear references to a profile; the active root endpoint remains unchanged.
    pub fn clear_provider_references(&mut self, name: &str) {
        if let Some(roles) = self.roles.as_mut() {
            for slot in [
                &mut roles.summarizer,
                &mut roles.subagent_default,
                &mut roles.oracle,
                &mut roles.apply,
            ] {
                if slot
                    .as_ref()
                    .and_then(|r| r.provider.as_deref())
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
                {
                    if let Some(r) = slot.as_mut() {
                        r.provider = None;
                    }
                }
            }
        }
        if let Some(routes) = self.agent_routes.as_mut() {
            for route in routes {
                if route
                    .provider
                    .as_deref()
                    .is_some_and(|n| n.eq_ignore_ascii_case(name))
                {
                    route.provider = None;
                }
            }
        }
    }

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

/// The model THIS process is pinned to, once its REPL has resolved one.
///
/// Why this exists: `cli-config.json` is a single file shared by every aizen window on the machine,
/// and the REPL re-reads it on EVERY turn (`resolve_endpoint`). So a second window running `/model`
/// wrote the new id to that shared file and the first window silently adopted it on its next turn —
/// the user picked a model in window B and watched window A switch too. `model_label` was already
/// per-process, but disk won each turn, so the in-memory value was pointless.
///
/// The pin makes the model a SESSION decision: window A keeps what it started with (or last chose
/// itself), regardless of what other windows persist. `/model` still saves to disk — that is what
/// makes the choice the default for the NEXT window — it just no longer reaches back into running
/// ones.
///
/// `None` ⇒ unpinned: fall through to `cfg.model`. That is deliberately the state for every
/// non-REPL caller (one-shot `aizen -p`, cron jobs, the hostbot daemon, sub-agents), which have no
/// session to pin and must keep reading the saved config.
static SESSION_MODEL: Lazy<RwLock<Option<String>>> = Lazy::new(|| RwLock::new(None));

/// Pin this process to `model` (called by the REPL at startup and after any in-session switch).
pub fn pin_session_model(model: &str) {
    let model = model.trim();
    if model.is_empty() {
        return;
    }
    *SESSION_MODEL.write().unwrap_or_else(|e| e.into_inner()) = Some(model.to_string());
}

/// The pinned session model, if this process is a REPL that has resolved one.
pub fn session_model() -> Option<String> {
    SESSION_MODEL
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

/// Drop the pin. Test-only: at runtime a session never un-pins — it either keeps its model or
/// re-pins to a new one — so exposing this outside tests would only invite an accidental un-pin.
#[cfg(test)]
pub fn clear_session_model() {
    *SESSION_MODEL.write().unwrap_or_else(|e| e.into_inner()) = None;
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
    EFFORT_OVERRIDE
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
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
        return matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        );
    }
    load().auto_effort.unwrap_or(true)
}

/// Is "ultimate mode" ON? `AIZEN_ULTIMATE` env wins (`1`/`true`/`on`/`yes` ⇒ on); otherwise the
/// `ultimate` config field, defaulting to OFF. Mirrors `auto_effort_enabled` (env-forced, else config).
pub fn ultimate_enabled() -> bool {
    if let Ok(v) = std::env::var("AIZEN_ULTIMATE") {
        return matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        );
    }
    load().ultimate.unwrap_or(false)
}

/// Is adaptive difficulty→effort routing ON (P3)? `AIZEN_ADAPTIVE_EFFORT` env wins; otherwise the
/// `adaptive_effort` config field, defaulting to OFF (so the heuristic caps at `high` by default).
pub fn adaptive_effort_enabled() -> bool {
    if let Ok(v) = std::env::var("AIZEN_ADAPTIVE_EFFORT") {
        return matches!(
            v.trim().to_ascii_lowercase().as_str(),
            "1" | "true" | "on" | "yes"
        );
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
    aizen_home().join("cli-config.json")
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
                // Through the TUI funnel, never a raw print: `load()` runs on per-turn paths (prompt
                // tier, toolsets, reach routing), so a raw `eprintln!` here would land inside the
                // retained frame and corrupt it. `note_line` degrades to stderr outside the REPL.
                crate::ui::tui::note_line(&format!(
                    "warning: {} is corrupt ({e}); using defaults. A .bak copy is kept before any \
                     re-save — run `aizen config` to repair.",
                    path.display()
                ));
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
    fn self_review_resolution_honors_explicit_values_and_oracle_default() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-self-review-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("AIZEN_HOME", &dir);

        let plain = CliConfig::default();
        assert!(
            !self_review_enabled(&plain),
            "no oracle keeps the default off"
        );

        save(&CliConfig {
            roles: Some(RolesConfig {
                oracle: Some(RoleModelConfig {
                    model: Some("reviewer".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
        assert!(
            self_review_enabled(&CliConfig::default()),
            "a configured oracle implies review"
        );
        assert!(!self_review_enabled(&CliConfig {
            self_review: Some(false),
            ..Default::default()
        }));
        assert!(self_review_enabled(&CliConfig {
            self_review: Some(true),
            ..Default::default()
        }));

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn provider_references_rename_and_specialist_routes_are_atomic() {
        let mut cfg = CliConfig::default();
        cfg.upsert_provider(
            ProviderProfile::normalized("fast", "https://fast/v1", "fast-key", "fast-model")
                .unwrap(),
        )
        .unwrap();
        cfg.roles = Some(RolesConfig {
            subagent_default: Some(RoleModelConfig {
                provider: Some("fast".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        cfg.set_agent_route("reviewer", Some("fast".into()), Some("review-model".into()))
            .unwrap();
        assert_eq!(
            cfg.provider_references("FAST"),
            vec!["role:subagent_default", "agent:reviewer"]
        );
        let resolved = resolve_agent_route(&cfg, "reviewer").unwrap().1;
        assert_eq!(
            (
                resolved.base_url.as_str(),
                resolved.api_key.as_str(),
                resolved.model.as_str()
            ),
            ("https://fast/v1", "fast-key", "review-model")
        );
        cfg.rename_provider("fast", "backup").unwrap();
        assert!(cfg.provider("backup").is_some());
        assert_eq!(
            cfg.roles
                .as_ref()
                .unwrap()
                .subagent_default
                .as_ref()
                .unwrap()
                .provider
                .as_deref(),
            Some("backup")
        );
        assert_eq!(
            cfg.agent_route("reviewer").unwrap().provider.as_deref(),
            Some("backup")
        );
        cfg.clear_provider_references("backup");
        assert!(cfg.provider_references("backup").is_empty());
    }

    #[test]
    fn role_provider_uses_profile_and_model_override() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-roles-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("AIZEN_HOME", &dir);
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
        // Provider selection supplies the endpoint/default model; a role model overrides only model.
        save(&CliConfig {
            providers: Some(vec![ProviderProfile::normalized(
                "cheap-provider",
                "https://cheap/v1",
                "cheap-key",
                "provider-model",
            )
            .unwrap()]),
            roles: Some(RolesConfig {
                summarizer: Some(RoleModelConfig {
                    provider: Some("cheap-provider".into()),
                    model: Some("cheap".into()),
                    ..Default::default()
                }),
                oracle: Some(RoleModelConfig {
                    api_key_ref: Some("env:AIZEN_TEST_ORACLE_KEY".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
        .unwrap();
        let r = resolve_role("summarizer", &main);
        assert_eq!(r.model, "cheap");
        assert_eq!(r.base_url, "https://cheap/v1");
        assert_eq!(r.api_key, "cheap-key");
        assert!(role_configured("summarizer"));
        assert!(
            role_configured("oracle"),
            "an api_key_ref alone counts as configured"
        );
        std::env::set_var("AIZEN_TEST_ORACLE_KEY", "ok-secret");
        assert_eq!(
            resolve_role("oracle", &main).api_key,
            "ok-secret",
            "env: indirection resolves"
        );
        std::env::remove_var("AIZEN_TEST_ORACLE_KEY");
        // Env beats config.
        std::env::set_var("AIZEN_SUMMARIZER_MODEL", "env-model");
        assert_eq!(resolve_role("summarizer", &main).model, "env-model");
        std::env::remove_var("AIZEN_SUMMARIZER_MODEL");
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn model_endpoint_registry_routes_endpoint_with_model() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-mep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("AIZEN_HOME", &dir);
        let caller = ResolvedEndpoint {
            base_url: "https://parent/v1".into(),
            api_key: "parent-key".into(),
            model: "parent-model".into(),
        };
        // No registry entry → only the model name changes; endpoint stays the caller's.
        let r = endpoint_for_model("gpt-4o", &caller);
        assert_eq!(r.model, "gpt-4o");
        assert_eq!(
            r.base_url, "https://parent/v1",
            "no entry ⇒ caller's endpoint"
        );
        assert_eq!(r.api_key, "parent-key");
        // Register gpt-4o on its own gateway with an env-indirected key.
        save(&CliConfig {
            model_endpoints: Some(vec![ModelEndpoint {
                model: "gpt-4o".into(),
                base_url: Some("https://openai/v1".into()),
                api_key_ref: Some("env:AIZEN_TEST_OAI_KEY".into()),
            }]),
            ..Default::default()
        })
        .unwrap();
        std::env::set_var("AIZEN_TEST_OAI_KEY", "oai-secret");
        let r = endpoint_for_model("gpt-4o", &caller);
        assert_eq!(
            (r.model.as_str(), r.base_url.as_str(), r.api_key.as_str()),
            ("gpt-4o", "https://openai/v1", "oai-secret"),
            "the model carries its own gateway + key"
        );
        // A model with no entry still inherits the caller (registry is per-model, not global).
        assert_eq!(
            endpoint_for_model("other", &caller).base_url,
            "https://parent/v1"
        );
        std::env::remove_var("AIZEN_TEST_OAI_KEY");
        // Env override: AIZEN_MODEL_<sanitized-upper>_BASE_URL beats the config entry.
        std::env::set_var("AIZEN_MODEL_GPT_4O_BASE_URL", "https://env-override/v1");
        assert_eq!(
            endpoint_for_model("gpt-4o", &caller).base_url,
            "https://env-override/v1"
        );
        std::env::remove_var("AIZEN_MODEL_GPT_4O_BASE_URL");
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subagent_endpoint_folds_role_default_through_registry() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-subep-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("AIZEN_HOME", &dir);
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
        std::env::remove_var("AIZEN_HOME");
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
    fn provider_profiles_activate_sync_remove_and_load_legacy_json() {
        let mut cfg: CliConfig = serde_json::from_str(
            r#"{"base_url":"https://legacy/v1","api_key":"legacy-key","model":"legacy-model"}"#,
        )
        .unwrap();
        assert!(cfg.providers.is_none());
        assert!(cfg.active_provider.is_none());

        let mut primary = ProviderProfile::normalized(
            "primary",
            "https://primary/v1/",
            "primary-secret",
            "model-a",
        )
        .unwrap();
        primary.model_context_window = Some(200_000);
        cfg.upsert_provider(primary.clone()).unwrap();
        cfg.upsert_provider(
            ProviderProfile::normalized("backup", "https://backup/v1", "backup-secret", "model-b")
                .unwrap(),
        )
        .unwrap();
        assert!(cfg.provider("PRIMARY").is_some());
        assert!(ProviderProfile::normalized("bad name", "https://x/v1", "k", "m").is_err());

        cfg.activate_provider("PRIMARY").unwrap();
        assert_eq!(cfg.active_provider.as_deref(), Some("primary"));
        assert_eq!(cfg.base_url.as_deref(), Some("https://primary/v1"));
        assert_eq!(cfg.api_key.as_deref(), Some("primary-secret"));
        assert_eq!(cfg.model.as_deref(), Some("model-a"));
        assert_eq!(cfg.model_context_window, Some(200_000));

        cfg.api_key = Some("rotated-secret".into());
        cfg.model = Some("model-a2".into());
        cfg.sync_active_provider();
        let primary = cfg.provider("primary").unwrap();
        assert_eq!(primary.api_key, "rotated-secret");
        assert_eq!(primary.model, "model-a2");

        cfg.remove_provider("primary").unwrap();
        assert_eq!(cfg.active_provider, None);
        assert_eq!(cfg.base_url.as_deref(), Some("https://primary/v1"));
        assert_eq!(cfg.api_key.as_deref(), Some("rotated-secret"));
    }

    #[test]
    fn provider_profile_serialization_keeps_key_but_mask_never_reveals_it() {
        let profile = ProviderProfile::normalized(
            "backup",
            "https://backup/v1",
            "sk-backup-super-secret",
            "model-b",
        )
        .unwrap();
        let json = serde_json::to_string(&profile).unwrap();
        assert!(
            json.contains("sk-backup-super-secret"),
            "profiles persist credentials"
        );
        let shown = mask(&profile.api_key);
        assert!(!shown.contains("super-secret"));
        assert!(shown.contains("chars"));
    }

    #[test]
    fn round_trips_through_disk() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("AIZEN_HOME", &dir);

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

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn response_visuals_defaults_parses_and_round_trips() {
        let legacy: CliConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(legacy.response_visuals(), ResponseVisuals::Auto);
        for (raw, mode) in [
            ("auto", ResponseVisuals::Auto),
            ("always", ResponseVisuals::Always),
            ("off", ResponseVisuals::Off),
        ] {
            assert_eq!(raw.parse::<ResponseVisuals>().unwrap(), mode);
            let json = serde_json::to_string(&CliConfig {
                response_visuals: Some(mode),
                ..Default::default()
            })
            .unwrap();
            assert_eq!(
                serde_json::from_str::<CliConfig>(&json)
                    .unwrap()
                    .response_visuals(),
                mode
            );
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
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-approval-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::env::set_var("AIZEN_HOME", &dir);

        save(&CliConfig {
            smart_approve: Some(true),
            ..Default::default()
        })
        .unwrap();
        let raw = std::fs::read_to_string(config_path()).unwrap();
        assert!(raw.contains("\"approval_mode\": \"smart\""), "{raw}");
        assert!(!raw.contains("smart_approve"), "{raw}");
        assert!(!raw.contains("auto_approve"), "{raw}");
        assert_eq!(load().persisted_approval_mode(), ApprovalMode::Smart);

        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn session_model_pin_survives_a_sibling_window_rewriting_the_config() {
        // Serialize against other tests touching process-global state / $AIZEN_HOME.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        clear_session_model();
        assert_eq!(
            session_model(),
            None,
            "unpinned by default (non-REPL callers)"
        );

        // This window launched on model A and pinned it.
        pin_session_model("model-a");
        assert_eq!(session_model(), Some("model-a".into()));

        // A second window runs `/model` and picks B — that only rewrites the SHARED config file.
        // The pin is what keeps this window on A, which is the whole bug.
        assert_eq!(
            session_model(),
            Some("model-a".into()),
            "a sibling window's save must not retarget this session"
        );

        // An in-session switch here (this window's own `/model`) re-pins.
        pin_session_model("model-c");
        assert_eq!(session_model(), Some("model-c".into()));

        // Blank ids are ignored — a window with no model configured must stay unpinned so it can
        // still adopt whatever `/config` sets up.
        pin_session_model("   ");
        assert_eq!(
            session_model(),
            Some("model-c".into()),
            "blank pin is a no-op"
        );

        clear_session_model();
        assert_eq!(session_model(), None);
    }

    #[test]
    fn suppress_effort_override_isolates_subagents_then_restores() {
        // Serialize against any other test that touches the process-global override.
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Start clean.
        clear_effort_override();
        assert_eq!(
            resolved_reasoning_effort(Some("low".into())),
            Some("low".into()),
            "disarmed → caller's config default is used"
        );

        // Parent turn arms the override (e.g. ultimate pins `max`).
        set_effort_override(Some("max".into()));
        assert_eq!(
            resolved_reasoning_effort(Some("low".into())),
            Some("max".into()),
            "armed → parent tier wins over the caller default"
        );

        // A sub-agent dispatch suppresses it: inside the guard, the caller's own default wins again.
        {
            let _s = suppress_effort_override();
            assert_eq!(resolved_reasoning_effort(Some("low".into())), Some("low".into()),
                "suppressed → sub-agent resolves its own cfg.reasoning_effort, not the parent's max");
            assert_eq!(
                resolved_reasoning_effort(None),
                None,
                "suppressed with no caller default → omit the field"
            );
        }
        // Guard dropped → the parent's armed override is restored for the rest of the turn.
        assert_eq!(
            resolved_reasoning_effort(Some("low".into())),
            Some("max".into()),
            "drop restores the parent tier"
        );

        // The nested "omit" state (Some(None)) must also round-trip through suppression.
        set_effort_override(None); // armed-but-omit
        {
            let _s = suppress_effort_override();
            assert_eq!(
                resolved_reasoning_effort(Some("high".into())),
                Some("high".into())
            );
        }
        assert_eq!(
            resolved_reasoning_effort(Some("high".into())),
            None,
            "restored to armed-but-omit (Some(None)), so the field is omitted again"
        );

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
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
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
