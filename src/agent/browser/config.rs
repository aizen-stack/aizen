//! Versioned browser backend/profile configuration.
//!
//! Only profile labels, CDP endpoints, host routes, and ENVIRONMENT VARIABLE NAMES are persisted.
//! Credential values are resolved from the environment at connect time and never serialized,
//! logged, returned by tools, or embedded in their schemas.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

const SCHEMA: u32 = 1;
pub const DEFAULT_PROFILE: &str = "local";
pub const CDP_DEFAULT: &str = "127.0.0.1:9222";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserConfig {
    #[serde(default = "schema_one")]
    pub schema: u32,
    #[serde(default = "default_profile_name")]
    pub default_profile: String,
    #[serde(default)]
    pub profiles: BTreeMap<String, BrowserProfile>,
    /// URL host (exact or `*.example.com`) → profile name. Longest matching suffix wins.
    #[serde(default)]
    pub routes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BrowserProfile {
    #[serde(default = "default_provider")]
    pub provider: String,
    /// CDP host:port or http(s) base URL. Legacy `AIZEN_BROWSER_CDP` overrides the local default.
    pub endpoint: String,
    /// Environment variable whose VALUE is sent as an Authorization header. The value itself is
    /// never accepted in browser.json.
    #[serde(default)]
    pub auth_env: Option<String>,
}

fn schema_one() -> u32 {
    SCHEMA
}
fn default_profile_name() -> String {
    DEFAULT_PROFILE.to_string()
}
fn default_provider() -> String {
    "cdp".to_string()
}

impl Default for BrowserConfig {
    fn default() -> Self {
        let mut profiles = BTreeMap::new();
        profiles.insert(
            DEFAULT_PROFILE.to_string(),
            BrowserProfile {
                provider: default_provider(),
                endpoint: CDP_DEFAULT.to_string(),
                auth_env: None,
            },
        );
        Self {
            schema: SCHEMA,
            default_profile: DEFAULT_PROFILE.to_string(),
            profiles,
            routes: BTreeMap::new(),
        }
    }
}

pub fn config_path() -> PathBuf {
    crate::core::config::nextgen_home().join("browser.json")
}

pub fn load() -> Result<BrowserConfig> {
    let path = config_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BrowserConfig::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    if raw.trim().is_empty() {
        return Ok(BrowserConfig::default());
    }
    let mut cfg: BrowserConfig =
        serde_json::from_str(&raw).with_context(|| format!("parsing {}", path.display()))?;
    if cfg.schema != SCHEMA {
        bail!(
            "unsupported browser.json schema {} (this binary supports {SCHEMA})",
            cfg.schema
        );
    }
    if cfg.profiles.is_empty() {
        cfg.profiles = BrowserConfig::default().profiles;
    }
    validate(&cfg)?;
    Ok(cfg)
}

fn validate(cfg: &BrowserConfig) -> Result<()> {
    if !cfg.profiles.contains_key(&cfg.default_profile) {
        bail!(
            "browser default_profile '{}' is not defined",
            cfg.default_profile
        );
    }
    for (name, profile) in &cfg.profiles {
        if profile.provider != "cdp" {
            bail!(
                "browser profile '{name}' uses unsupported provider '{}' (supported: cdp)",
                profile.provider
            );
        }
        if profile.endpoint.trim().is_empty() {
            bail!("browser profile '{name}' has an empty endpoint");
        }
        if profile.endpoint.starts_with("http://") || profile.endpoint.starts_with("https://") {
            let parsed = url::Url::parse(&profile.endpoint)
                .with_context(|| format!("browser profile '{name}' has an invalid endpoint"))?;
            if !parsed.username().is_empty()
                || parsed.password().is_some()
                || parsed.query().is_some()
                || parsed.fragment().is_some()
            {
                bail!(
                    "browser profile '{name}' endpoint must not contain userinfo, query credentials, or a fragment; use auth_env"
                );
            }
        }
        if let Some(var) = profile.auth_env.as_deref() {
            if var.is_empty() || !var.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                bail!("browser profile '{name}' auth_env must be an environment variable name");
            }
        }
    }
    for (route, profile) in &cfg.routes {
        if route.trim().is_empty() {
            bail!("browser route host cannot be empty");
        }
        if !cfg.profiles.contains_key(profile) {
            bail!("browser route '{route}' references missing profile '{profile}'");
        }
    }
    Ok(())
}

/// Resolve a profile for a URL. Exact route wins, then the longest `*.suffix` match, then default.
pub fn resolve_for_url(cfg: &BrowserConfig, url: &str) -> Result<(String, BrowserProfile)> {
    let host = url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()));
    let mut selected: Option<(&str, &str)> = None;
    if let Some(host) = host.as_deref() {
        for (route, profile) in &cfg.routes {
            let route_l = route.to_ascii_lowercase();
            let matched = if let Some(suffix) = route_l.strip_prefix("*.") {
                host == suffix || host.ends_with(&format!(".{suffix}"))
            } else {
                host == route_l
            };
            if matched && selected.is_none_or(|(prev, _)| route_l.len() > prev.len()) {
                selected = Some((route.as_str(), profile.as_str()));
            }
        }
    }
    let name = selected
        .map(|(_, p)| p)
        .unwrap_or(cfg.default_profile.as_str());
    let mut profile = cfg
        .profiles
        .get(name)
        .cloned()
        .with_context(|| format!("browser profile '{name}' not found"))?;
    // Backward compatibility: the original env knob overrides only the implicit local profile.
    if name == DEFAULT_PROFILE {
        if let Some(endpoint) = crate::core::cli_config::branded_env("BROWSER_CDP") {
            if !endpoint.trim().is_empty() {
                profile.endpoint = endpoint;
            }
        }
    }
    Ok((name.to_string(), profile))
}

pub fn resolve_named(cfg: &BrowserConfig, name: &str) -> Result<BrowserProfile> {
    let mut profile = cfg
        .profiles
        .get(name)
        .cloned()
        .with_context(|| format!("browser profile '{name}' not found"))?;
    if name == DEFAULT_PROFILE {
        if let Some(endpoint) = crate::core::cli_config::branded_env("BROWSER_CDP") {
            if !endpoint.trim().is_empty() {
                profile.endpoint = endpoint;
            }
        }
    }
    Ok(profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_choose_exact_then_longest_wildcard_then_default() {
        let mut cfg = BrowserConfig::default();
        cfg.profiles.insert(
            "work".into(),
            BrowserProfile {
                provider: "cdp".into(),
                endpoint: "127.0.0.1:9333".into(),
                auth_env: None,
            },
        );
        cfg.profiles.insert(
            "docs".into(),
            BrowserProfile {
                provider: "cdp".into(),
                endpoint: "127.0.0.1:9444".into(),
                auth_env: None,
            },
        );
        cfg.routes.insert("*.example.com".into(), "work".into());
        cfg.routes.insert("docs.example.com".into(), "docs".into());
        assert_eq!(
            resolve_for_url(&cfg, "https://docs.example.com/x")
                .unwrap()
                .0,
            "docs"
        );
        assert_eq!(
            resolve_for_url(&cfg, "https://api.example.com/x")
                .unwrap()
                .0,
            "work"
        );
        assert_eq!(
            resolve_for_url(&cfg, "https://other.test/x").unwrap().0,
            DEFAULT_PROFILE
        );
    }

    #[test]
    fn config_rejects_literal_like_auth_field_and_unknown_provider() {
        let raw = r#"{"schema":1,"default_profile":"x","profiles":{"x":{"provider":"playwright","endpoint":"x","auth_env":"TOKEN"}}}"#;
        let cfg: BrowserConfig = serde_json::from_str(raw).unwrap();
        assert!(validate(&cfg)
            .unwrap_err()
            .to_string()
            .contains("unsupported provider"));
        let raw2 = r#"{"schema":1,"default_profile":"x","profiles":{"x":{"provider":"cdp","endpoint":"x","auth_env":"Bearer secret"}}}"#;
        let cfg2: BrowserConfig = serde_json::from_str(raw2).unwrap();
        assert!(
            validate(&cfg2).is_err(),
            "only env names are accepted, never literal credentials"
        );
        let raw3 = r#"{"schema":1,"default_profile":"x","profiles":{"x":{"provider":"cdp","endpoint":"https://user:secret@cdp.example"}}}"#;
        let cfg3: BrowserConfig = serde_json::from_str(raw3).unwrap();
        assert!(
            validate(&cfg3).is_err(),
            "endpoint userinfo must never carry credentials"
        );
    }
}
