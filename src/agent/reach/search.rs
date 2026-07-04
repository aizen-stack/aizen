//! The `search` channel — keyless web search with an automatic backend chain, plus per-platform
//! search for the sites that expose real APIs (GitHub / Hacker News / Stack Overflow / Wikipedia).
//!
//! Web chain: `ddg-html` (POST https://html.duckduckgo.com/html/) → `ddg-lite`
//! (https://lite.duckduckgo.com/lite/, lighter markup, same operator) → `jina-search`
//! (s.jina.ai — needs a key; skipped keyless). DDG's anomaly wall answers HTTP 202 with an empty
//! page instead of an error — that is treated as "blocked, fall through", and DDG calls are paced
//! ≥2 s apart to stay under the wall. (Live-verified 2026-07-04: the aizen UA is accepted; only
//! curl-shaped UAs are blocked.)

use super::http;
use crate::agent::web_tools::{percent_decode, strip_tags};
use anyhow::{bail, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use std::time::Duration;

pub(crate) struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

/// Numbered list, the classic `web_search` output shape.
pub(crate) fn render_results(results: &[SearchResult]) -> String {
    let mut s = String::new();
    for (i, r) in results.iter().enumerate() {
        s.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.url));
        if !r.snippet.is_empty() {
            s.push_str(&format!("   {}\n", r.snippet));
        }
    }
    s.trim_end().to_string()
}

/// Search the web through the backend chain. Returns a friendly "(no results …)" — never an error —
/// only after EVERY backend answered and found nothing: a 200 that parses to zero results is
/// indistinguishable from markup drift or a consent page (the doctor probe treats it as a failure
/// for the same reason), so it falls through to the next backend instead of ending the chain.
pub(crate) async fn web(query: &str, limit: usize) -> Result<String> {
    let c = http::client()?;
    let mut failures: Vec<String> = Vec::new();
    let mut any_answered_empty = false;

    for backend in super::ordered_backends("search") {
        let outcome = match backend {
            "ddg-html" => ddg_html(&c, query, limit).await,
            "ddg-lite" => ddg_lite(&c, query, limit).await,
            "jina-search" => match super::jina_key() {
                Some(k) => jina_search(&c, query, limit, &k).await,
                None => continue, // optional keyed backend — silently absent keyless
            },
            _ => continue,
        };
        match outcome {
            Ok(results) if !results.is_empty() => {
                super::note_ok("search", backend);
                return Ok(render_results(&results));
            }
            Ok(_) => {
                // Answered but empty — possibly genuine, possibly a silent block/markup change.
                // Don't mark the backend healthy; let the rest of the chain have a shot.
                any_answered_empty = true;
            }
            Err(e) => {
                super::note_err("search", backend, &e.to_string());
                failures.push(format!("{backend}: {}", http::snippet(&e.to_string())));
            }
        }
    }
    if any_answered_empty {
        return Ok(format!("(no results for '{query}')"));
    }
    bail!("all search backends failed — {}", failures.join("; "))
}

// ── DuckDuckGo (html + lite endpoints) ──────────────────────────────────────

async fn ddg_html(c: &reqwest::Client, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
    super::pace("ddg", Duration::from_secs(2)).await;
    let f = http::post_form(c, "https://html.duckduckgo.com/html/", &[], &[("q", query)]).await?;
    if f.status == 202 {
        bail!("DuckDuckGo anomaly wall (HTTP 202) — backing off");
    }
    if !f.is_success() {
        bail!("DuckDuckGo returned HTTP {}", f.status);
    }
    Ok(parse_ddg(&f.text(), limit))
}

async fn ddg_lite(c: &reqwest::Client, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
    super::pace("ddg", Duration::from_secs(2)).await;
    let f = http::post_form(c, "https://lite.duckduckgo.com/lite/", &[], &[("q", query)]).await?;
    if f.status == 202 {
        bail!("DuckDuckGo lite anomaly wall (HTTP 202) — backing off");
    }
    if !f.is_success() {
        bail!("DuckDuckGo lite returned HTTP {}", f.status);
    }
    Ok(parse_ddg_lite(&f.text(), limit))
}

/// Parse DuckDuckGo's html-endpoint result list into (title, url, snippet) triples.
pub(crate) fn parse_ddg(html: &str, limit: usize) -> Vec<SearchResult> {
    static ANCHOR: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?is)<a[^>]+class="[^"]*result__a[^"]*"[^>]+href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap()
    });
    static SNIPPET: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?is)<a[^>]+class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</a>"#).unwrap()
    });
    let titled: Vec<(String, String)> =
        ANCHOR.captures_iter(html).map(|c| (c[1].to_string(), strip_tags(&c[2]))).collect();
    let snippets: Vec<String> = SNIPPET.captures_iter(html).map(|c| strip_tags(&c[1])).collect();
    titled
        .into_iter()
        .enumerate()
        .take(limit)
        .map(|(i, (href, title))| SearchResult {
            url: ddg_unwrap(&href),
            title,
            snippet: snippets.get(i).cloned().unwrap_or_default(),
        })
        .collect()
}

/// Parse the lite endpoint (`result-link` anchors; snippets live in `result-snippet` table cells).
/// Attribute ORDER is not fixed in this markup (`href` often precedes `class`), so the anchor tag
/// is matched on the class token and the href extracted from the attribute blob separately.
pub(crate) fn parse_ddg_lite(html: &str, limit: usize) -> Vec<SearchResult> {
    static ANCHOR: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?is)<a\b([^>]*\bresult-link\b[^>]*)>(.*?)</a>"#).unwrap());
    static HREF: Lazy<Regex> = Lazy::new(|| Regex::new(r#"(?is)href\s*=\s*["']([^"']+)["']"#).unwrap());
    static SNIPPET: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?is)<td[^>]+class=["'][^"']*result-snippet[^"']*["'][^>]*>(.*?)</td>"#).unwrap()
    });
    let titled: Vec<(String, String)> = ANCHOR
        .captures_iter(html)
        .filter_map(|c| {
            let href = HREF.captures(&c[1]).map(|h| h[1].to_string())?;
            Some((href, strip_tags(&c[2])))
        })
        .collect();
    let snippets: Vec<String> = SNIPPET.captures_iter(html).map(|c| strip_tags(&c[1])).collect();
    titled
        .into_iter()
        .enumerate()
        .take(limit)
        .map(|(i, (href, title))| SearchResult {
            url: ddg_unwrap(&href),
            title,
            snippet: snippets.get(i).cloned().unwrap_or_default(),
        })
        .collect()
}

/// DuckDuckGo wraps result links: `//duckduckgo.com/l/?uddg=<percent-encoded-url>&rut=…`.
/// Pull out and decode the real target; pass through a direct/protocol-relative URL.
pub(crate) fn ddg_unwrap(href: &str) -> String {
    if let Some(idx) = href.find("uddg=") {
        let rest = &href[idx + 5..];
        let enc = rest.split('&').next().unwrap_or(rest);
        return percent_decode(enc);
    }
    if let Some(stripped) = href.strip_prefix("//") {
        format!("https://{stripped}")
    } else {
        href.to_string()
    }
}

// ── Jina search (keyed only) ────────────────────────────────────────────────

async fn jina_search(c: &reqwest::Client, query: &str, limit: usize, key: &str) -> Result<Vec<SearchResult>> {
    let url = format!("https://s.jina.ai/?q={}", urlencode(query));
    let headers = [
        ("Authorization", format!("Bearer {key}")),
        ("Accept", "application/json".to_string()),
        ("X-Respond-With", "no-content".to_string()),
    ];
    let v = http::get_json(c, &url, &headers).await?;
    let items = v["data"].as_array().cloned().unwrap_or_default();
    Ok(items
        .iter()
        .take(limit)
        .map(|it| SearchResult {
            title: it["title"].as_str().unwrap_or("(untitled)").to_string(),
            url: it["url"].as_str().unwrap_or("").to_string(),
            snippet: it["description"].as_str().unwrap_or("").to_string(),
        })
        .collect())
}

pub(crate) fn urlencode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── doctor probes ───────────────────────────────────────────────────────────

pub(crate) async fn probe_ddg_html() -> super::Probe {
    match probe_ddg(false).await {
        Ok(msg) => super::Probe::Ok(msg),
        Err(e) if e.to_string().contains("202") => super::Probe::Warn(http::snippet(&e.to_string())),
        Err(e) => super::Probe::Fail(http::snippet(&e.to_string())),
    }
}

pub(crate) async fn probe_ddg_lite() -> super::Probe {
    match probe_ddg(true).await {
        Ok(msg) => super::Probe::Ok(msg),
        Err(e) if e.to_string().contains("202") => super::Probe::Warn(http::snippet(&e.to_string())),
        Err(e) => super::Probe::Fail(http::snippet(&e.to_string())),
    }
}

async fn probe_ddg(lite: bool) -> Result<String> {
    let c = http::client()?;
    let results = if lite { ddg_lite(&c, "duckduckgo", 3).await? } else { ddg_html(&c, "duckduckgo", 3).await? };
    if results.is_empty() {
        bail!("responded but returned 0 parseable results (markup change?)");
    }
    Ok(format!("search OK ({} results for the probe query)", results.len()))
}

pub(crate) async fn probe_jina_search() -> super::Probe {
    let Some(key) = super::jina_key() else {
        return super::Probe::Off("needs a (free) Jina key — set JINA_API_KEY".into());
    };
    let c = match http::client() {
        Ok(c) => c,
        Err(e) => return super::Probe::Fail(e.to_string()),
    };
    match jina_search(&c, "rust", 1, &key).await {
        Ok(_) => super::Probe::Ok("keyed search OK".into()),
        Err(e) => super::Probe::Fail(http::snippet(&e.to_string())),
    }
}

// ── per-platform search (the `site` param of web_search) ───────────────────────

/// GitHub repository search (unauth quota: 10 searches/min).
pub(crate) async fn github(query: &str, limit: usize) -> Result<String> {
    let c = http::client()?;
    let url = format!(
        "https://api.github.com/search/repositories?q={}&sort=stars&order=desc&per_page={limit}",
        urlencode(query)
    );
    let v = http::get_json(&c, &url, &super::github::api_headers())
        .await
        .inspect_err(|e| super::note_err("github", "api", &e.to_string()))?;
    super::note_ok("github", "api");
    let items = v["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return Ok(format!("(no GitHub repositories match '{query}')"));
    }
    let results: Vec<SearchResult> = items
        .iter()
        .take(limit)
        .map(|r| SearchResult {
            title: format!(
                "{} ★{} {}",
                r["full_name"].as_str().unwrap_or("?"),
                r["stargazers_count"].as_u64().unwrap_or(0),
                r["language"].as_str().unwrap_or("")
            )
            .trim_end()
            .to_string(),
            url: r["html_url"].as_str().unwrap_or("").to_string(),
            snippet: r["description"].as_str().unwrap_or("").to_string(),
        })
        .collect();
    Ok(render_results(&results))
}

/// Hacker News search via Algolia (rock-solid, 10k req/h).
pub(crate) async fn hackernews(query: &str, limit: usize) -> Result<String> {
    let c = http::client()?;
    let url = format!("https://hn.algolia.com/api/v1/search?query={}&hitsPerPage={limit}", urlencode(query));
    let v = http::get_json(&c, &url, &[])
        .await
        .inspect_err(|e| super::note_err("hackernews", "algolia", &e.to_string()))?;
    super::note_ok("hackernews", "algolia");
    let hits = v["hits"].as_array().cloned().unwrap_or_default();
    if hits.is_empty() {
        return Ok(format!("(no Hacker News results for '{query}')"));
    }
    let results: Vec<SearchResult> = hits
        .iter()
        .take(limit)
        .map(|h| {
            let id = h["objectID"].as_str().unwrap_or("");
            SearchResult {
                title: format!(
                    "{} ({} points, {} comments)",
                    h["title"].as_str().unwrap_or("(untitled)"),
                    h["points"].as_u64().unwrap_or(0),
                    h["num_comments"].as_u64().unwrap_or(0)
                ),
                url: format!("https://news.ycombinator.com/item?id={id}"),
                snippet: h["url"].as_str().unwrap_or("").to_string(),
            }
        })
        .collect();
    Ok(render_results(&results))
}

/// Stack Overflow search (StackExchange API 2.3; keyless quota 300/day/IP).
pub(crate) async fn stackoverflow(query: &str, limit: usize) -> Result<String> {
    let c = http::client()?;
    let url = format!(
        "https://api.stackexchange.com/2.3/search/advanced?q={}&site=stackoverflow&order=desc&sort=relevance&pagesize={limit}",
        urlencode(query)
    );
    let v = http::get_json(&c, &url, &super::stackex::api_headers())
        .await
        .inspect_err(|e| super::note_err("stackexchange", "api", &e.to_string()))?;
    super::note_ok("stackexchange", "api");
    let items = v["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return Ok(format!("(no Stack Overflow results for '{query}')"));
    }
    let results: Vec<SearchResult> = items
        .iter()
        .take(limit)
        .map(|q| SearchResult {
            title: format!(
                "{} (score {}, {})",
                crate::agent::web_tools::decode_entities(q["title"].as_str().unwrap_or("(untitled)")),
                q["score"].as_i64().unwrap_or(0),
                if q["is_answered"].as_bool().unwrap_or(false) { "answered" } else { "unanswered" }
            ),
            url: q["link"].as_str().unwrap_or("").to_string(),
            snippet: String::new(),
        })
        .collect();
    Ok(render_results(&results))
}

/// Wikipedia title search (the opensearch action API).
pub(crate) async fn wikipedia(query: &str, limit: usize) -> Result<String> {
    let c = http::client()?;
    let url = format!(
        "https://en.wikipedia.org/w/api.php?action=opensearch&search={}&limit={limit}&format=json",
        urlencode(query)
    );
    let v = http::get_json(&c, &url, &[])
        .await
        .inspect_err(|e| super::note_err("wikipedia", "rest", &e.to_string()))?;
    super::note_ok("wikipedia", "rest");
    // opensearch returns [query, [titles], [descriptions], [urls]]
    let titles = v.get(1).and_then(|t| t.as_array()).cloned().unwrap_or_default();
    let urls = v.get(3).and_then(|u| u.as_array()).cloned().unwrap_or_default();
    if titles.is_empty() {
        return Ok(format!("(no Wikipedia articles match '{query}')"));
    }
    let results: Vec<SearchResult> = titles
        .iter()
        .zip(urls.iter().chain(std::iter::repeat(&serde_json::Value::Null)))
        .take(limit)
        .map(|(t, u)| SearchResult {
            title: t.as_str().unwrap_or("(untitled)").to_string(),
            url: u.as_str().unwrap_or("").to_string(),
            snippet: String::new(),
        })
        .collect();
    Ok(render_results(&results))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_ddg_extracts_results() {
        let html = r#"
            <div class="result">
              <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Frust-lang.org%2F">The <b>Rust</b> Language</a>
              <a class="result__snippet">A language empowering everyone.</a>
            </div>
            <div class="result">
              <a class="result__a" href="https://docs.rs/">docs.rs</a>
              <a class="result__snippet">Docs for &amp; crates.</a>
            </div>"#;
        let r = parse_ddg(html, 5);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].url, "https://rust-lang.org/");
        assert_eq!(r[0].title, "The Rust Language");
        assert_eq!(r[0].snippet, "A language empowering everyone.");
        assert_eq!(r[1].url, "https://docs.rs/");
        assert_eq!(r[1].snippet, "Docs for & crates.");
    }

    #[test]
    fn parse_ddg_respects_limit() {
        let block = r#"<a class="result__a" href="https://a.com/">A</a>"#.repeat(10);
        assert_eq!(parse_ddg(&block, 3).len(), 3);
    }

    #[test]
    fn parse_ddg_lite_extracts_results() {
        let html = r#"
            <tr><td><a rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Ftokio.rs%2F" class="result-link">Tokio - <b>async</b> runtime</a></td></tr>
            <tr><td class="result-snippet">Build reliable network applications.</td></tr>
            <tr><td><a href="https://docs.rs/tokio" class="result-link">tokio - Rust</a></td></tr>
            <tr><td class="result-snippet">Docs.</td></tr>"#;
        let r = parse_ddg_lite(html, 5);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].url, "https://tokio.rs/");
        assert_eq!(r[0].title, "Tokio - async runtime");
        assert_eq!(r[0].snippet, "Build reliable network applications.");
        assert_eq!(r[1].url, "https://docs.rs/tokio");
    }

    #[test]
    fn ddg_unwrap_extracts_real_url() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fstd%2F&rut=abc";
        assert_eq!(ddg_unwrap(href), "https://doc.rust-lang.org/std/");
        assert_eq!(ddg_unwrap("//example.com/x"), "https://example.com/x");
        assert_eq!(ddg_unwrap("https://direct.com"), "https://direct.com");
    }

    #[test]
    fn urlencode_basics() {
        assert_eq!(urlencode("rust async"), "rust%20async");
        assert_eq!(urlencode("a+b&c=d"), "a%2Bb%26c%3Dd");
        assert_eq!(urlencode("safe-chars_.~"), "safe-chars_.~");
    }

    #[test]
    fn render_numbers_results() {
        let rs = vec![
            SearchResult { title: "T1".into(), url: "https://a".into(), snippet: "s1".into() },
            SearchResult { title: "T2".into(), url: "https://b".into(), snippet: String::new() },
        ];
        let out = render_results(&rs);
        assert!(out.starts_with("1. T1"));
        assert!(out.contains("2. T2"));
        assert!(out.contains("   s1"));
    }
}
