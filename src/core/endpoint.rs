//! Resolving which endpoint a call goes to, and the shared HTTP client it goes through.
//!
//! Precedence is explicit flag/env first, then saved config — one place, so the REPL, the one-shot
//! subcommands, cron and the host bot cannot disagree about which provider is live.

use crate::core::cli_config;
use anyhow::{Context, Result};

/// Resolve base URL + API key + model: explicit flag/env (clap) > saved config. Errors name all
/// three ways to provide a missing value.
pub(crate) fn resolve_endpoint(
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
) -> Result<(String, String, String)> {
    let cfg = cli_config::load();
    // Precedence: explicit `--flag` (already folded into the args) > env (`AIZEN_*`) > saved config.
    // Reading env here (not just via clap) means the bare REPL honors it too.
    let base_url = base_url
        .or_else(|| cli_config::branded_env("BASE_URL"))
        .or(cfg.base_url)
        .context("no base URL — run `aizen config` (interactive setup), or pass --base-url / set AIZEN_BASE_URL")?;
    let api_key = api_key
        .or_else(|| cli_config::branded_env("API_KEY"))
        .or(cfg.api_key)
        .context("no API key — run `aizen config` (interactive setup), or pass --api-key / set AIZEN_API_KEY")?;
    // Session pin sits between env and disk: a REPL window stays on the model IT resolved, so a
    // sibling window running `/model` (which rewrites the shared cli-config.json) can't switch this
    // one out from under it on the next turn. Non-REPL callers never pin ⇒ they read `cfg.model`.
    let model = model
        .or_else(|| cli_config::branded_env("MODEL"))
        .or_else(cli_config::session_model)
        .or(cfg.model)
        .context("no model — run `aizen config` (interactive setup) or `aizen models` to list, or pass --model / set AIZEN_MODEL")?;
    Ok((base_url, api_key, model))
}

/// Codex signs requests with OAuth tokens stored out of band, so there is no API key to resolve.
/// `cli_config::Provider::new` already stores this same placeholder for Codex profiles — returning it
/// here keeps a Codex user from being stopped by "no API key" when the key genuinely does not exist.
fn codex_oauth_api_key(base_url: &str) -> Option<String> {
    crate::llm::oauth_codex::is_codex_base_url(base_url).then(|| "codex-oauth".to_string())
}

pub(crate) fn resolve_base_key(
    base_url: Option<String>,
    api_key: Option<String>,
) -> Result<(String, String)> {
    let cfg = cli_config::load();
    let base_url = base_url
        .or_else(|| cli_config::branded_env("BASE_URL"))
        .or(cfg.base_url)
        .context("no base URL — run `aizen config`")?;
    let api_key = api_key
        .or_else(|| cli_config::branded_env("API_KEY"))
        .or(cfg.api_key)
        .or_else(|| codex_oauth_api_key(&base_url))
        .context("no API key — run `aizen config`")?;
    Ok((base_url, api_key))
}

/// Why this client carries NO total-request `timeout`.
///
/// 0.5.2 added `.timeout(1800s)` here as "a backstop under any path nobody has enumerated". That was
/// a bug, and the reason is worth keeping: reqwest's total timeout is applied "from when the request
/// starts connecting until the response body has finished" — a whole-response deadline, not a
/// header-phase one. This very client is what the REPL hands to `stream_chat_with_tools_eager` for
/// every turn, so the ceiling did not merely cap pathological hangs: it cut off a HEALTHY stream that
/// was still emitting tokens, 30 minutes in, losing the entire turn. A deep reasoning run with many
/// tool calls reaches that legitimately.
///
/// The stall protection that a streaming path actually needs is shaped per-event, not per-response,
/// and already exists in two layers: `read_timeout` below (the socket going byte-silent) and
/// `llm::client`'s inter-event watchdog, which re-arms on every SSE event and so distinguishes "the
/// gateway stopped writing" from "the answer is long". A total deadline cannot make that distinction,
/// which is exactly why it is wrong here.
///
/// One-shot clients (health probe, update check, model discovery) DO set a total timeout — nothing
/// they fetch streams, so "the whole response took too long" is a meaningful failure there.
pub(crate) fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(15))
        .read_timeout(std::time::Duration::from_secs(300))
        .tcp_keepalive(std::time::Duration::from_secs(30))
        .build()
        .context("building HTTP client")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_base_uses_oauth_placeholder_without_api_key() {
        assert_eq!(
            codex_oauth_api_key(crate::llm::oauth_codex::CODEX_BASE_URL).as_deref(),
            Some("codex-oauth")
        );
        assert_eq!(codex_oauth_api_key("https://api.openai.com/v1"), None);
    }
}
