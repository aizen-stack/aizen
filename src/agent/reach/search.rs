//! The `search` channel — keyless web search with an automatic backend chain, plus per-platform
//! search for the sites that expose real APIs (GitHub / Hacker News / Stack Overflow / Wikipedia).
//!
//! Web chain: `ddg-html` (POST https://html.duckduckgo.com/html/) → `ddg-lite`
//! (https://lite.duckduckgo.com/lite/, lighter markup, same operator) → `marginalia`
//! (api.marginalia.nu/public/search — keyless JSON, an INDEPENDENT index, so it both survives a DDG
//! outage and gives a real second-source cross-check) → `jina-search` (s.jina.ai — needs a key;
//! skipped keyless). DDG's anomaly wall answers HTTP 202 with an empty page instead of an error —
//! that is treated as "blocked, fall through", and DDG calls are paced ≥2 s apart to stay under the
//! wall. (Live-verified 2026-07-05: the aizen UA is accepted by DDG; only curl-shaped UAs are
//! blocked.) Marginalia is deliberately placed AFTER both DDG endpoints, not before: it's a small
//! volunteer-run service that is sometimes slow/504s under load (observed: sub-second on a good
//! call, 60s+ timeout on a bad one) — fine as a third-choice fallback that never delays the common
//! case, wrong as a primary. Paced ≥1 s as its own politeness floor.

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
    let results = web_results(query, limit).await?;
    if results.is_empty() {
        return Ok(format!("(no results for '{query}')"));
    }
    Ok(render_results(&results))
}

/// The backend-chain core behind [`web`] and the fan-out [`web_multi`]. Returns the first backend's
/// non-empty results; an EMPTY (but non-error) `Ok(vec![])` means every backend that answered found
/// genuinely nothing (the caller renders "(no results)"). `Err` means every backend errored (or was
/// blocked) — never a silent empty, so a broken chain is visible, not mistaken for "no results".
pub(crate) async fn web_results(query: &str, limit: usize) -> Result<Vec<SearchResult>> {
    let c = http::client()?;
    let mut failures: Vec<String> = Vec::new();
    let mut any_answered_empty = false;

    for backend in super::ordered_backends("search") {
        let outcome = match backend {
            "ddg-html" => ddg_html(&c, query, limit).await,
            "ddg-lite" => ddg_lite(&c, query, limit).await,
            "marginalia" => marginalia(&c, query, limit).await,
            "jina-search" => match super::jina_key() {
                Some(k) => jina_search(&c, query, limit, &k).await,
                None => continue, // optional keyed backend — silently absent keyless
            },
            _ => continue,
        };
        match outcome {
            Ok(results) if !results.is_empty() => {
                super::note_ok("search", backend);
                return Ok(results);
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
        return Ok(Vec::new());
    }
    bail!("all search backends failed — {}", failures.join("; "))
}

/// Fan-out search (W20): run 2–3 DIFFERENT-angle queries concurrently through the same backend
/// chain, then merge with [`dedup_and_diversify`] (W21) so the model gets multi-angle coverage in
/// ONE round-trip instead of serial single queries. Order within each query's results is preserved
/// (relevance), and the merge interleaves queries so no single query monopolizes the head. A query
/// that errors is dropped (not fatal) — the union still returns; only an all-empty/all-failed set
/// yields "(no results)".
pub(crate) async fn web_multi(queries: &[String], limit: usize) -> Result<String> {
    // De-dup identical queries and cap the fan-out — three angles is plenty, and it bounds the
    // pace() cost against the shared "ddg"/"marginalia" politeness floors.
    let mut uniq: Vec<&String> = Vec::new();
    for q in queries {
        let t = q.trim();
        if !t.is_empty() && !uniq.iter().any(|u| u.trim().eq_ignore_ascii_case(t)) {
            uniq.push(q);
        }
        if uniq.len() >= 3 {
            break;
        }
    }
    if uniq.is_empty() {
        bail!("no non-empty query in the fan-out set");
    }
    if uniq.len() == 1 {
        return web(uniq[0], limit).await;
    }
    // Each query fetches a little deeper than the final limit so the merge has material to dedup
    // and diversify from without starving any single angle.
    let per_query = (limit + 2).min(10);
    let futs = uniq.iter().map(|q| web_results(q, per_query));
    let outcomes = futures_util::future::join_all(futs).await;

    let mut errors: Vec<String> = Vec::new();
    let mut per_query_lists: Vec<Vec<SearchResult>> = Vec::new();
    for (q, r) in uniq.iter().zip(outcomes) {
        match r {
            Ok(list) => per_query_lists.push(list),
            Err(e) => errors.push(format!("'{}': {}", q.trim(), http::snippet(&e.to_string()))),
        }
    }
    if per_query_lists.iter().all(|l| l.is_empty()) {
        if !errors.is_empty() && per_query_lists.is_empty() {
            bail!("all fan-out queries failed — {}", errors.join("; "));
        }
        return Ok(format!("(no results for any of: {})", uniq.iter().map(|q| format!("'{}'", q.trim())).collect::<Vec<_>>().join(", ")));
    }
    let merged = interleave(per_query_lists);
    let diversified = dedup_and_diversify(merged, limit);
    Ok(render_results(&diversified))
}

/// Round-robin interleave of each query's ranked results — take the #1 of every query, then the #2
/// of every query, … — so every angle contributes to the head rather than the first query filling
/// it. Preserves each list's internal (relevance) order.
fn interleave(lists: Vec<Vec<SearchResult>>) -> Vec<SearchResult> {
    let max_len = lists.iter().map(|l| l.len()).max().unwrap_or(0);
    let mut cursors: Vec<std::vec::IntoIter<SearchResult>> = lists.into_iter().map(|l| l.into_iter()).collect();
    let mut out = Vec::new();
    for _ in 0..max_len {
        for cur in cursors.iter_mut() {
            if let Some(r) = cur.next() {
                out.push(r);
            }
        }
    }
    out
}

/// Dedup by canonical URL, then cap results-per-registrable-host so one domain can't crowd out
/// coverage (W21). Input order is authoritative (already relevance/interleave-ranked); we keep the
/// FIRST occurrence of each URL and skip a host once it hits the per-host cap, until `limit` is met.
pub(crate) fn dedup_and_diversify(results: Vec<SearchResult>, limit: usize) -> Vec<SearchResult> {
    // Per-host cap scales with the limit but is never punitive on a small ask: at most half the
    // results from a single host (min 1), so a 2-result request can still return two same-host hits
    // when that's all there is, while a 10-result request spreads across ≥2 hosts.
    let per_host_cap = (limit / 2).max(1);
    let mut seen_urls: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut host_counts: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut out = Vec::with_capacity(limit);
    // Two passes: first honoring the per-host cap (diversity), then a fill pass that relaxes the cap
    // if we're still short (better to return same-host extras than fewer results than asked).
    for relax in [false, true] {
        for r in &results {
            if out.len() >= limit {
                break;
            }
            let canon = canonical_url(&r.url);
            if canon.is_empty() || seen_urls.contains(&canon) {
                continue;
            }
            let host = host_of(&r.url);
            let count = host_counts.get(&host).copied().unwrap_or(0);
            if !relax && count >= per_host_cap {
                continue;
            }
            seen_urls.insert(canon);
            *host_counts.entry(host).or_insert(0) += 1;
            out.push(SearchResult { title: r.title.clone(), url: r.url.clone(), snippet: r.snippet.clone() });
        }
        if out.len() >= limit {
            break;
        }
    }
    out
}

/// Canonical form of a URL for dedup: lowercased scheme+host, path without a trailing slash, query
/// dropped (tracking params make the same page look distinct), fragment dropped. Falls back to a
/// trimmed lowercase string if it won't parse.
fn canonical_url(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(u) => {
            let host = u.host_str().unwrap_or("").trim_start_matches("www.").to_ascii_lowercase();
            let path = u.path().trim_end_matches('/');
            format!("{}://{}{}", u.scheme().to_ascii_lowercase(), host, path)
        }
        Err(_) => url.trim().trim_end_matches('/').to_ascii_lowercase(),
    }
}

/// Registrable-ish host for the per-domain cap (strips a leading `www.`; lowercased). Not a full
/// public-suffix parse — good enough to keep `docs.rs` and `tokio.rs` distinct while folding
/// `www.example.com`/`example.com` together.
fn host_of(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.trim_start_matches("www.").to_ascii_lowercase()))
        .unwrap_or_default()
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
    let body = f.text();
    let results = parse_ddg(&body, limit);
    if results.is_empty() && looks_like_broken_parse(&body, "result__a") {
        bail!("DuckDuckGo returned result markup but 0 parsed (markup drift?) — falling through");
    }
    Ok(results)
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
    let body = f.text();
    let results = parse_ddg_lite(&body, limit);
    if results.is_empty() && looks_like_broken_parse(&body, "result-link") {
        bail!("DuckDuckGo lite returned result markup but 0 parsed (markup drift?) — falling through");
    }
    Ok(results)
}

/// Distinguish "the engine genuinely found nothing" (a real empty page → answer `(no results)`)
/// from "our regex broke against a markup change" (the result-anchor CSS token is still all over
/// the page, yet we extracted zero → the parser drifted, W19). Only the latter should fall through
/// to the next backend; the former is a truthful empty. Heuristic: the class token appears at least
/// twice in a body that parsed to nothing.
fn looks_like_broken_parse(html: &str, result_class_token: &str) -> bool {
    html.matches(result_class_token).count() >= 2
}

/// Pair each title anchor with the snippet that FOLLOWS it in document order — the snippet whose
/// byte offset falls between this title and the next. Index-zipping title[i]↔snippet[i] (the old
/// approach) silently mis-shifts every later row when one result lacks a snippet (W18); positional
/// binding tolerates a missing snippet on any result and simply leaves that one blank.
///
/// `titles` and `snippets` are (byte_offset, text) pairs from separate regex sweeps; both are in
/// ascending offset order (`captures_iter` yields left-to-right), so the pairing is a linear merge.
fn bind_titles_to_snippets(
    titles: Vec<(usize, String, String)>, // (offset, href, title)
    snippets: Vec<(usize, String)>,       // (offset, snippet)
    limit: usize,
) -> Vec<SearchResult> {
    let mut out = Vec::with_capacity(titles.len().min(limit));
    for (i, (t_off, href, title)) in titles.iter().enumerate() {
        if out.len() >= limit {
            break;
        }
        // The next title's offset bounds this result's region (∞ for the last title).
        let next_off = titles.get(i + 1).map(|(o, _, _)| *o).unwrap_or(usize::MAX);
        let snippet = snippets
            .iter()
            .find(|(s_off, _)| *s_off > *t_off && *s_off < next_off)
            .map(|(_, s)| s.clone())
            .unwrap_or_default();
        out.push(SearchResult { url: ddg_unwrap(href), title: title.clone(), snippet });
    }
    out
}

/// Parse DuckDuckGo's html-endpoint result list into (title, url, snippet) triples, binding each
/// snippet to the title it follows (positional, not index-zipped — see [`bind_titles_to_snippets`]).
pub(crate) fn parse_ddg(html: &str, limit: usize) -> Vec<SearchResult> {
    static ANCHOR: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?is)<a[^>]+class="[^"]*result__a[^"]*"[^>]+href="([^"]+)"[^>]*>(.*?)</a>"#).unwrap()
    });
    static SNIPPET: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?is)<a[^>]+class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</a>"#).unwrap()
    });
    let titles: Vec<(usize, String, String)> = ANCHOR
        .captures_iter(html)
        .map(|c| {
            let m = c.get(0).unwrap();
            (m.start(), c[1].to_string(), strip_tags(&c[2]))
        })
        .collect();
    let snippets: Vec<(usize, String)> =
        SNIPPET.captures_iter(html).map(|c| (c.get(0).unwrap().start(), strip_tags(&c[1]))).collect();
    bind_titles_to_snippets(titles, snippets, limit)
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
    let titles: Vec<(usize, String, String)> = ANCHOR
        .captures_iter(html)
        .filter_map(|c| {
            let href = HREF.captures(&c[1]).map(|h| h[1].to_string())?;
            Some((c.get(0).unwrap().start(), href, strip_tags(&c[2])))
        })
        .collect();
    let snippets: Vec<(usize, String)> =
        SNIPPET.captures_iter(html).map(|c| (c.get(0).unwrap().start(), strip_tags(&c[1]))).collect();
    bind_titles_to_snippets(titles, snippets, limit)
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

// ── Marginalia (keyless JSON, independent index) ──────────────────────────────

/// Marginalia's public search API — keyless JSON, a small INDEPENDENT crawler/index (not a DDG/Bing
/// reseller), so it doubles as a second-source cross-check and as a survivor when DDG's wall is up.
/// Paced ≥1 s as a politeness floor (it's a volunteer-run service). Shape (live-verified 2026-07-05):
/// `{ "results": [ { "url", "title", "description" }, … ] }`.
async fn marginalia(c: &reqwest::Client, query: &str, limit: usize) -> Result<Vec<SearchResult>> {
    super::pace("marginalia", Duration::from_secs(1)).await;
    let url = format!("https://api.marginalia.nu/public/search/{}?count={limit}", urlencode(query));
    let v = http::get_json(c, &url, &[]).await?;
    let items = v["results"].as_array().cloned().unwrap_or_default();
    Ok(items
        .iter()
        .take(limit)
        .filter_map(|it| {
            let url = it["url"].as_str().unwrap_or("").to_string();
            if url.is_empty() {
                return None;
            }
            Some(SearchResult {
                title: it["title"].as_str().unwrap_or("(untitled)").to_string(),
                url,
                snippet: it["description"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect())
}

pub(crate) async fn probe_marginalia() -> super::Probe {
    let c = match http::client() {
        Ok(c) => c,
        Err(e) => return super::Probe::Fail(e.to_string()),
    };
    match marginalia(&c, "rust", 3).await {
        Ok(r) if !r.is_empty() => super::Probe::Ok(format!("keyless independent index OK ({} results)", r.len())),
        Ok(_) => super::Probe::Warn("responded but returned 0 results (index gap or drift?)".into()),
        Err(e) => super::Probe::Fail(http::snippet(&e.to_string())),
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

    // ── W18: positional (not index-zipped) title↔snippet binding ───────────────

    #[test]
    fn parse_ddg_missing_snippet_does_not_shift_later_rows() {
        // Result 1 has NO snippet at all. Index-zipping (the old bug) would then assign result 2's
        // snippet to result 1, and result 3 would end up with none — every row after the gap shifts.
        // Positional binding must leave result 1 blank and keep 2/3 correctly paired.
        let html = r#"
            <div class="result">
              <a class="result__a" href="https://a.com/">Result A (no snippet)</a>
            </div>
            <div class="result">
              <a class="result__a" href="https://b.com/">Result B</a>
              <a class="result__snippet">Snippet for B.</a>
            </div>
            <div class="result">
              <a class="result__a" href="https://c.com/">Result C</a>
              <a class="result__snippet">Snippet for C.</a>
            </div>"#;
        let r = parse_ddg(html, 5);
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].url, "https://a.com/");
        assert_eq!(r[0].snippet, "", "no snippet in this result's block");
        assert_eq!(r[1].url, "https://b.com/");
        assert_eq!(r[1].snippet, "Snippet for B.", "must not receive C's snippet nor stay empty");
        assert_eq!(r[2].url, "https://c.com/");
        assert_eq!(r[2].snippet, "Snippet for C.");
    }

    #[test]
    fn parse_ddg_lite_missing_snippet_does_not_shift_later_rows() {
        let html = r#"
            <tr><td><a href="https://a.com/" class="result-link">Result A</a></td></tr>
            <tr><td><a href="https://b.com/" class="result-link">Result B</a></td></tr>
            <tr><td class="result-snippet">Snippet for B.</td></tr>"#;
        let r = parse_ddg_lite(html, 5);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].snippet, "", "A has no snippet block");
        assert_eq!(r[1].snippet, "Snippet for B.", "B's own snippet, not misattributed");
    }

    // ── W19: parser-break vs genuine-empty ──────────────────────────────────────

    #[test]
    fn broken_parse_detected_when_class_present_but_unparsed() {
        // The class token is all over the page (so it's not a truly empty results page) yet our
        // anchor regex (deliberately not matched here) extracts nothing — that's markup drift.
        let drifted = r#"<span class="result__a-token">x</span><span class="result__a-token">y</span>"#;
        assert!(looks_like_broken_parse(drifted, "result__a-token"));
    }

    #[test]
    fn broken_parse_not_flagged_on_genuinely_empty_page() {
        let empty = "<html><body>No results found for your search.</body></html>";
        assert!(!looks_like_broken_parse(empty, "result__a"));
    }

    // ── Marginalia (keyless JSON backend) ───────────────────────────────────────

    #[test]
    fn marginalia_json_shape_parses() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"results":[{"url":"https://tokio.rs/","title":"Tokio","description":"An async runtime"},
                            {"url":"","title":"dropped (empty url)","description":"x"}]}"#,
        )
        .unwrap();
        let items = v["results"].as_array().cloned().unwrap();
        let parsed: Vec<SearchResult> = items
            .iter()
            .filter_map(|it| {
                let url = it["url"].as_str().unwrap_or("").to_string();
                if url.is_empty() {
                    return None;
                }
                Some(SearchResult {
                    title: it["title"].as_str().unwrap_or("(untitled)").to_string(),
                    url,
                    snippet: it["description"].as_str().unwrap_or("").to_string(),
                })
            })
            .collect();
        assert_eq!(parsed.len(), 1, "the empty-url entry must be dropped");
        assert_eq!(parsed[0].url, "https://tokio.rs/");
        assert_eq!(parsed[0].snippet, "An async runtime");
    }

    // ── W20: fan-out interleave ──────────────────────────────────────────────────

    #[test]
    fn interleave_round_robins_across_queries() {
        fn r(u: &str) -> SearchResult {
            SearchResult { title: u.into(), url: u.into(), snippet: String::new() }
        }
        let a = vec![r("a1"), r("a2")];
        let b = vec![r("b1")];
        let c = vec![r("c1"), r("c2"), r("c3")];
        let merged = interleave(vec![a, b, c]);
        let urls: Vec<&str> = merged.iter().map(|r| r.url.as_str()).collect();
        // Round 0: a1,b1,c1 — round 1: a2,(b exhausted),c2 — round 2: c3.
        assert_eq!(urls, vec!["a1", "b1", "c1", "a2", "c2", "c3"]);
    }

    // ── W21: dedup + domain diversity ───────────────────────────────────────────

    #[test]
    fn dedup_and_diversify_drops_duplicate_urls() {
        fn r(u: &str) -> SearchResult {
            SearchResult { title: u.into(), url: u.into(), snippet: String::new() }
        }
        let results = vec![r("https://a.com/x"), r("https://a.com/x"), r("https://a.com/x/"), r("https://b.com/y")];
        let out = dedup_and_diversify(results, 10);
        // "https://a.com/x" and "https://a.com/x/" canonicalize the same (trailing slash dropped).
        assert_eq!(out.len(), 2, "exact + trailing-slash duplicates collapse to one");
    }

    #[test]
    fn dedup_and_diversify_caps_per_host_when_alternatives_exist() {
        fn r(u: &str) -> SearchResult {
            SearchResult { title: u.into(), url: u.into(), snippet: String::new() }
        }
        // 4 from the same host, 4 from distinct hosts — asking for 6 should prefer spreading across
        // hosts rather than returning 6 from the one dominant host.
        let mut results = vec![r("https://a.com/1"), r("https://a.com/2"), r("https://a.com/3"), r("https://a.com/4")];
        for h in ["b", "c", "d", "e"] {
            results.push(r(&format!("https://{h}.com/1")));
        }
        let out = dedup_and_diversify(results, 6);
        assert_eq!(out.len(), 6);
        let a_count = out.iter().filter(|r| r.url.contains("a.com")).count();
        assert!(a_count <= 3, "per-host cap (limit/2) should curb a.com's dominance, got {a_count}");
        let distinct_hosts: std::collections::HashSet<&str> =
            out.iter().filter_map(|r| r.url.split("://").nth(1)?.split('/').next()).collect();
        assert!(distinct_hosts.len() >= 4, "diversity should span multiple hosts, got {distinct_hosts:?}");
    }

    #[test]
    fn dedup_and_diversify_relaxes_cap_to_fill_when_no_alternatives() {
        fn r(u: &str) -> SearchResult {
            SearchResult { title: u.into(), url: u.into(), snippet: String::new() }
        }
        // All 5 results are the SAME host — the per-host cap must relax rather than return fewer
        // than requested when there's nothing else to diversify with.
        let results: Vec<_> = (1..=5).map(|i| r(&format!("https://a.com/{i}"))).collect();
        let out = dedup_and_diversify(results, 5);
        assert_eq!(out.len(), 5, "same-host fill pass must not starve the result count");
    }

    #[test]
    fn canonical_url_normalizes_www_trailing_slash_and_query() {
        assert_eq!(canonical_url("https://www.example.com/path/"), canonical_url("https://example.com/path"));
        assert_eq!(canonical_url("https://example.com/path?utm=x"), canonical_url("https://example.com/path"));
    }
}
