//! agentskill.sh skill registry client — search + install reusable skills from the public
//! marketplace (274k+ skills). Pure HTTP/JSON over reqwest (NO npx/Node). Search hits
//! `GET {base}/api/skills?q=…`; the API returns each skill's GitHub coordinates, and the body is
//! fetched from `raw.githubusercontent.com`. The base defaults to `https://agentskill.sh` and is
//! overridable via `cli-config.json` `skill_registry`.
//!
//! Installed skills are THIRD-PARTY instructions the agent will then follow → `skill_install` is
//! `is_destructive() = true` (approval-gated) and search surfaces the registry's `securityScore`.

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::tools::Tool;

/// Default marketplace (the owner's instruction: "ng mặc định lên đây kiếm").
pub const DEFAULT_REGISTRY: &str = "https://agentskill.sh";
/// Default search result cap.
const DEFAULT_LIMIT: usize = 20;

/// One search hit from `/api/skills` (the subset of fields we use).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RegistrySkill {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub owner: String,
    #[serde(default)]
    pub slug: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub content_quality_score: Option<u32>,
    #[serde(default)]
    pub security_score: Option<u32>,
    #[serde(default)]
    pub github_owner: String,
    #[serde(default)]
    pub github_repo: String,
    #[serde(default)]
    pub github_branch: String,
    #[serde(default)]
    pub github_sha: String,
    #[serde(default)]
    pub github_path: String,
}

#[derive(Debug, Default, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    data: Vec<RegistrySkill>,
    #[serde(default)]
    total: usize,
}

impl RegistrySkill {
    /// `owner/name` identifier (prefers the API `slug`).
    pub fn id(&self) -> String {
        if !self.slug.is_empty() {
            self.slug.clone()
        } else if !self.owner.is_empty() {
            format!("{}/{}", self.owner, self.name)
        } else {
            self.name.clone()
        }
    }

    /// The GitHub raw URL for the skill markdown — pinned to the immutable SHA when present (so a
    /// later upstream edit can't change what we installed), else the branch, else `main`.
    pub fn raw_url(&self) -> Option<String> {
        if self.github_owner.is_empty() || self.github_repo.is_empty() || self.github_path.is_empty() {
            return None;
        }
        let reff = if !self.github_sha.is_empty() {
            self.github_sha.as_str()
        } else if !self.github_branch.is_empty() {
            self.github_branch.as_str()
        } else {
            "main"
        };
        let path = self.github_path.trim_start_matches('/');
        Some(format!("https://raw.githubusercontent.com/{}/{}/{reff}/{path}", self.github_owner, self.github_repo))
    }

    /// One-line summary for the search UI / tool result.
    pub fn summary_line(&self) -> String {
        let q = self.content_quality_score.map(|s| format!(" · quality {s}")).unwrap_or_default();
        let s = self.security_score.map(|s| format!(" · security {s}")).unwrap_or_default();
        let desc: String = self.description.chars().take(90).collect();
        format!("{}{q}{s} — {desc}", self.id())
    }
}

/// The configured registry base (default `agentskill.sh`).
pub fn registry_base() -> String {
    crate::cli_config::load()
        .skill_registry
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_string())
}

fn http() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("aizen/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .context("building registry HTTP client")
}

/// Search the registry by keyword. `limit` caps results.
pub async fn search(query: &str, limit: usize) -> Result<Vec<RegistrySkill>> {
    let base = registry_base();
    let url = format!("{}/api/skills", base.trim_end_matches('/'));
    let client = http()?;
    let resp = client
        .get(&url)
        .query(&[("q", query), ("limit", &limit.to_string())])
        .send()
        .await
        .with_context(|| format!("searching {base}"))?;
    let status = resp.status();
    if !status.is_success() {
        bail!("registry {base} returned HTTP {}", status.as_u16());
    }
    let parsed: SearchResponse = resp.json().await.context("parsing registry JSON")?;
    let _ = parsed.total;
    Ok(parsed.data)
}

/// Fetch a registry skill's markdown body from its GitHub raw URL.
pub async fn fetch_body(sk: &RegistrySkill) -> Result<String> {
    let url = sk.raw_url().context("this skill has no GitHub source location")?;
    let client = http()?;
    let resp = client.get(&url).send().await.with_context(|| format!("GET {url}"))?;
    if !resp.status().is_success() {
        bail!("GitHub returned HTTP {} for {url}", resp.status().as_u16());
    }
    resp.text().await.context("reading skill body")
}

/// Search → pick the exact-slug match (else the first hit) → fetch + save locally. Returns the
/// saved skill. Used by both the `ng skill install` CLI and the `skill_install` agent tool.
pub async fn install(query: &str) -> Result<crate::skill::Skill> {
    let hits = search(query, DEFAULT_LIMIT).await?;
    if hits.is_empty() {
        bail!("no skill on {} matches '{query}'", registry_base());
    }
    let want = query.trim().to_lowercase();
    let chosen = hits
        .iter()
        .find(|s| s.id().to_lowercase() == want || s.name.to_lowercase() == want)
        .unwrap_or(&hits[0])
        .clone();
    let body_md = fetch_body(&chosen).await?;
    let parsed = crate::skill::parse_markdown(&body_md, &crate::skill::sanitize_name(&chosen.name));
    let name = if parsed.name.trim().is_empty() { chosen.name.clone() } else { parsed.name.clone() };
    crate::skill::save(&name, &parsed.description, &parsed.when, &parsed.body)?;
    Ok(crate::skill::Skill { name, ..parsed })
}

/// Bridge an async future to the sync `Tool::execute` path (multi-thread runtime worker only;
/// same invariant as the other async tools → `is_concurrency_safe()=false`).
fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

// ── agent tools ────────────────────────────────────────────────────────────────

pub struct SkillSearch;
impl Tool for SkillSearch {
    fn name(&self) -> &str {
        "skill_search"
    }
    fn description(&self) -> &str {
        "Search the agentskill.sh marketplace for a reusable skill (a saved procedure) by keyword. \
         Call this when a task needs a recurring how-to you don't already have a local skill for; \
         then skill_install the best match to use it."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "keywords, e.g. 'deploy fastapi' or 'release notes'"},
                "limit": {"type": "integer", "description": "max results (default 20)"}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let query = args.get("query").and_then(|v| v.as_str()).context("missing required string arg 'query'")?;
        let limit = args.get("limit").and_then(|v| v.as_u64()).map(|n| n as usize).unwrap_or(DEFAULT_LIMIT).clamp(1, 50);
        let hits = block(search(query, limit))?;
        if hits.is_empty() {
            return Ok(format!("no skills on {} match '{query}'", registry_base()));
        }
        let mut out = format!("{} result(s) from {} (skill_install \"<owner/name>\" to use one):\n", hits.len(), registry_base());
        for sk in &hits {
            out.push_str(&format!("- {}\n", sk.summary_line()));
        }
        Ok(out.trim_end().to_string())
    }
}

pub struct SkillInstall;
impl Tool for SkillInstall {
    fn name(&self) -> &str {
        "skill_install"
    }
    fn description(&self) -> &str {
        "Install a skill from agentskill.sh by its \"owner/name\" (or exact name) — fetches the \
         procedure, saves it locally, and returns its steps so you can follow them now. The skill is \
         third-party content, so this asks for approval."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"slug": {"type": "string", "description": "owner/name from skill_search, e.g. 'NousResearch/spike'"}},
            "required": ["slug"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn is_destructive(&self) -> bool {
        true // installs + persists third-party instructions the agent will then follow
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let slug = args.get("slug").and_then(|v| v.as_str()).context("missing required string arg 'slug'")?;
        let sk = block(install(slug))?;
        Ok(format!("installed '{}'.\n\n{}", sk.name, crate::skill::render_loaded(&sk)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn skill(owner: &str, repo: &str, sha: &str, path: &str) -> RegistrySkill {
        RegistrySkill {
            github_owner: owner.into(),
            github_repo: repo.into(),
            github_sha: sha.into(),
            github_path: path.into(),
            ..Default::default()
        }
    }

    #[test]
    fn raw_url_pins_to_sha_then_branch() {
        let s = skill("NousResearch", "hermes-agent", "abc123", "skills/dev/spike/SKILL.md");
        assert_eq!(
            s.raw_url().as_deref(),
            Some("https://raw.githubusercontent.com/NousResearch/hermes-agent/abc123/skills/dev/spike/SKILL.md")
        );
        // no sha → branch
        let mut b = skill("o", "r", "", "p.md");
        b.github_branch = "dev".into();
        assert_eq!(b.raw_url().as_deref(), Some("https://raw.githubusercontent.com/o/r/dev/p.md"));
        // no sha + no branch → main
        let m = skill("o", "r", "", "p.md");
        assert_eq!(m.raw_url().as_deref(), Some("https://raw.githubusercontent.com/o/r/main/p.md"));
        // missing coordinates → None
        assert_eq!(RegistrySkill::default().raw_url(), None);
    }

    #[test]
    fn id_prefers_slug_then_owner_name() {
        let mut s = RegistrySkill { name: "spike".into(), owner: "Nous".into(), ..Default::default() };
        assert_eq!(s.id(), "Nous/spike");
        s.slug = "Nous/spike-v2".into();
        assert_eq!(s.id(), "Nous/spike-v2");
    }

    #[test]
    fn search_response_parses_camel_case_and_tolerates_missing_fields() {
        let body = r#"{"data":[{"name":"spike","owner":"NousResearch","slug":"NousResearch/spike",
            "description":"throwaway experiments","securityScore":100,"contentQualityScore":83,
            "githubOwner":"NousResearch","githubRepo":"hermes-agent","githubSha":"deadbeef",
            "githubPath":"skills/spike/SKILL.md"}],"total":142}"#;
        let parsed: SearchResponse = serde_json::from_str(body).unwrap();
        assert_eq!(parsed.total, 142);
        assert_eq!(parsed.data.len(), 1);
        let s = &parsed.data[0];
        assert_eq!(s.security_score, Some(100));
        assert_eq!(s.content_quality_score, Some(83));
        assert_eq!(s.id(), "NousResearch/spike");
        assert!(s.raw_url().unwrap().ends_with("/deadbeef/skills/spike/SKILL.md"));
        // a sparse object (only name) must not fail
        let sparse: SearchResponse = serde_json::from_str(r#"{"data":[{"name":"x"}]}"#).unwrap();
        assert_eq!(sparse.data[0].name, "x");
    }

    #[test]
    fn install_tool_is_destructive_search_is_not() {
        assert!(SkillInstall.is_destructive(), "installing third-party instructions must be approval-gated");
        assert!(!SkillSearch.is_destructive());
        assert!(!SkillSearch.is_concurrency_safe());
        assert!(!SkillInstall.is_concurrency_safe());
    }
}
