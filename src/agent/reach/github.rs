//! The `github` channel — API-backed reads for github.com URLs.
//!
//! Routing by URL shape: repo root → repo metadata + README; `/blob/` → raw file
//! (raw.githubusercontent.com — unmetered, spends no API quota); `/tree/` → directory listing;
//! `/issues/N` and `/pull/N` → issue/PR body + top comments. Unauth API quota is 60 req/h per IP
//! (the raw backend has none) — a GITHUB_TOKEN lifts it to 5000/h. Anything unrecognized falls
//! back to the plain web chain.

use super::http;
use crate::agent::web_tools::{html_to_text, truncate_chars};
use anyhow::{bail, Result};

/// What a github.com / raw.githubusercontent.com URL points at.
#[derive(Debug, PartialEq)]
pub(crate) enum GhTarget {
    Repo {
        owner: String,
        repo: String,
    },
    Blob {
        owner: String,
        repo: String,
        rest: String,
    }, // "<ref>/<path>"
    Tree {
        owner: String,
        repo: String,
        rest: String,
    }, // "<ref>[/<path>]" ("" = default branch root)
    Issue {
        owner: String,
        repo: String,
        number: u64,
    },
    Raw {
        owner: String,
        repo: String,
        rest: String,
    }, // raw.githubusercontent.com/<o>/<r>/<ref>/<path>
    Other,
}

pub(crate) fn classify(url: &reqwest::Url) -> GhTarget {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let segs: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    if host == "raw.githubusercontent.com" {
        if segs.len() >= 3 {
            return GhTarget::Raw {
                owner: segs[0].to_string(),
                repo: segs[1].to_string(),
                rest: segs[2..].join("/"),
            };
        }
        return GhTarget::Other;
    }
    if host != "github.com" && host != "www.github.com" {
        return GhTarget::Other;
    }
    match segs.as_slice() {
        [owner, repo] => GhTarget::Repo {
            owner: owner.to_string(),
            repo: repo.to_string(),
        },
        [owner, repo, "blob", rest @ ..] if !rest.is_empty() => GhTarget::Blob {
            owner: owner.to_string(),
            repo: repo.to_string(),
            rest: rest.join("/"),
        },
        [owner, repo, "raw", rest @ ..] if !rest.is_empty() => GhTarget::Blob {
            owner: owner.to_string(),
            repo: repo.to_string(),
            rest: rest.join("/"),
        },
        [owner, repo, "tree", rest @ ..] => GhTarget::Tree {
            owner: owner.to_string(),
            repo: repo.to_string(),
            rest: rest.join("/"),
        },
        [owner, repo, kind, num] if *kind == "issues" || *kind == "pull" => match num.parse() {
            Ok(n) => GhTarget::Issue {
                owner: owner.to_string(),
                repo: repo.to_string(),
                number: n,
            },
            Err(_) => GhTarget::Other,
        },
        _ => GhTarget::Other,
    }
}

/// Standard GitHub API headers (a UA is mandatory — GitHub 403s without one; reqwest already sends
/// the aizen UA). Token appended when configured.
pub(crate) fn api_headers() -> Vec<(&'static str, String)> {
    let mut h = vec![
        ("Accept", "application/vnd.github+json".to_string()),
        ("X-GitHub-Api-Version", "2022-11-28".to_string()),
    ];
    if let Some(t) = super::github_token() {
        h.push(("Authorization", format!("Bearer {t}")));
    }
    h
}

/// Read a classified GitHub target through the api/raw chain. `Other` is the caller's problem
/// (route sends it down the plain web chain).
pub(crate) async fn read(url: &reqwest::Url, target: GhTarget) -> Result<String> {
    let c = http::client()?;
    let out = match &target {
        GhTarget::Repo { owner, repo } => read_repo(&c, owner, repo).await,
        GhTarget::Blob { owner, repo, rest } | GhTarget::Raw { owner, repo, rest } => {
            read_raw(&c, owner, repo, rest).await
        }
        GhTarget::Tree { owner, repo, rest } => read_tree(&c, owner, repo, rest).await,
        GhTarget::Issue {
            owner,
            repo,
            number,
        } => read_issue(&c, owner, repo, *number).await,
        GhTarget::Other => bail!("not an API-mappable GitHub URL"),
    };
    match out {
        Ok(s) => Ok(s),
        Err(e) => {
            // API path failed (rate limit / 404 / private) → plain page fetch is still worth a shot.
            super::note_err("github", "api", &e.to_string());
            let fallback = super::web::direct_read(&c, url.as_str())
                .await
                .inspect(|_| super::note_ok("github", "html"));
            fallback.map_err(|_| rate_limit_hint(e))
        }
    }
}

/// Make the 60/h wall actionable (the single most likely API failure).
fn rate_limit_hint(e: anyhow::Error) -> anyhow::Error {
    let msg = e.to_string();
    if msg.contains("403") || msg.contains("429") {
        anyhow::anyhow!(
            "{msg} — unauthenticated GitHub API is 60 req/h per IP; set GITHUB_TOKEN for 5000/h"
        )
    } else {
        e
    }
}

async fn read_repo(c: &reqwest::Client, owner: &str, repo: &str) -> Result<String> {
    let v = http::get_json(
        c,
        &format!("https://api.github.com/repos/{owner}/{repo}"),
        &api_headers(),
    )
    .await?;
    super::note_ok("github", "api");
    let mut s = format!(
        "[github repo {owner}/{repo}]\n{} ★{} forks:{} lang:{} license:{} updated:{}\n{}\n",
        v["full_name"].as_str().unwrap_or("?"),
        v["stargazers_count"].as_u64().unwrap_or(0),
        v["forks_count"].as_u64().unwrap_or(0),
        v["language"].as_str().unwrap_or("-"),
        v["license"]["spdx_id"].as_str().unwrap_or("-"),
        v["updated_at"].as_str().unwrap_or("-"),
        v["description"].as_str().unwrap_or("(no description)"),
    );
    if let Some(topics) = v["topics"].as_array() {
        if !topics.is_empty() {
            let t: Vec<&str> = topics.iter().filter_map(|x| x.as_str()).collect();
            s.push_str(&format!("topics: {}\n", t.join(", ")));
        }
    }
    // README body (raw media type skips the base64 dance). Best-effort — a missing README is fine.
    // Full API headers (token + version) with only the Accept overridden to the raw media type.
    let mut readme_headers = api_headers();
    for (k, v) in readme_headers.iter_mut() {
        if *k == "Accept" {
            *v = "application/vnd.github.raw+json".to_string();
        }
    }
    let readme = http::get(
        c,
        &format!("https://api.github.com/repos/{owner}/{repo}/readme"),
        &readme_headers,
    )
    .await;
    if let Ok(f) = readme {
        if f.is_success() {
            s.push_str(&format!(
                "\n── README ──\n{}",
                truncate_chars(&f.text(), 12_000)
            ));
        }
    }
    Ok(s.trim_end().to_string())
}

async fn read_raw(c: &reqwest::Client, owner: &str, repo: &str, rest: &str) -> Result<String> {
    let url = format!("https://raw.githubusercontent.com/{owner}/{repo}/{rest}");
    let f = http::get(c, &url, &[]).await?;
    if !f.is_success() {
        bail!("HTTP {} fetching {url}", f.status);
    }
    super::note_ok("github", "raw");
    Ok(format!(
        "[github raw {owner}/{repo}/{rest}]\n{}",
        truncate_chars(&f.text(), crate::agent::web_tools::FETCH_CAP)
    ))
}

async fn read_tree(c: &reqwest::Client, owner: &str, repo: &str, rest: &str) -> Result<String> {
    // Split "<ref>/<path…>" — the ref is the first segment; the rest (may be empty) is the path.
    let (r#ref, path) = match rest.split_once('/') {
        Some((r, p)) => (r, p),
        None => (rest, ""),
    };
    let url = if rest.is_empty() {
        format!("https://api.github.com/repos/{owner}/{repo}/contents/")
    } else {
        format!("https://api.github.com/repos/{owner}/{repo}/contents/{path}?ref={ref}", path = path, ref = r#ref)
    };
    let v = http::get_json(c, &url, &api_headers()).await?;
    super::note_ok("github", "api");
    let entries = v.as_array().cloned().unwrap_or_default();
    if entries.is_empty() {
        return Ok(format!(
            "[github tree {owner}/{repo}/{rest}]\n(empty directory)"
        ));
    }
    let mut s = format!(
        "[github tree {owner}/{repo}/{rest}] {} entries:\n",
        entries.len()
    );
    for e in entries.iter().take(200) {
        s.push_str(&format!(
            "{:>9}  {}{}\n",
            e["size"]
                .as_u64()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "-".into()),
            e["name"].as_str().unwrap_or("?"),
            if e["type"].as_str() == Some("dir") {
                "/"
            } else {
                ""
            }
        ));
    }
    if entries.len() > 200 {
        s.push_str(&format!("…and {} more\n", entries.len() - 200));
    }
    Ok(s.trim_end().to_string())
}

async fn read_issue(c: &reqwest::Client, owner: &str, repo: &str, number: u64) -> Result<String> {
    let v = http::get_json(
        c,
        &format!("https://api.github.com/repos/{owner}/{repo}/issues/{number}"),
        &api_headers(),
    )
    .await?;
    super::note_ok("github", "api");
    let kind = if v.get("pull_request").is_some() {
        "PR"
    } else {
        "issue"
    };
    let mut s = format!(
        "[github {kind} {owner}/{repo}#{number}] {} ({}) by {} — {} comments\n{}\n",
        v["title"].as_str().unwrap_or("(untitled)"),
        v["state"].as_str().unwrap_or("?"),
        v["user"]["login"].as_str().unwrap_or("?"),
        v["comments"].as_u64().unwrap_or(0),
        truncate_chars(v["body"].as_str().unwrap_or("(no body)"), 8_000),
    );
    // Top comments (one extra call, capped) — best-effort.
    if v["comments"].as_u64().unwrap_or(0) > 0 {
        let url = format!(
            "https://api.github.com/repos/{owner}/{repo}/issues/{number}/comments?per_page=10"
        );
        if let Ok(cs) = http::get_json(c, &url, &api_headers()).await {
            for cm in cs.as_array().cloned().unwrap_or_default() {
                s.push_str(&format!(
                    "\n── {} ──\n{}\n",
                    cm["user"]["login"].as_str().unwrap_or("?"),
                    truncate_chars(cm["body"].as_str().unwrap_or(""), 1_500)
                ));
            }
        }
    }
    Ok(s.trim_end().to_string())
}

// html fallback for completeness of the backend chain (route uses web::direct_read directly).
#[allow(dead_code)]
async fn read_html(c: &reqwest::Client, url: &str) -> Result<String> {
    let f = http::get(c, url, &[]).await?;
    Ok(format!(
        "[{} {}]\n{}",
        f.status,
        url,
        truncate_chars(&html_to_text(&f.text()), crate::agent::web_tools::FETCH_CAP)
    ))
}

// ── doctor probes ───────────────────────────────────────────────────────────

/// `/rate_limit` is FREE (does not consume quota) and doubles as an auth check.
pub(crate) async fn probe_api() -> super::Probe {
    let c = match http::client() {
        Ok(c) => c,
        Err(e) => return super::Probe::Fail(e.to_string()),
    };
    match http::get_json(&c, "https://api.github.com/rate_limit", &api_headers()).await {
        Ok(v) => {
            let limit = v["resources"]["core"]["limit"].as_u64().unwrap_or(0);
            let remaining = v["resources"]["core"]["remaining"].as_u64().unwrap_or(0);
            let msg = if limit > 60 {
                format!("API OK — {remaining}/{limit} req/h (token set)")
            } else {
                format!("API OK — {remaining}/{limit} req/h unauth (set GITHUB_TOKEN for 5000/h)")
            };
            if remaining == 0 {
                super::Probe::Warn(format!(
                    "{msg} — quota exhausted, raw/html fallbacks still work"
                ))
            } else {
                super::Probe::Ok(msg)
            }
        }
        Err(e) => super::Probe::Fail(http::snippet(&e.to_string())),
    }
}

pub(crate) async fn probe_raw() -> super::Probe {
    let c = match http::client() {
        Ok(c) => c,
        Err(e) => return super::Probe::Fail(e.to_string()),
    };
    match http::get(
        &c,
        "https://raw.githubusercontent.com/octocat/Hello-World/master/README",
        &[],
    )
    .await
    {
        Ok(f) if f.is_success() => super::Probe::Ok("raw file reads OK (unmetered)".into()),
        Ok(f) => super::Probe::Fail(format!("HTTP {}", f.status)),
        Err(e) => super::Probe::Fail(http::snippet(&e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(s: &str) -> reqwest::Url {
        reqwest::Url::parse(s).unwrap()
    }

    #[test]
    fn classifies_github_url_shapes() {
        assert_eq!(
            classify(&u("https://github.com/rust-lang/rust")),
            GhTarget::Repo {
                owner: "rust-lang".into(),
                repo: "rust".into()
            }
        );
        assert_eq!(
            classify(&u(
                "https://github.com/rust-lang/rust/blob/master/README.md"
            )),
            GhTarget::Blob {
                owner: "rust-lang".into(),
                repo: "rust".into(),
                rest: "master/README.md".into()
            }
        );
        assert_eq!(
            classify(&u(
                "https://github.com/tokio-rs/tokio/tree/master/tokio/src"
            )),
            GhTarget::Tree {
                owner: "tokio-rs".into(),
                repo: "tokio".into(),
                rest: "master/tokio/src".into()
            }
        );
        assert_eq!(
            classify(&u("https://github.com/serde-rs/serde/issues/1234")),
            GhTarget::Issue {
                owner: "serde-rs".into(),
                repo: "serde".into(),
                number: 1234
            }
        );
        assert_eq!(
            classify(&u("https://github.com/serde-rs/serde/pull/99")),
            GhTarget::Issue {
                owner: "serde-rs".into(),
                repo: "serde".into(),
                number: 99
            }
        );
        assert_eq!(
            classify(&u("https://raw.githubusercontent.com/o/r/main/src/lib.rs")),
            GhTarget::Raw {
                owner: "o".into(),
                repo: "r".into(),
                rest: "main/src/lib.rs".into()
            }
        );
        // Non-mappable shapes stay Other (and route falls back to the web chain).
        assert_eq!(
            classify(&u("https://github.com/rust-lang")),
            GhTarget::Other
        );
        assert_eq!(
            classify(&u("https://github.com/rust-lang/rust/actions")),
            GhTarget::Other
        );
        assert_eq!(
            classify(&u("https://github.com/o/r/issues/notanumber")),
            GhTarget::Other
        );
        assert_eq!(classify(&u("https://gitlab.com/o/r")), GhTarget::Other);
    }

    #[test]
    fn api_headers_always_carry_version_and_accept() {
        let h = api_headers();
        assert!(h.iter().any(|(k, _)| *k == "Accept"));
        assert!(h.iter().any(|(k, _)| *k == "X-GitHub-Api-Version"));
    }
}
