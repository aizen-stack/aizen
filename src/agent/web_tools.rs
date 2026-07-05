//! Web tools: `web_fetch` (read any URL — platform-aware), `web_search` (web or a specific
//! platform), `web_crawl` (site mapping). Read-only/outward-facing research tools — not
//! approval-gated (like the memory and file_read tools), but they do reach the network.
//!
//! `web_fetch`/`web_search` are thin `Tool` shells over the reach layer (`crate::agent::reach`) —
//! per-platform channels with ordered backend chains and automatic call-time fallback (YouTube
//! transcripts, tweets, GitHub API reads, HN threads, Wikipedia, RSS/Atom, Stack Exchange, and a
//! direct→Jina chain for everything else). `/reach` shows which backend serves each channel.
//!
//! Async-from-sync: `reqwest` is async; the `Tool` trait is sync. We bridge through the shared
//! cancel-aware `tools::block_for_tool`, valid on runtime workers AND on the executor's
//! `spawn_blocking` threads (tokio's `block_in_place` is a verified pass-through there — pinned by
//! `tools::tests::bridge_works_inside_spawn_blocking`). Therefore all declare
//! `is_concurrency_safe() = true`: read-only network fetches are exactly the calls that WIN from
//! running concurrently in a batch.
//!
//! No HTML crate: HTML is reduced to text with a few regexes (strip script/style, drop tags,
//! decode a handful of entities). Best-effort — good enough to feed a page's prose to the model.

use crate::agent::tools::Tool;
use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::time::Duration;

const UA: &str = concat!("aizen/", env!("CARGO_PKG_VERSION"));
/// Cap the characters returned to the model so one page can't blow the context window.
pub(crate) const FETCH_CAP: usize = 20_000;
const REQUEST_TIMEOUT_SECS: u64 = 20;

/// A short-lived HTTP client for the crawler (its own pool; bound to the current runtime when
/// first driven by `block_on`). rustls + webpki-roots reach any public HTTPS host without system
/// certs. Auto-redirects are DISABLED (SSRF: the crawler re-vets every link itself); the reach
/// layer has its own identical builder in `reach::http`.
fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building HTTP client")
}

/// Drive an async future from the sync `Tool::execute` path — the shared cancel-aware bridge
/// (valid on workers AND spawn_blocking threads; Esc aborts the in-flight fetch promptly).
fn block<T>(f: impl std::future::Future<Output = Result<T>>) -> Result<T> {
    crate::agent::tools::block_for_tool(f)
}

// ── web_fetch ────────────────────────────────────────────────────────────────

pub struct WebFetch;
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Fetch an absolute http(s) URL and return its content in the most useful form for that \
         site: YouTube → title + transcript; a tweet/X status → text + stats; GitHub repo/file/\
         tree/issue/PR → API-backed content; Hacker News item → story + top comments; Wikipedia → \
         summary + article; RSS/Atom feeds (incl. the arXiv API) → parsed items; anything else → \
         readable text, automatically falling back to the Jina reader when a page is blocked or \
         JS-only (reddit/linkedin/x profiles go straight there). Backends fall over automatically \
         — see /reach. Not a search engine — to FIND a URL first use web_search. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {"url": {"type": "string", "description": "absolute http(s) URL"}},
            "required": ["url"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        true // read-only network fetch — the concurrent batch is the whole point (see module note)
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let url = args.get("url").and_then(|v| v.as_str()).context("missing required string arg 'url'")?;
        let url = url.to_string();
        // Scheme validation + the SSRF floor live inside `read_url` (single entry for every caller).
        block(crate::agent::reach::route::read_url(&url))
    }
}

// ── web_search ─────────────────────────────────────────────────────────────────

/// Pull the search text out of a `web_search` call, tolerating the aliases models commonly emit
/// instead of `query` (`q`, `question`, `text`, `search`, or a `queries` list). This turns a
/// slightly-malformed call into a working search rather than a hard "missing required string arg".
fn extract_query(args: &Value) -> Option<String> {
    for k in ["query", "q", "question", "text", "search", "queries"] {
        match args.get(k) {
            Some(Value::String(s)) if !s.trim().is_empty() => return Some(s.clone()),
            Some(Value::Array(a)) => {
                let joined = a.iter().filter_map(|x| x.as_str()).collect::<Vec<_>>().join(" ");
                if !joined.trim().is_empty() {
                    return Some(joined);
                }
            }
            _ => {}
        }
    }
    None
}

pub struct WebSearch;
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web (DuckDuckGo chain, keyless) and return the top results as title + URL + \
         snippet. Use to FIND pages relevant to a query, then web_fetch a result URL to read it. \
         Optional 'site' searches one platform's own index instead: github (repositories), \
         hackernews, stackoverflow, wikipedia. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer", "description": "max results (default 5, max 10)"},
                "site": {
                    "type": "string",
                    "enum": ["web", "github", "hackernews", "stackoverflow", "wikipedia"],
                    "description": "where to search (default web)"
                }
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        true // read-only network fetch — the concurrent batch is the whole point (see module note)
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let query = extract_query(args)
            .context("missing required string arg 'query' (the text to search for)")?;
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5).clamp(1, 10) as usize;
        let site = args.get("site").and_then(|v| v.as_str()).map(str::to_string);
        block(crate::agent::reach::route::search(&query, limit, site.as_deref()))
    }
}

// ── web_crawl ────────────────────────────────────────────────────────────────

pub struct WebCrawl;
impl Tool for WebCrawl {
    fn name(&self) -> &str {
        "web_crawl"
    }
    fn description(&self) -> &str {
        "Crawl a website from a seed URL (katana-style): follow links to map its pages/endpoints, \
         extracting from HTML + JS. Use to discover a site's structure or API endpoints. Heavier \
         than web_fetch (many requests) — keep depth small. Read-only, scoped to the seed host."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "url": {"type": "string", "description": "seed http(s) URL"},
                "depth": {"type": "integer", "description": "hops from the seed (default 1, max 3)"},
                "max_pages": {"type": "integer", "description": "max URLs to return (default 40, max 200)"},
                "scope": {"type": "string", "enum": ["strict", "subs"], "description": "strict = same host (default); subs = + subdomains"}
            },
            "required": ["url"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        true // read-only network fetch — the concurrent batch is the whole point (see module note)
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let url = args.get("url").and_then(|v| v.as_str()).context("missing required string arg 'url'")?;
        // SSRF floor on the seed (followed links are vetted per-fetch inside crawl).
        crate::core::net_guard::guard_url(url)?;
        let depth = args.get("depth").and_then(|v| v.as_u64()).unwrap_or(1).clamp(0, 3) as usize;
        let max_pages = args.get("max_pages").and_then(|v| v.as_u64()).unwrap_or(40).clamp(1, 200) as usize;
        let scope = match args.get("scope").and_then(|v| v.as_str()) {
            Some(s) => crate::features::crawl::Scope::parse(s)?,
            None => crate::features::crawl::Scope::Strict,
        };
        let opts = crate::features::crawl::CrawlOptions {
            seeds: vec![url.to_string()],
            max_depth: depth,
            max_pages,
            scope,
            concurrency: 8,
            timeout_secs: REQUEST_TIMEOUT_SECS,
        };
        let report = block(async {
            let c = client()?;
            crate::features::crawl::crawl(&c, &opts).await
        })?;
        if report.found.is_empty() {
            return Ok(format!("(crawl of {url} found no URLs)"));
        }
        let mut s = format!("crawled {} page(s) → {} URL(s):\n", report.pages_fetched, report.found.len());
        for f in &report.found {
            s.push_str(&format!("{} [{}]\n", f.url, f.via.tag()));
        }
        Ok(s.trim_end().to_string())
    }
}

// ── parsing helpers (pure — unit-tested offline; shared with the reach layer) ──

/// Reduce an HTML document to readable text: drop `<script>`/`<style>`, strip tags, decode a few
/// entities, collapse runs of spaces (keeping line breaks).
pub(crate) fn html_to_text(html: &str) -> String {
    // No backreference (`\1`) — the `regex` crate doesn't support them; spell both out.
    static SCRIPT: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?is)<script[^>]*>.*?</\s*script\s*>|<style[^>]*>.*?</\s*style\s*>").unwrap());
    static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<[^>]+>").unwrap());
    static SPACES: Lazy<Regex> = Lazy::new(|| Regex::new(r"[ \t\r\x0c]+").unwrap());
    static NEWLINES: Lazy<Regex> = Lazy::new(|| Regex::new(r"\n{3,}").unwrap());
    let no_script = SCRIPT.replace_all(html, "\n");
    let no_tags = TAG.replace_all(&no_script, " ");
    let decoded = decode_entities(&no_tags);
    let spaced = SPACES.replace_all(&decoded, " ");
    NEWLINES.replace_all(&spaced, "\n\n").trim().to_string()
}

/// Strip tags + decode entities + collapse whitespace in a small HTML fragment (anchor inner text).
pub(crate) fn strip_tags(s: &str) -> String {
    static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<[^>]+>").unwrap());
    decode_entities(&TAG.replace_all(s, "")).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Decode the handful of HTML entities that actually show up in titles/snippets. `&amp;` is
/// decoded LAST so we don't double-decode (`&amp;lt;` → `&lt;`, not `<`).
pub(crate) fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// Minimal application/x-www-form-urlencoded percent-decoder (`%XX` + `+`→space).
pub(crate) fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                (Some(a), Some(b)) => {
                    out.push(a * 16 + b);
                    i += 3;
                }
                _ => {
                    out.push(bytes[i]);
                    i += 1;
                }
            },
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Truncate to `cap` characters (not bytes — never split a multibyte char), with a marker.
pub(crate) fn truncate_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let head: String = s.chars().take(cap).collect();
    format!("{head}\n…[truncated at {cap} chars]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_drops_script_and_tags() {
        let html = "<html><head><style>.x{color:red}</style></head><body><h1>Hi</h1>\
                    <script>alert('x')</script><p>Hello &amp; welcome &lt;3</p></body></html>";
        let text = html_to_text(html);
        assert!(text.contains("Hi"));
        assert!(text.contains("Hello & welcome <3"));
        assert!(!text.contains("alert"), "script body removed");
        assert!(!text.contains("color:red"), "style body removed");
        assert!(!text.contains('<') || text.contains("<3"), "tags stripped (entity-decoded < kept)");
    }

    #[test]
    fn percent_decode_basics() {
        assert_eq!(percent_decode("https%3A%2F%2Fa.com%2Fx"), "https://a.com/x");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("plain"), "plain");
    }

    #[test]
    fn truncate_caps_long_text() {
        let s = "x".repeat(100);
        let t = truncate_chars(&s, 10);
        assert!(t.starts_with(&"x".repeat(10)));
        assert!(t.contains("truncated"));
        assert_eq!(truncate_chars("short", 10), "short");
    }

    #[test]
    fn tools_are_parallel_eligible_read_only() {
        // The shared bridge is valid on spawn_blocking threads → network reads run concurrently.
        assert!(WebFetch.is_concurrency_safe());
        assert!(WebSearch.is_concurrency_safe());
        assert!(WebCrawl.is_concurrency_safe());
        // Read-only research tools — not approval-gated.
        assert!(!WebFetch.is_destructive());
        assert!(!WebSearch.is_destructive());
    }

    #[test]
    fn web_search_schema_offers_platform_sites() {
        let p = WebSearch.parameters();
        let sites = p["properties"]["site"]["enum"].as_array().unwrap();
        for want in ["web", "github", "hackernews", "stackoverflow", "wikipedia"] {
            assert!(sites.iter().any(|s| s == want), "missing site {want}");
        }
    }
}
