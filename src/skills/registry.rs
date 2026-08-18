//! agentskill.sh skill registry client — search + install reusable skills from the public
//! marketplace (274k+ skills). Pure HTTP/JSON over reqwest (NO npx/Node). Search hits
//! `GET {base}/api/skills?q=…`; the API returns each skill's GitHub coordinates, and the body is
//! fetched from `raw.githubusercontent.com`. The base defaults to `https://agentskill.sh` and is
//! overridable via `cli-config.json` `skill_registry`.
//!
//! Installed skills are THIRD-PARTY instructions the agent will then follow → `skill_install` is
//! `is_destructive() = true` (approval-gated) and search surfaces the registry's `securityScore`.
//!
//! Network posture: every outbound URL (the registry base AND the GitHub raw URL built from the
//! registry's response) passes the `net_guard` SSRF floor, and all fetches go through the shared
//! `reach::http` guarded client — auto-redirects disabled, every redirect hop re-vetted, bodies
//! bounded — so neither a poisoned `skill_registry` config nor a malicious registry response can
//! steer a fetch at loopback/private/link-local targets (cloud metadata et al.).

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
    /// Whether another page exists — the browse fallback stops rather than asking past the end.
    #[serde(default, rename = "hasMore")]
    has_more: bool,
}

/// The by-slug record, from the route that serves one named skill. It carries the markdown itself
/// rather than GitHub coordinates, so an install through it never leaves the registry.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DirectSkill {
    #[serde(default)]
    name: String,
    #[serde(default)]
    skill_md: String,
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
        if self.github_owner.is_empty()
            || self.github_repo.is_empty()
            || self.github_path.is_empty()
        {
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
        Some(format!(
            "https://raw.githubusercontent.com/{}/{}/{reff}/{path}",
            self.github_owner, self.github_repo
        ))
    }

    /// One-line summary for the search UI / tool result.
    pub fn summary_line(&self) -> String {
        let q = self
            .content_quality_score
            .map(|s| format!(" · quality {s}"))
            .unwrap_or_default();
        let s = self
            .security_score
            .map(|s| format!(" · security {s}"))
            .unwrap_or_default();
        let desc: String = self.description.chars().take(90).collect();
        format!("{}{q}{s} — {desc}", self.id())
    }
}

/// The configured registry base (default `agentskill.sh`).
pub fn registry_base() -> String {
    crate::core::cli_config::load()
        .skill_registry
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_REGISTRY.to_string())
}

/// Search the registry by keyword. `limit` caps results. SSRF-guarded (see the module doc).
///
/// The keyword index is asked first and, when it fails, is not the last word: the catalogue is
/// still served page by page, and matching the query against those pages here finds the same rows
/// the index would have. Slower, and only on the failing path — but a registry whose search is
/// down is not a registry with no skills on it, and for a day of HTTP 500s that is exactly how
/// this read to everyone using it.
pub async fn search(query: &str, limit: usize) -> Result<Vec<RegistrySkill>> {
    let indexed = match search_index(query, limit).await {
        Ok(hits) => return Ok(hits),
        Err(e) => e,
    };
    // Nothing found by hand does not disprove the index — say what actually went wrong instead of
    // reporting an empty catalogue.
    match browse_match(query, limit).await {
        Ok(hits) if !hits.is_empty() => Ok(hits),
        _ => Err(indexed),
    }
}

/// Every word of the query has to appear somewhere in what the row says about itself — the way a
/// person reading down the catalogue would match it.
fn matches(sk: &RegistrySkill, words: &[String]) -> bool {
    let hay = format!("{} {} {}", sk.id(), sk.name, sk.description).to_lowercase();
    words.iter().all(|w| hay.contains(w.as_str()))
}

/// How far down the catalogue the fallback is willing to read. Bounded on purpose: this runs only
/// when the index is down, and a search that quietly turns into hundreds of requests is its own
/// kind of outage.
const BROWSE_PAGES: usize = 12;
const BROWSE_PAGE_SIZE: usize = 100;

/// Page the listing route and filter locally. Stops at the first full page of results, at the end
/// of the catalogue, or at `BROWSE_PAGES` — whichever comes first.
async fn browse_match(query: &str, limit: usize) -> Result<Vec<RegistrySkill>> {
    let words: Vec<String> = query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|w| !w.is_empty())
        .collect();
    if words.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }
    let base = registry_base();
    let client = crate::agent::reach::http::client()?;
    let mut out: Vec<RegistrySkill> = Vec::new();
    for page in 1..=BROWSE_PAGES {
        let url = reqwest::Url::parse_with_params(
            &format!("{}/api/skills", base.trim_end_matches('/')),
            &[
                ("limit", BROWSE_PAGE_SIZE.to_string()),
                ("page", page.to_string()),
            ],
        )
        .with_context(|| format!("bad registry base '{base}'"))?;
        crate::core::net_guard::guard_url_async(url.as_str()).await?;
        let f = crate::agent::reach::http::get(&client, url.as_str(), &[])
            .await
            .with_context(|| format!("reading the catalogue on {base}"))?;
        if !f.is_success() {
            break;
        }
        let Ok(parsed) = serde_json::from_slice::<SearchResponse>(&f.body) else {
            break;
        };
        if parsed.data.is_empty() {
            break;
        }
        let more = parsed.has_more;
        for sk in parsed.data {
            if matches(&sk, &words) && !out.iter().any(|o| o.id() == sk.id()) {
                out.push(sk);
                if out.len() >= limit {
                    return Ok(out);
                }
            }
        }
        if !more {
            break;
        }
    }
    Ok(out)
}

/// Ask the registry's own keyword index.
async fn search_index(query: &str, limit: usize) -> Result<Vec<RegistrySkill>> {
    let base = registry_base();
    let url = reqwest::Url::parse_with_params(
        &format!("{}/api/skills", base.trim_end_matches('/')),
        &[("q", query), ("limit", &limit.to_string())],
    )
    .with_context(|| format!("bad registry base '{base}'"))?;
    crate::core::net_guard::guard_url_async(url.as_str()).await?;
    let client = crate::agent::reach::http::client()?;
    let f = crate::agent::reach::http::get(&client, url.as_str(), &[])
        .await
        .with_context(|| format!("searching {base}"))?;
    if !f.is_success() {
        bail!("registry {base} returned HTTP {}", f.status);
    }
    let parsed: SearchResponse =
        serde_json::from_slice(&f.body).context("parsing registry JSON")?;
    let _ = parsed.total;
    Ok(parsed.data)
}

/// Fetch a registry skill's markdown body from its GitHub raw URL. The URL is BUILT from the
/// registry's response (untrusted input) → it passes the SSRF floor like any model-supplied URL.
pub async fn fetch_body(sk: &RegistrySkill) -> Result<String> {
    let url = sk
        .raw_url()
        .context("this skill has no GitHub source location")?;
    crate::core::net_guard::guard_url_async(&url).await?;
    let client = crate::agent::reach::http::client()?;
    let f = crate::agent::reach::http::get(&client, &url, &[])
        .await
        .with_context(|| format!("GET {url}"))?;
    if !f.is_success() {
        bail!("GitHub returned HTTP {} for {url}", f.status);
    }
    Ok(f.text())
}

/// The URL that serves one skill by its `owner/name`. Built through `path_segments_mut` so the
/// slash inside the slug is encoded as a slug and cannot become another path segment — a name is
/// data here, and a registry route is not a place to let data spell itself.
fn by_slug_url(base: &str, slug: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(base.trim_end_matches('/'))
        .with_context(|| format!("bad registry base '{base}'"))?;
    url.path_segments_mut()
        .map_err(|_| anyhow::anyhow!("registry base '{base}' cannot hold a path"))?
        .pop_if_empty()
        .extend(["api", "agent", "skills", slug, "install"]);
    Ok(url)
}

/// Fetch one skill by `owner/name`. `Ok(None)` means the registry does not serve this route or
/// does not know the slug — both are reasons to fall back to a search, not reasons to fail.
async fn by_slug(slug: &str) -> Result<Option<DirectSkill>> {
    let base = registry_base();
    let url = by_slug_url(&base, slug)?;
    crate::core::net_guard::guard_url_async(url.as_str()).await?;
    let client = crate::agent::reach::http::client()?;
    let f = crate::agent::reach::http::get(&client, url.as_str(), &[])
        .await
        .with_context(|| format!("asking {base} for '{slug}'"))?;
    if !f.is_success() {
        return Ok(None);
    }
    let got: DirectSkill = match serde_json::from_slice(&f.body) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };
    Ok((!got.skill_md.trim().is_empty()).then_some(got))
}

/// Parse one skill's markdown and save it locally, under its own `name:` when it declares one.
fn save_body(fallback: &str, body_md: &str) -> Result<crate::skills::Skill> {
    let parsed = crate::skills::parse_markdown(body_md, &crate::skills::sanitize_name(fallback));
    let name = if parsed.name.trim().is_empty() {
        fallback.to_string()
    } else {
        parsed.name.clone()
    };
    crate::skills::save(&name, &parsed.description, &parsed.when, &parsed.body)?;
    Ok(crate::skills::Skill { name, ..parsed })
}

/// Install one skill by name. An exact `owner/name` is fetched by slug; anything else is a search
/// followed by the best match.
///
/// The by-slug route first because an install by exact name never needed the search index, and
/// borrowing it meant borrowing its outages: when agentskill.sh's index began answering HTTP 500
/// (2026-08-18) every `skill install` failed too, though every skill on it was still being served.
/// A dependency that only ever costs you is one to drop.
pub async fn install(query: &str) -> Result<crate::skills::Skill> {
    let q = query.trim();
    if q.contains('/') {
        if let Some(saved) = install_by_slug(q, q).await? {
            return Ok(saved);
        }
    }

    let hits = search(q, DEFAULT_LIMIT).await?;
    if hits.is_empty() {
        bail!("no skill on {} matches '{query}'", registry_base());
    }
    let want = q.to_lowercase();
    let chosen = hits
        .iter()
        .find(|s| s.id().to_lowercase() == want || s.name.to_lowercase() == want)
        .unwrap_or(&hits[0])
        .clone();

    // The registry's own copy before GitHub's. Its stored coordinates are a snapshot of where the
    // file was, and repositories move: `bytedance/frontend-design` is listed at a path that now
    // answers 404, while the registry still serves the same skill by slug. Falling at that fence
    // would fail an install for a reason that has nothing to do with the skill.
    if let Some(saved) = install_by_slug(&chosen.id(), &chosen.name).await? {
        return Ok(saved);
    }
    let body_md = fetch_body(&chosen).await?;
    save_body(&chosen.name, &body_md)
}

/// Fetch `slug` from the registry's by-slug route and save it. `Ok(None)` when that route has
/// nothing for this slug, which leaves the caller free to try GitHub.
async fn install_by_slug(slug: &str, name_hint: &str) -> Result<Option<crate::skills::Skill>> {
    let Some(direct) = by_slug(slug).await? else {
        return Ok(None);
    };
    let fallback = [direct.name.trim(), name_hint.trim()]
        .into_iter()
        .find(|n| !n.is_empty())
        .unwrap_or(slug)
        .to_string();
    // A bare `owner/` is not a name to save a file under.
    let fallback = fallback
        .rsplit('/')
        .find(|p| !p.is_empty())
        .unwrap_or(slug)
        .to_string();
    save_body(&fallback, &direct.skill_md).map(Some)
}

/// Bridge an async future to the sync `Tool::execute` path — the shared cancel-aware bridge
/// (valid on workers AND spawn_blocking threads; Esc aborts an in-flight marketplace call).
fn block<T>(f: impl std::future::Future<Output = anyhow::Result<T>>) -> anyhow::Result<T> {
    crate::agent::tools::block_for_tool(f)
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
        true // read-only marketplace query — the shared bridge is spawn_blocking-safe
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .context("missing required string arg 'query'")?;
        let limit = args
            .get("limit")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_LIMIT)
            .clamp(1, 50);
        let hits = block(search(query, limit))?;
        if hits.is_empty() {
            return Ok(format!("no skills on {} match '{query}'", registry_base()));
        }
        let mut out = format!(
            "{} result(s) from {} (skill_install \"<owner/name>\" to use one):\n",
            hits.len(),
            registry_base()
        );
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
        let slug = args
            .get("slug")
            .and_then(|v| v.as_str())
            .context("missing required string arg 'slug'")?;
        let sk = block(install(slug))?;
        Ok(format!(
            "installed '{}'.\n\n{}",
            sk.name,
            crate::skills::render_loaded(&sk)
        ))
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
        let s = skill(
            "NousResearch",
            "hermes-agent",
            "abc123",
            "skills/dev/spike/SKILL.md",
        );
        assert_eq!(
            s.raw_url().as_deref(),
            Some("https://raw.githubusercontent.com/NousResearch/hermes-agent/abc123/skills/dev/spike/SKILL.md")
        );
        // no sha → branch
        let mut b = skill("o", "r", "", "p.md");
        b.github_branch = "dev".into();
        assert_eq!(
            b.raw_url().as_deref(),
            Some("https://raw.githubusercontent.com/o/r/dev/p.md")
        );
        // no sha + no branch → main
        let m = skill("o", "r", "", "p.md");
        assert_eq!(
            m.raw_url().as_deref(),
            Some("https://raw.githubusercontent.com/o/r/main/p.md")
        );
        // missing coordinates → None
        assert_eq!(RegistrySkill::default().raw_url(), None);
    }

    /// The slug is one path segment, however many slashes it contains. If it ever stopped being
    /// one, `a/../b` would address something other than the skill named `a/../b`.
    #[test]
    fn a_slug_is_one_encoded_segment() {
        assert_eq!(
            by_slug_url("https://agentskill.sh", "agentskill-sh/learn")
                .unwrap()
                .as_str(),
            "https://agentskill.sh/api/agent/skills/agentskill-sh%2Flearn/install"
        );
        // A base with its own path keeps it, and a trailing slash does not become an empty segment.
        assert_eq!(
            by_slug_url("https://reg.example.com/aizen/", "o/n")
                .unwrap()
                .as_str(),
            "https://reg.example.com/aizen/api/agent/skills/o%2Fn/install"
        );
        // Traversal is data, not structure.
        assert_eq!(
            by_slug_url("https://agentskill.sh", "../../etc")
                .unwrap()
                .as_str(),
            "https://agentskill.sh/api/agent/skills/..%2F..%2Fetc/install"
        );
    }

    /// The fallback matches on what a row says about itself, and only on all of the words —
    /// otherwise a two-word query returns everything that shares its commonest half.
    #[test]
    fn the_browse_fallback_matches_every_word_or_none() {
        let mut sk = RegistrySkill {
            name: "frontend-design".into(),
            slug: "bytedance/frontend-design".into(),
            description: "Create distinctive, production-grade interfaces".into(),
            ..Default::default()
        };
        let words =
            |q: &str| -> Vec<String> { q.split_whitespace().map(str::to_lowercase).collect() };
        assert!(matches(&sk, &words("frontend")));
        assert!(
            matches(&sk, &words("FRONTEND")),
            "case is not a distinction"
        );
        assert!(matches(&sk, &words("frontend interfaces")));
        assert!(
            matches(&sk, &words("bytedance")),
            "the owner is part of the row"
        );
        assert!(
            !matches(&sk, &words("frontend rust")),
            "both words or neither"
        );
        assert!(!matches(&sk, &words("kubernetes")));
        // A row that says nothing about itself matches nothing but its own name.
        sk.description = String::new();
        assert!(!matches(&sk, &words("interfaces")));
    }

    /// The listing's paging flag is read, so the fallback stops at the end of the catalogue rather
    /// than asking twelve times for a page that is not there.
    #[test]
    fn the_listing_says_whether_another_page_exists() {
        let body = r#"{"data":[],"total":4,"page":2,"limit":100,"hasMore":true}"#;
        let r: SearchResponse = serde_json::from_str(body).unwrap();
        assert!(r.has_more);
        let last: SearchResponse = serde_json::from_str(r#"{"data":[]}"#).unwrap();
        assert!(!last.has_more, "a response that omits it has no more pages");
    }

    /// The by-slug record is the whole install: markdown in hand, no GitHub round trip.
    #[test]
    fn a_direct_record_carries_the_markdown() {
        let body = r#"{"slug":"o/n","name":"learn","skillMd":"---\nname: learn\n---\nsteps",
            "installCount":5,"securityScore":90}"#;
        let d: DirectSkill = serde_json::from_str(body).unwrap();
        assert_eq!(d.name, "learn");
        assert!(d.skill_md.contains("steps"));
        // A record without a body is not an install — `by_slug` treats it as a miss.
        let empty: DirectSkill = serde_json::from_str(r#"{"name":"x"}"#).unwrap();
        assert!(empty.skill_md.is_empty());
    }

    #[test]
    fn id_prefers_slug_then_owner_name() {
        let mut s = RegistrySkill {
            name: "spike".into(),
            owner: "Nous".into(),
            ..Default::default()
        };
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
        assert!(s
            .raw_url()
            .unwrap()
            .ends_with("/deadbeef/skills/spike/SKILL.md"));
        // a sparse object (only name) must not fail
        let sparse: SearchResponse = serde_json::from_str(r#"{"data":[{"name":"x"}]}"#).unwrap();
        assert_eq!(sparse.data[0].name, "x");
    }

    /// Offline SSRF wiring test: a loopback registry base must be refused by the net_guard floor
    /// BEFORE any network I/O (literal private/loopback hosts reject synchronously, no DNS).
    /// A plain #[test] + local block_on so the env lock is never held across an await point.
    #[test]
    fn search_refuses_a_private_registry_base() {
        let _g = crate::core::config::TEST_HOME_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("aizen-registry-ssrf-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::env::set_var("AIZEN_HOME", &dir);
        let mut cfg = crate::core::cli_config::load();
        cfg.skill_registry = Some("http://127.0.0.1:9".to_string());
        crate::core::cli_config::save(&cfg).unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let res = rt.block_on(search("anything", 5));
        std::env::remove_var("AIZEN_HOME");
        let _ = std::fs::remove_dir_all(&dir);
        let err = res.expect_err("a loopback registry base must be refused");
        assert!(
            format!("{err:#}").contains("SSRF"),
            "refused by the SSRF floor: {err:#}"
        );
    }

    #[test]
    fn install_tool_is_destructive_search_is_not() {
        assert!(
            SkillInstall.is_destructive(),
            "installing third-party instructions must be approval-gated"
        );
        assert!(!SkillSearch.is_destructive());
        assert!(
            SkillSearch.is_concurrency_safe(),
            "read-only marketplace query parallelizes"
        );
        assert!(
            !SkillInstall.is_concurrency_safe(),
            "installs stay serial (writes to disk)"
        );
    }
}
