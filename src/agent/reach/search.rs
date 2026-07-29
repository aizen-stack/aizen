//! The `search` channel — keyed web search with an automatic backend chain, plus per-platform
//! search for the sites that expose real APIs (GitHub / Hacker News / Stack Overflow / Wikipedia).
//!
//! Web chain: `tavily` (POST https://api.tavily.com/search — a keyed JSON search API built for
//! agents; the primary backend) → `jina-search` (s.jina.ai — also keyed; a secondary fallback).
//! BOTH backends need a key. DuckDuckGo scraping (html + lite) and the keyless Marginalia floor
//! were REMOVED: DDG's anomaly wall (HTTP 202, empty page) blocked keyless scraping too often to be
//! a dependable primary, and Marginalia's volunteer index was too slow/flaky (60s+ timeouts) to
//! stand alone. With NO key configured, `web_search` returns an actionable "add a Tavily key" error
//! rather than silently degrading to an unreliable scrape.

use super::http;
use anyhow::{bail, Context, Result};

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
    // Every search backend is now keyed. Track whether ANY backend even had a key to try, so a
    // no-key config yields an actionable "add a key" message instead of an empty "all failed".
    let mut any_attempted = false;

    for backend in super::ordered_backends("search") {
        let outcome = match backend {
            "tavily" => match super::tavily_key() {
                Some(k) => {
                    any_attempted = true;
                    tavily(&c, query, limit, &k).await
                }
                None => continue, // keyed backend — silently absent when no key (chain-level error covers it)
            },
            "jina-search" => match super::jina_key() {
                Some(k) => {
                    any_attempted = true;
                    jina_search(&c, query, limit, &k).await
                }
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
    if !any_attempted {
        // No key was configured for ANY search backend, so nothing was even tried. Tell the user how
        // to make search work rather than emitting a misleading "all backends failed".
        bail!(
            "web search needs an API key — none is configured. Set a Tavily key (get one free at \
             https://app.tavily.com) via `AIZEN_TAVILY_API_KEY=<key>`, the `TAVILY_API_KEY` env var, \
             or `reach.tavily_api_key` in the config. (A Jina key via `JINA_API_KEY` also works as a \
             fallback search backend.)"
        );
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
        return Ok(format!(
            "(no results for any of: {})",
            uniq.iter()
                .map(|q| format!("'{}'", q.trim()))
                .collect::<Vec<_>>()
                .join(", ")
        ));
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
    let mut cursors: Vec<std::vec::IntoIter<SearchResult>> =
        lists.into_iter().map(|l| l.into_iter()).collect();
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
    let mut host_counts: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
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
            out.push(SearchResult {
                title: r.title.clone(),
                url: r.url.clone(),
                snippet: r.snippet.clone(),
            });
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
            let host = u
                .host_str()
                .unwrap_or("")
                .trim_start_matches("www.")
                .to_ascii_lowercase();
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
        .and_then(|u| {
            u.host_str()
                .map(|h| h.trim_start_matches("www.").to_ascii_lowercase())
        })
        .unwrap_or_default()
}

// ── Tavily (keyed JSON — the primary search backend) ──────────────────────────

/// Tavily's search API — a keyed JSON search endpoint built for agents (get a free key at
/// https://app.tavily.com). This is the primary backend: DuckDuckGo scraping was dropped because its
/// anomaly wall (HTTP 202) blocked keyless requests too often to depend on. POST JSON to
/// `https://api.tavily.com/search` with a bearer key; response shape:
/// `{ "results": [ { "title", "url", "content", "score" }, … ] }`.
async fn tavily(
    c: &reqwest::Client,
    query: &str,
    limit: usize,
    key: &str,
) -> Result<Vec<SearchResult>> {
    let body = serde_json::json!({
        "query": query,
        "max_results": limit,
        "search_depth": "basic",
    });
    let headers = [("Authorization", format!("Bearer {key}"))];
    let f = http::post_json(c, "https://api.tavily.com/search", &headers, &body).await?;
    if f.status == 401 || f.status == 403 {
        bail!(
            "Tavily rejected the key (HTTP {}) — check AIZEN_TAVILY_API_KEY / TAVILY_API_KEY",
            f.status
        );
    }
    if f.status == 429 {
        bail!("Tavily rate limit (HTTP 429) — the free tier's monthly quota may be exhausted");
    }
    if !f.is_success() {
        bail!(
            "Tavily returned HTTP {}: {}",
            f.status,
            http::snippet(&f.text())
        );
    }
    let v: serde_json::Value =
        serde_json::from_slice(&f.body).context("parsing Tavily JSON response")?;
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
                snippet: it["content"].as_str().unwrap_or("").to_string(),
            })
        })
        .collect())
}

/// Verdict of checking a CANDIDATE key — one the user just pasted, which is not in the config or the
/// environment yet. That's why this can't reuse `probe_tavily`/`probe_jina_search`: those read the
/// SAVED key, so during setup they'd validate the old key (or none) instead of the new one.
///
/// The split matters because the two failures need different things from the user: a rejected key
/// means re-paste it, while an unreachable API means the key might be perfectly good and only the
/// network is at fault — re-asking for it there would be blaming them for our own connectivity.
#[derive(Debug)]
pub(crate) enum KeyCheck {
    /// The provider accepted the key. Carries the result count so the UI can prove it really searched.
    Ok(usize),
    /// The provider answered and said no (401/403, or a 429 that means this key is out of quota).
    Rejected(String),
    /// No usable answer: DNS, TLS, timeout, or an unexpected status. The key is unproven, not wrong.
    Unreachable(String),
}

/// Validate a candidate Tavily key with one real (minimal) search.
///
/// Classified by STATUS rather than by matching on `tavily()`'s error strings: those strings are
/// user-facing prose, and a validator that greps them would silently start reporting "unreachable"
/// the day someone rewords an error message.
pub(crate) async fn check_tavily_key(key: &str) -> KeyCheck {
    let c = match http::client() {
        Ok(c) => c,
        Err(e) => return KeyCheck::Unreachable(e.to_string()),
    };
    let body = serde_json::json!({ "query": "aizen key check", "max_results": 1 });
    let headers = [("Authorization", format!("Bearer {key}"))];
    let f = match http::post_json(&c, "https://api.tavily.com/search", &headers, &body).await {
        Ok(f) => f,
        Err(e) => return KeyCheck::Unreachable(http::snippet(&e.to_string())),
    };
    match f.status {
        // 429 is grouped with auth on PURPOSE: an exhausted free-tier key cannot serve a search, so
        // telling the user "looks fine" would be a lie they'd discover on their first real query.
        401 | 403 => KeyCheck::Rejected(format!("Tavily rejected the key (HTTP {})", f.status)),
        429 => KeyCheck::Rejected(
            "Tavily rate limit (HTTP 429) — the key is valid but its quota is exhausted".into(),
        ),
        s if (200..300).contains(&s) => {
            let n = serde_json::from_slice::<serde_json::Value>(&f.body)
                .ok()
                .and_then(|v| v["results"].as_array().map(|a| a.len()))
                .unwrap_or(0);
            KeyCheck::Ok(n)
        }
        s => KeyCheck::Unreachable(format!("HTTP {s}: {}", http::snippet(&f.text()))),
    }
}

/// Validate a candidate Jina key. Same classification rules as [`check_tavily_key`].
pub(crate) async fn check_jina_key(key: &str) -> KeyCheck {
    let c = match http::client() {
        Ok(c) => c,
        Err(e) => return KeyCheck::Unreachable(e.to_string()),
    };
    let url = format!("https://s.jina.ai/?q={}", urlencode("aizen key check"));
    let headers = [
        ("Authorization", format!("Bearer {key}")),
        ("Accept", "application/json".to_string()),
        ("X-Respond-With", "no-content".to_string()),
    ];
    let f = match http::get(&c, &url, &headers).await {
        Ok(f) => f,
        Err(e) => return KeyCheck::Unreachable(http::snippet(&e.to_string())),
    };
    match f.status {
        401 | 403 => KeyCheck::Rejected(format!("Jina rejected the key (HTTP {})", f.status)),
        429 => KeyCheck::Rejected(
            "Jina rate limit (HTTP 429) — the key is valid but its quota is exhausted".into(),
        ),
        s if (200..300).contains(&s) => {
            let n = serde_json::from_slice::<serde_json::Value>(&f.body)
                .ok()
                .and_then(|v| v["data"].as_array().map(|a| a.len()))
                .unwrap_or(0);
            KeyCheck::Ok(n)
        }
        s => KeyCheck::Unreachable(format!("HTTP {s}: {}", http::snippet(&f.text()))),
    }
}

pub(crate) async fn probe_tavily() -> super::Probe {
    let Some(key) = super::tavily_key() else {
        return super::Probe::Off(
            "needs a (free) Tavily key — set AIZEN_TAVILY_API_KEY or TAVILY_API_KEY".into(),
        );
    };
    let c = match http::client() {
        Ok(c) => c,
        Err(e) => return super::Probe::Fail(e.to_string()),
    };
    match tavily(&c, "rust", 1, &key).await {
        Ok(r) if !r.is_empty() => super::Probe::Ok("keyed search OK".into()),
        Ok(_) => super::Probe::Warn("responded but returned 0 results".into()),
        Err(e) => super::Probe::Fail(http::snippet(&e.to_string())),
    }
}

// ── Jina search (keyed only) ────────────────────────────────────────────────

async fn jina_search(
    c: &reqwest::Client,
    query: &str,
    limit: usize,
    key: &str,
) -> Result<Vec<SearchResult>> {
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
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push_str("%20"),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ── doctor probes ───────────────────────────────────────────────────────────

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
    let url = format!(
        "https://hn.algolia.com/api/v1/search?query={}&hitsPerPage={limit}",
        urlencode(query)
    );
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
                crate::agent::web_tools::decode_entities(
                    q["title"].as_str().unwrap_or("(untitled)")
                ),
                q["score"].as_i64().unwrap_or(0),
                if q["is_answered"].as_bool().unwrap_or(false) {
                    "answered"
                } else {
                    "unanswered"
                }
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
    let titles = v
        .get(1)
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();
    let urls = v
        .get(3)
        .and_then(|u| u.as_array())
        .cloned()
        .unwrap_or_default();
    if titles.is_empty() {
        return Ok(format!("(no Wikipedia articles match '{query}')"));
    }
    let results: Vec<SearchResult> = titles
        .iter()
        .zip(
            urls.iter()
                .chain(std::iter::repeat(&serde_json::Value::Null)),
        )
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
    fn tavily_json_shape_parses() {
        // The primary backend's response shape: results[].{title,url,content}. Entries with an
        // empty url are dropped; `content` maps to our snippet field.
        let v: serde_json::Value = serde_json::from_str(
            r#"{"results":[{"title":"Tokio","url":"https://tokio.rs/","content":"An async runtime","score":0.9},
                            {"title":"dropped (empty url)","url":"","content":"x"}]}"#,
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
                    snippet: it["content"].as_str().unwrap_or("").to_string(),
                })
            })
            .collect();
        assert_eq!(parsed.len(), 1, "the empty-url entry must be dropped");
        assert_eq!(parsed[0].url, "https://tokio.rs/");
        assert_eq!(parsed[0].snippet, "An async runtime");
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
            SearchResult {
                title: "T1".into(),
                url: "https://a".into(),
                snippet: "s1".into(),
            },
            SearchResult {
                title: "T2".into(),
                url: "https://b".into(),
                snippet: String::new(),
            },
        ];
        let out = render_results(&rs);
        assert!(out.starts_with("1. T1"));
        assert!(out.contains("2. T2"));
        assert!(out.contains("   s1"));
    }

    // ── W20: fan-out interleave ──────────────────────────────────────────────────

    #[test]
    fn interleave_round_robins_across_queries() {
        fn r(u: &str) -> SearchResult {
            SearchResult {
                title: u.into(),
                url: u.into(),
                snippet: String::new(),
            }
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
            SearchResult {
                title: u.into(),
                url: u.into(),
                snippet: String::new(),
            }
        }
        let results = vec![
            r("https://a.com/x"),
            r("https://a.com/x"),
            r("https://a.com/x/"),
            r("https://b.com/y"),
        ];
        let out = dedup_and_diversify(results, 10);
        // "https://a.com/x" and "https://a.com/x/" canonicalize the same (trailing slash dropped).
        assert_eq!(
            out.len(),
            2,
            "exact + trailing-slash duplicates collapse to one"
        );
    }

    #[test]
    fn dedup_and_diversify_caps_per_host_when_alternatives_exist() {
        fn r(u: &str) -> SearchResult {
            SearchResult {
                title: u.into(),
                url: u.into(),
                snippet: String::new(),
            }
        }
        // 4 from the same host, 4 from distinct hosts — asking for 6 should prefer spreading across
        // hosts rather than returning 6 from the one dominant host.
        let mut results = vec![
            r("https://a.com/1"),
            r("https://a.com/2"),
            r("https://a.com/3"),
            r("https://a.com/4"),
        ];
        for h in ["b", "c", "d", "e"] {
            results.push(r(&format!("https://{h}.com/1")));
        }
        let out = dedup_and_diversify(results, 6);
        assert_eq!(out.len(), 6);
        let a_count = out.iter().filter(|r| r.url.contains("a.com")).count();
        assert!(
            a_count <= 3,
            "per-host cap (limit/2) should curb a.com's dominance, got {a_count}"
        );
        let distinct_hosts: std::collections::HashSet<&str> = out
            .iter()
            .filter_map(|r| r.url.split("://").nth(1)?.split('/').next())
            .collect();
        assert!(
            distinct_hosts.len() >= 4,
            "diversity should span multiple hosts, got {distinct_hosts:?}"
        );
    }

    #[test]
    fn dedup_and_diversify_relaxes_cap_to_fill_when_no_alternatives() {
        fn r(u: &str) -> SearchResult {
            SearchResult {
                title: u.into(),
                url: u.into(),
                snippet: String::new(),
            }
        }
        // All 5 results are the SAME host — the per-host cap must relax rather than return fewer
        // than requested when there's nothing else to diversify with.
        let results: Vec<_> = (1..=5).map(|i| r(&format!("https://a.com/{i}"))).collect();
        let out = dedup_and_diversify(results, 5);
        assert_eq!(
            out.len(),
            5,
            "same-host fill pass must not starve the result count"
        );
    }

    #[test]
    fn canonical_url_normalizes_www_trailing_slash_and_query() {
        assert_eq!(
            canonical_url("https://www.example.com/path/"),
            canonical_url("https://example.com/path")
        );
        assert_eq!(
            canonical_url("https://example.com/path?utm=x"),
            canonical_url("https://example.com/path")
        );
    }
}
