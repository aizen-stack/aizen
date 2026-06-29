//! Web tools: `web_search` (no-key DuckDuckGo HTML endpoint) + `web_fetch` (GET a URL → readable
//! text). Both are read-only/outward-facing research tools — not approval-gated (like the memory
//! and file_read tools), but they do reach the network.
//!
//! Async-from-sync: `reqwest` is async; the `Tool` trait is sync. We bridge with `block_in_place`
//! + the CURRENT runtime's `block_on` (same pattern + invariant as `task_tool`). Therefore both
//! declare `is_concurrency_safe() = false`: the parallel scoped-thread path has no Tokio runtime,
//! where `block_in_place`/`Handle::current()` would panic. They stay on the serial path (a runtime
//! worker thread) where the bridge is valid.
//!
//! No HTML crate: HTML is reduced to text with a few regexes (strip script/style, drop tags,
//! decode a handful of entities). Best-effort — good enough to feed a page's prose to the model.

use crate::agent::tools::Tool;
use anyhow::{bail, Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use std::time::Duration;

const UA: &str = concat!("aizen/", env!("CARGO_PKG_VERSION"));
/// Cap the characters returned to the model so one page can't blow the context window.
const FETCH_CAP: usize = 20_000;
const REQUEST_TIMEOUT_SECS: u64 = 20;
/// Max HTTP redirects we follow manually (each one re-vetted by the SSRF floor).
const MAX_REDIRECTS: usize = 5;

/// A short-lived HTTP client for a single web call (its own pool; bound to the current runtime
/// when first driven by `block_on`). rustls + webpki-roots (reqwest's `rustls-tls`) reach any
/// public HTTPS host without system certs.
///
/// SSRF: auto-redirects are DISABLED (`Policy::none`). reqwest would otherwise transparently follow
/// up to 10 redirects, and a public page returning `Location: http://169.254.169.254/…` (cloud
/// metadata) would be fetched WITHOUT passing the `net_guard` floor again. `web_fetch` follows
/// redirects manually instead, re-vetting every hop. (See `fetch_guarded`.)
fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building HTTP client")
}

/// GET `url`, following redirects MANUALLY so the SSRF floor (`net_guard::guard_url_async`) re-vets
/// every hop's target — closing the "public URL → 302 → 169.254.169.254" metadata-exfiltration hole.
/// The caller has already vetted the initial `url`. Returns the final (status, content-type, body).
async fn fetch_guarded(c: &reqwest::Client, url: &str) -> Result<(reqwest::StatusCode, String, String)> {
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let resp = c.get(&url).send().await.with_context(|| format!("GET {url} failed"))?;
        let status = resp.status();
        if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
            let loc = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| anyhow::anyhow!("redirect ({status}) with no usable Location header"))?;
            // Resolve a possibly-relative Location against the current URL, then re-run the SSRF floor.
            let next = reqwest::Url::parse(&url)
                .ok()
                .and_then(|base| base.join(loc).ok())
                .ok_or_else(|| anyhow::anyhow!("bad redirect target '{loc}'"))?
                .to_string();
            crate::core::net_guard::guard_url_async(&next).await?;
            url = next;
            continue;
        }
        let ctype = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        let text = resp.text().await.context("reading response body")?;
        return Ok((status, ctype, text));
    }
    bail!("too many redirects (> {MAX_REDIRECTS})")
}

/// Drive an async future from the sync `Tool::execute` path (see the module note on the invariant).
fn block<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

// ── web_fetch ────────────────────────────────────────────────────────────────

pub struct WebFetch;
impl Tool for WebFetch {
    fn name(&self) -> &str {
        "web_fetch"
    }
    fn description(&self) -> &str {
        "Fetch an absolute http(s) URL and return its content (HTML is reduced to readable text). \
         Use to read a documentation page, API reference, or article. Not a search engine — to \
         FIND a URL first use web_search. Read-only."
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
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let url = args.get("url").and_then(|v| v.as_str()).context("missing required string arg 'url'")?;
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            bail!("url must be an absolute http(s) URL");
        }
        // SSRF floor: refuse loopback / private / link-local (cloud metadata) targets.
        crate::core::net_guard::guard_url(url)?;
        let url = url.to_string();
        let (status, ctype, text) = block(async {
            let c = client()?;
            // Manual redirect-following with a per-hop SSRF re-check (see `fetch_guarded`).
            fetch_guarded(&c, &url).await
        })?;
        let looks_html = ctype.contains("html") || text.trim_start().starts_with('<');
        let content = if looks_html { html_to_text(&text) } else { text };
        Ok(format!("[{} {}]\n{}", status.as_u16(), url, truncate_chars(&content, FETCH_CAP)))
    }
}

// ── web_search ─────────────────────────────────────────────────────────────────

pub struct WebSearch;
impl Tool for WebSearch {
    fn name(&self) -> &str {
        "web_search"
    }
    fn description(&self) -> &str {
        "Search the web (DuckDuckGo) and return the top results as title + URL + snippet. Use to \
         FIND pages relevant to a query, then web_fetch a result URL to read it. Read-only."
    }
    fn parameters(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "limit": {"type": "integer", "description": "max results (default 5, max 10)"}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }
    fn is_concurrency_safe(&self) -> bool {
        false
    }
    fn execute(&self, args: &Value) -> Result<String> {
        let query = args
            .get("query")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .context("missing required string arg 'query'")?
            .to_string();
        let limit = args.get("limit").and_then(|v| v.as_u64()).unwrap_or(5).clamp(1, 10) as usize;

        let html = block(async {
            let c = client()?;
            // The no-JS HTML endpoint — no API key. (POST form is the documented no-script path.)
            let resp = c
                .post("https://html.duckduckgo.com/html/")
                .form(&[("q", query.as_str())])
                .send()
                .await
                .context("DuckDuckGo request failed")?;
            anyhow::Ok(resp.text().await.context("reading search response")?)
        })?;

        let results = parse_ddg(&html, limit);
        if results.is_empty() {
            return Ok(format!("(no results for '{query}')"));
        }
        let mut s = String::new();
        for (i, r) in results.iter().enumerate() {
            s.push_str(&format!("{}. {}\n   {}\n", i + 1, r.title, r.url));
            if !r.snippet.is_empty() {
                s.push_str(&format!("   {}\n", r.snippet));
            }
        }
        Ok(s.trim_end().to_string())
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
        false
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

// ── parsing helpers (pure — unit-tested offline) ────────────────────────────────

struct SearchResult {
    title: String,
    url: String,
    snippet: String,
}

/// Reduce an HTML document to readable text: drop `<script>`/`<style>`, strip tags, decode a few
/// entities, collapse runs of spaces (keeping line breaks).
fn html_to_text(html: &str) -> String {
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
fn strip_tags(s: &str) -> String {
    static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<[^>]+>").unwrap());
    decode_entities(&TAG.replace_all(s, "")).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Decode the handful of HTML entities that actually show up in titles/snippets. `&amp;` is
/// decoded LAST so we don't double-decode (`&amp;lt;` → `&lt;`, not `<`).
fn decode_entities(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
}

/// Parse DuckDuckGo's HTML result list into (title, url, snippet) triples, capped at `limit`.
fn parse_ddg(html: &str, limit: usize) -> Vec<SearchResult> {
    static ANCHOR: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?is)<a[^>]+class="[^"]*result__a[^"]*"[^>]+href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap()
    });
    static SNIPPET: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?is)<a[^>]+class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</a>"#).unwrap()
    });
    let titled: Vec<(String, String)> = ANCHOR
        .captures_iter(html)
        .map(|c| (c[1].to_string(), strip_tags(&c[2])))
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
fn ddg_unwrap(href: &str) -> String {
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

/// Minimal application/x-www-form-urlencoded percent-decoder (`%XX` + `+`→space).
fn percent_decode(s: &str) -> String {
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
fn truncate_chars(s: &str, cap: usize) -> String {
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
    fn ddg_unwrap_extracts_real_url() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fstd%2F&rut=abc";
        assert_eq!(ddg_unwrap(href), "https://doc.rust-lang.org/std/");
        assert_eq!(ddg_unwrap("//example.com/x"), "https://example.com/x");
        assert_eq!(ddg_unwrap("https://direct.com"), "https://direct.com");
    }

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
    fn truncate_caps_long_text() {
        let s = "x".repeat(100);
        let t = truncate_chars(&s, 10);
        assert!(t.starts_with(&"x".repeat(10)));
        assert!(t.contains("truncated"));
        assert_eq!(truncate_chars("short", 10), "short");
    }

    #[test]
    fn tools_are_serial_path_only() {
        // block_in_place would panic on the parallel scoped-thread path → must be false.
        assert!(!WebFetch.is_concurrency_safe());
        assert!(!WebSearch.is_concurrency_safe());
        // Read-only research tools — not approval-gated.
        assert!(!WebFetch.is_destructive());
        assert!(!WebSearch.is_destructive());
    }
}
