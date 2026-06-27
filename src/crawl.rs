//! A katana-style standard web crawler — pure Rust (no headless browser, no passive sources, so
//! the single static binary holds). BFS over HTTP: fetch a page, extract links from HTML
//! (`href`/`src`/`action`) and endpoints from JS (regex over quoted paths/URLs), enqueue the
//! in-scope, unseen ones up to a depth/page cap, repeat.
//!
//! Shared by the `ng crawl <url>` subcommand and the `web_crawl` agent tool. The crawl makes only
//! GET requests; scope is restricted to the seed host (or its root domain) so it can't wander the
//! whole web, and `max_pages` is a hard ceiling.

use anyhow::{bail, Context, Result};
use futures_util::future::join_all;
use once_cell::sync::Lazy;
use regex::Regex;
use std::collections::{HashSet, VecDeque};
use std::time::Duration;
use url::Url;

/// How far off the seed host a discovered URL may be to still get crawled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Same host exactly (e.g. only `www.example.com`).
    Strict,
    /// Same root domain, any subdomain (anything under `example.com`). Root = last two labels
    /// (a heuristic — `example.co.uk` is treated as root `co.uk`; good enough for v1).
    Subdomain,
}

impl Scope {
    pub fn parse(s: &str) -> Result<Scope> {
        match s.trim().to_lowercase().as_str() {
            "strict" | "host" | "fqdn" => Ok(Scope::Strict),
            "subs" | "subdomain" | "domain" | "rdn" => Ok(Scope::Subdomain),
            other => bail!("unknown scope '{other}' (use: strict | subs)"),
        }
    }
}

/// Where a URL was discovered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Seed,
    Html,
    Js,
}

impl Source {
    pub fn tag(&self) -> &'static str {
        match self {
            Source::Seed => "seed",
            Source::Html => "html",
            Source::Js => "js",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Found {
    pub url: String,
    pub depth: usize,
    pub via: Source,
}

pub struct CrawlOptions {
    pub seeds: Vec<String>,
    pub max_depth: usize,
    pub max_pages: usize,
    pub scope: Scope,
    pub concurrency: usize,
    pub timeout_secs: u64,
}

impl Default for CrawlOptions {
    fn default() -> Self {
        Self {
            seeds: Vec::new(),
            max_depth: 2,
            max_pages: 200,
            scope: Scope::Strict,
            concurrency: 10,
            timeout_secs: 15,
        }
    }
}

pub struct CrawlReport {
    /// All discovered in-scope URLs (deduped, sorted).
    pub found: Vec<Found>,
    pub pages_fetched: usize,
}

/// Run the crawl. Best-effort: a page that fails to fetch is skipped (recon shouldn't abort on one
/// dead link). Returns once the frontier drains or `max_pages` discovered URLs is reached.
pub async fn crawl(client: &reqwest::Client, opts: &CrawlOptions) -> Result<CrawlReport> {
    let mut seed_urls = Vec::new();
    for s in &opts.seeds {
        let u = Url::parse(s.trim()).with_context(|| format!("invalid seed URL '{s}'"))?;
        if u.scheme() != "http" && u.scheme() != "https" {
            bail!("seed must be an http(s) URL: {s}");
        }
        // SSRF floor: a private/loopback/link-local seed is refused up front (clear error).
        crate::net_guard::guard_url_async(u.as_str()).await?;
        seed_urls.push(normalize(&u));
    }
    if seed_urls.is_empty() {
        bail!("no seed URLs");
    }
    let scope_hosts: Vec<String> =
        seed_urls.iter().filter_map(|u| u.host_str().map(str::to_string)).collect();

    let mut seen: HashSet<String> = HashSet::new();
    let mut found: Vec<Found> = Vec::new();
    let mut frontier: VecDeque<(Url, usize)> = VecDeque::new();

    for u in &seed_urls {
        let key = u.as_str().to_string();
        if seen.insert(key.clone()) {
            found.push(Found { url: key, depth: 0, via: Source::Seed });
            if opts.max_depth > 0 {
                frontier.push_back((u.clone(), 0));
            }
        }
    }

    let mut pages_fetched = 0usize;
    'outer: while !frontier.is_empty() {
        // Take a wave (bounded by concurrency); every node in the frontier has depth < max_depth.
        let mut wave = Vec::new();
        while wave.len() < opts.concurrency.max(1) {
            match frontier.pop_front() {
                Some(item) => wave.push(item),
                None => break,
            }
        }
        let futs = wave.into_iter().map(|(u, d)| {
            let url = u.clone();
            async move {
                let body = fetch_body(client, &url, opts.timeout_secs).await;
                (url, d, body)
            }
        });
        for (url, depth, body) in join_all(futs).await {
            let (ctype, text) = match body {
                Some(b) => b,
                None => continue,
            };
            pages_fetched += 1;
            for (child, src) in extract_links(&ctype, &text, &url) {
                if found.len() >= opts.max_pages {
                    break 'outer;
                }
                if !in_scope(&child, &scope_hosts, opts.scope) {
                    continue;
                }
                let key = child.as_str().to_string();
                if seen.insert(key.clone()) {
                    let child_depth = depth + 1;
                    found.push(Found { url: key, depth: child_depth, via: src });
                    if child_depth < opts.max_depth {
                        frontier.push_back((child, child_depth));
                    }
                }
            }
        }
    }

    found.sort_by(|a, b| a.url.cmp(&b.url));
    Ok(CrawlReport { found, pages_fetched })
}

/// GET a URL; return `(content_type, body)` or `None` on any error / non-2xx. Only text-ish bodies
/// are worth parsing — a giant binary is dropped via the content-type gate downstream.
async fn fetch_body(client: &reqwest::Client, url: &Url, timeout: u64) -> Option<(String, String)> {
    // SSRF floor: skip any URL that resolves to a private/loopback/link-local address. Best-effort
    // (recon shouldn't abort on one blocked link) — the seed is also vetted by the caller.
    if crate::net_guard::guard_url_async(url.as_str()).await.is_err() {
        return None;
    }
    let resp = client
        .get(url.clone())
        .timeout(Duration::from_secs(timeout))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_lowercase();
    let text = resp.text().await.ok()?;
    Some((ctype, text))
}

/// Decide which links to extract from a fetched body, by content type.
fn extract_links(ctype: &str, body: &str, base: &Url) -> Vec<(Url, Source)> {
    let is_html = ctype.contains("html") || body.trim_start().starts_with('<');
    let is_js = ctype.contains("javascript") || ctype.contains("ecmascript");
    let mut out = Vec::new();
    if is_html {
        for u in extract_html_links(body, base) {
            out.push((u, Source::Html));
        }
        // inline <script> endpoints
        for u in extract_js_endpoints(body, base) {
            out.push((u, Source::Js));
        }
    } else if is_js {
        for u in extract_js_endpoints(body, base) {
            out.push((u, Source::Js));
        }
    }
    out
}

/// Pull `href`/`src`/`action` attribute values out of HTML and resolve them against `base`.
pub fn extract_html_links(html: &str, base: &Url) -> Vec<Url> {
    static ATTR: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?i)(?:href|src|action)\s*=\s*["']([^"'#]+)["']"#).unwrap());
    let mut out = Vec::new();
    for c in ATTR.captures_iter(html) {
        if let Some(u) = resolve(base, &c[1]) {
            out.push(u);
        }
    }
    out
}

/// Pull endpoint-looking strings out of JS: absolute `http(s)://…` or root-relative `"/path…"`.
pub fn extract_js_endpoints(js: &str, base: &Url) -> Vec<Url> {
    static EP: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"["'](https?://[^"'\s]+|/[A-Za-z0-9_\-./][A-Za-z0-9_\-./?=&%]*)["']"#).unwrap()
    });
    let mut out = Vec::new();
    for c in EP.captures_iter(js) {
        let raw = &c[1];
        // Skip obvious non-endpoints: bare "//", protocol-relative double slash w/o host handled by resolve.
        if raw == "/" {
            continue;
        }
        if let Some(u) = resolve(base, raw) {
            out.push(u);
        }
    }
    out
}

/// Resolve a raw link against `base`, keep only http(s), drop the fragment. Skips
/// mailto:/tel:/javascript:/data: and unparseable values.
fn resolve(base: &Url, raw: &str) -> Option<Url> {
    let raw = raw.trim();
    if raw.is_empty()
        || raw.starts_with("mailto:")
        || raw.starts_with("tel:")
        || raw.starts_with("javascript:")
        || raw.starts_with("data:")
    {
        return None;
    }
    let joined = base.join(raw).ok()?;
    if joined.scheme() != "http" && joined.scheme() != "https" {
        return None;
    }
    Some(normalize(&joined))
}

/// Strip the fragment (keep the query); a `#anchor` is the same resource for crawling.
fn normalize(u: &Url) -> Url {
    let mut u = u.clone();
    u.set_fragment(None);
    u
}

/// Root domain = last two dot-labels (heuristic; ignores multi-label eTLDs like `co.uk`).
fn root_domain(host: &str) -> String {
    let labels: Vec<&str> = host.rsplit('.').collect();
    if labels.len() >= 2 {
        format!("{}.{}", labels[1], labels[0])
    } else {
        host.to_string()
    }
}

fn in_scope(u: &Url, scope_hosts: &[String], scope: Scope) -> bool {
    let host = match u.host_str() {
        Some(h) => h,
        None => return false,
    };
    match scope {
        Scope::Strict => scope_hosts.iter().any(|h| h == host),
        Scope::Subdomain => {
            let r = root_domain(host);
            scope_hosts.iter().any(|h| root_domain(h) == r)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/dir/page.html").unwrap()
    }

    #[test]
    fn html_links_resolve_relative_and_absolute() {
        let html = r##"<a href="/about">A</a> <a href="sub/x.html">B</a>
            <link href="https://cdn.example.com/s.css"> <img src="../img/p.png">
            <a href="#frag">skip</a> <a href="mailto:x@y.com">skip</a>"##;
        let urls: Vec<String> = extract_html_links(html, &base()).iter().map(|u| u.as_str().to_string()).collect();
        assert!(urls.contains(&"https://example.com/about".to_string()));
        assert!(urls.contains(&"https://example.com/dir/sub/x.html".to_string()));
        assert!(urls.contains(&"https://cdn.example.com/s.css".to_string()));
        assert!(urls.contains(&"https://example.com/img/p.png".to_string()));
        assert!(!urls.iter().any(|u| u.contains("frag")), "fragment-only link dropped");
        assert!(!urls.iter().any(|u| u.contains("mailto")), "mailto dropped");
    }

    #[test]
    fn js_endpoints_extracted() {
        let js = r#"const a = "/api/v2/users"; fetch("https://api.example.com/data");
            var b = "/"; let c = "notapath"; const d = '/static/app.js';"#;
        let urls: Vec<String> = extract_js_endpoints(js, &base()).iter().map(|u| u.as_str().to_string()).collect();
        assert!(urls.contains(&"https://example.com/api/v2/users".to_string()));
        assert!(urls.contains(&"https://api.example.com/data".to_string()));
        assert!(urls.contains(&"https://example.com/static/app.js".to_string()));
        assert!(!urls.iter().any(|u| u.ends_with("/example.com/") && u.len() < 22), "bare '/' skipped");
        assert!(!urls.iter().any(|u| u.contains("notapath")), "non-path word skipped");
    }

    #[test]
    fn scope_strict_vs_subdomain() {
        let hosts = vec!["example.com".to_string()];
        let same = Url::parse("https://example.com/x").unwrap();
        let sub = Url::parse("https://api.example.com/x").unwrap();
        let other = Url::parse("https://evil.org/x").unwrap();
        assert!(in_scope(&same, &hosts, Scope::Strict));
        assert!(!in_scope(&sub, &hosts, Scope::Strict), "subdomain out of strict scope");
        assert!(in_scope(&sub, &hosts, Scope::Subdomain), "subdomain in subdomain scope");
        assert!(!in_scope(&other, &hosts, Scope::Subdomain), "other domain always out");
    }

    #[test]
    fn root_domain_takes_last_two_labels() {
        assert_eq!(root_domain("a.b.example.com"), "example.com");
        assert_eq!(root_domain("example.com"), "example.com");
        assert_eq!(root_domain("localhost"), "localhost");
    }

    #[test]
    fn normalize_strips_fragment_keeps_query() {
        let u = Url::parse("https://x.com/a?b=1#frag").unwrap();
        assert_eq!(normalize(&u).as_str(), "https://x.com/a?b=1");
    }

    #[test]
    fn scope_parse_accepts_aliases() {
        assert_eq!(Scope::parse("strict").unwrap(), Scope::Strict);
        assert_eq!(Scope::parse("subs").unwrap(), Scope::Subdomain);
        assert_eq!(Scope::parse("domain").unwrap(), Scope::Subdomain);
        assert!(Scope::parse("bogus").is_err());
    }
}
