//! URL → channel dispatch: the single entry points `read_url` / `search` that `web_fetch` /
//! `web_search` delegate to. Every model-supplied URL passes the SSRF floor HERE, once, before any
//! backend sees it; platform channels then talk to their own fixed API hosts.
//!
//! Resilience rule: when an entire platform channel fails (all its backends), the plain web chain
//! (direct → jina) gets a last shot — so a broken scraping surface degrades to "worse output",
//! never to "no output".

use super::{feed, github, hn, stackex, twitter, web, wikipedia, youtube};
use anyhow::{bail, Result};

#[derive(Debug)]
pub(crate) enum Target {
    YouTube(String),
    Tweet(String),
    GitHub(github::GhTarget),
    HnItem(u64),
    HnFront,
    Wikipedia { lang: String, title: String },
    Feed,
    StackEx { site: String, qid: u64 },
    Web,
}

pub(crate) fn classify(url: &reqwest::Url) -> Target {
    if let Some(vid) = youtube::video_id(url) {
        return Target::YouTube(vid);
    }
    if let Some(id) = twitter::tweet_id(url) {
        return Target::Tweet(id);
    }
    match github::classify(url) {
        github::GhTarget::Other => {}
        t => return Target::GitHub(t),
    }
    if let Some(id) = hn::item_id(url) {
        return Target::HnItem(id);
    }
    if hn::is_front_page(url) {
        return Target::HnFront;
    }
    if let Some((lang, title)) = wikipedia::article(url) {
        return Target::Wikipedia { lang, title };
    }
    if let Some((site, qid)) = stackex::question(url) {
        return Target::StackEx { site, qid };
    }
    if feed::looks_like_feed_url(url) {
        return Target::Feed;
    }
    Target::Web
}

/// Read any URL through the best channel for it. This is `web_fetch`'s engine.
pub async fn read_url(raw: &str) -> Result<String> {
    // Case-insensitive: URL schemes are case-insensitive per RFC 3986 (Url::parse normalizes them).
    let scheme_ok = {
        let head: String = raw.chars().take(8).collect::<String>().to_ascii_lowercase();
        head.starts_with("http://") || head.starts_with("https://")
    };
    if !scheme_ok {
        bail!("url must be an absolute http(s) URL");
    }
    // SSRF floor — refuse loopback / private / link-local (cloud metadata) targets. Redirect hops
    // and the Jina-wrapped target are re-vetted downstream; fixed API hosts are public by nature.
    crate::core::net_guard::guard_url_async(raw).await?;
    let url = reqwest::Url::parse(raw).map_err(|e| anyhow::anyhow!("invalid URL: {e}"))?;

    let (channel, outcome) = match classify(&url) {
        Target::YouTube(vid) => ("youtube", youtube::read(&vid).await),
        Target::Tweet(id) => ("twitter", twitter::read(&id).await),
        Target::GitHub(t) => ("github", github::read(&url, t).await),
        Target::HnItem(id) => ("hackernews", hn::read_item(id).await),
        Target::HnFront => ("hackernews", hn::read_front_page().await),
        Target::Wikipedia { lang, title } => ("wikipedia", wikipedia::read(&url, &lang, &title).await),
        Target::StackEx { site, qid } => ("stackexchange", stackex::read(&url, &site, qid).await),
        Target::Feed => ("feed", feed::read(raw).await),
        Target::Web => return web::read(raw).await,
    };
    match outcome {
        Ok(s) => Ok(s),
        Err(e) => {
            // Whole-channel failure → last-resort plain web read, with the cause kept visible.
            match web::read(raw).await {
                Ok(s) => Ok(format!("(note: {channel} channel failed — {}; fell back to a plain page read)\n{s}", super::http::snippet(&e.to_string()))),
                Err(_) => Err(e), // the channel error is the more specific diagnosis
            }
        }
    }
}

/// Fold a `site` alias to its canonical form (`gh`→`github`, `hn`→`hackernews`, …), so two calls
/// that hit the SAME handler share one cache entry instead of caching separately per spelling.
/// Unknown/empty stays as-is (the caller's match falls through to its error/default arm).
fn canonical_site(site_key: &str) -> &'static str {
    match site_key {
        "" | "web" => "web",
        "github" | "gh" => "github",
        "hackernews" | "hn" => "hackernews",
        "stackoverflow" | "so" => "stackoverflow",
        "wikipedia" | "wiki" => "wikipedia",
        _ => "unknown",
    }
}

/// Search the web or a specific platform. This is `web_search`'s engine. Successful results are
/// cached in-process for a short TTL (W23) so a repeated query — a retry, a sub-agent re-asking —
/// skips both the network and the `pace` politeness floor.
pub async fn search(query: &str, limit: usize, site: Option<&str>) -> Result<String> {
    let site_key = site.map(|s| s.trim().to_ascii_lowercase()).unwrap_or_default();
    // Canonicalize aliases (`gh`→`github`, …) BEFORE the cache key so `site="github"` and
    // `site="gh"` — which hit the exact same handler — share one cache entry instead of two.
    let canon_site = canonical_site(&site_key);
    let cache_key = format!("s|{canon_site}|{limit}|{}", query.trim().to_ascii_lowercase());
    if let Some(hit) = super::cache_get(&cache_key) {
        return Ok(hit);
    }
    let out = match canon_site {
        "web" => super::search::web(query, limit).await,
        "github" => super::search::github(query, limit).await,
        "hackernews" => super::search::hackernews(query, limit).await,
        "stackoverflow" => super::search::stackoverflow(query, limit).await,
        "wikipedia" => super::search::wikipedia(query, limit).await,
        _ => bail!("unknown search site '{site_key}' (use web, github, hackernews, stackoverflow, or wikipedia)"),
    }?;
    // Cache real answers only — never a "(no results)" placeholder (it might be a transient block
    // that a later retry should get a fresh shot at).
    if !out.starts_with("(no results") && !out.starts_with("(no ") {
        super::cache_put(cache_key, out.clone());
    }
    Ok(out)
}

/// Fan-out search over 2–3 different-angle queries (W20). Only the generic web channel fans out —
/// the per-platform indexes (github/hn/…) are single-index and cheap to call directly, so a
/// multi-query against them degrades to the first query. Merges + dedups + diversifies the union.
pub async fn search_multi(queries: &[String], limit: usize, site: Option<&str>) -> Result<String> {
    match site.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        None | Some("") | Some("web") => {
            // Cache the whole fan-out under the ordered, normalized query set (its own dedup happens
            // inside web_multi; the key just needs to be stable for identical fan-out calls).
            let mut norm: Vec<String> = queries.iter().map(|q| q.trim().to_ascii_lowercase()).filter(|q| !q.is_empty()).collect();
            norm.sort();
            norm.dedup(); // identical queries collapse — web_multi dedups them anyway, so the
                          // cache key should too (["rust","rust"] and ["rust"] → one entry).
            let cache_key = format!("m||{limit}|{}", norm.join("\u{1f}"));
            if let Some(hit) = super::cache_get(&cache_key) {
                return Ok(hit);
            }
            let out = super::search::web_multi(queries, limit).await?;
            if !out.starts_with("(no results") {
                super::cache_put(cache_key, out.clone());
            }
            Ok(out)
        }
        // A platform index has one query surface; fan-out there is just the first query (which
        // `search` caches on its own).
        _ => {
            let first = queries.iter().find(|q| !q.trim().is_empty());
            match first {
                Some(q) => search(q, limit, site).await,
                None => bail!("no non-empty query provided"),
            }
        }
    }
}

/// Live end-to-end checks against the real internet — `cargo test --bin aizen reach::route::itest -- --ignored`.
/// Kept `#[ignore]` so the normal suite stays offline/deterministic (same pattern as `lsp::itest`).
#[cfg(test)]
mod itest {
    use super::*;

    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn reach_end_to_end() {
        // YouTube: metadata + transcript through InnerTube.
        let yt = read_url("https://www.youtube.com/watch?v=dQw4w9WgXcQ").await.expect("youtube read");
        assert!(yt.contains("[youtube dQw4w9WgXcQ]"), "{yt}");
        assert!(yt.to_ascii_lowercase().contains("transcript") || yt.contains("no caption"), "{yt}");

        // Tweet 20 via fxtwitter/syndication.
        let tw = read_url("https://x.com/jack/status/20").await.expect("tweet read");
        assert!(tw.contains("just setting up my twttr"), "{tw}");

        // GitHub blob → raw (unmetered, no API quota).
        let gh = read_url("https://github.com/rust-lang/rust/blob/master/README.md").await.expect("github read");
        assert!(gh.contains("[github raw rust-lang/rust/master/README.md]"), "{gh}");
        assert!(gh.to_ascii_lowercase().contains("rust"), "{gh}");

        // Hacker News item 8863 (the 2007 Dropbox announcement) with comments.
        let hn = read_url("https://news.ycombinator.com/item?id=8863").await.expect("hn read");
        assert!(hn.contains("[hackernews 8863]"), "{hn}");
        assert!(hn.contains("▸"), "comments flattened: {hn}");

        // Wikipedia summary + article.
        let wp = read_url("https://en.wikipedia.org/wiki/Rust_(programming_language)").await.expect("wikipedia read");
        assert!(wp.contains("[wikipedia en]"), "{wp}");
        assert!(wp.contains("full article"), "{wp}");

        // arXiv API rides the feed channel.
        let ax = read_url("https://export.arxiv.org/api/query?search_query=all:rust&max_results=2").await.expect("arxiv read");
        assert!(ax.contains("[feed "), "{ax}");
        assert!(ax.contains("•"), "{ax}");

        // Stack Overflow question + answers through the API (id 11227809 = the famous branch-prediction question).
        let so = read_url("https://stackoverflow.com/questions/11227809/why-is-processing-a-sorted-array-faster")
            .await
            .expect("stackoverflow read");
        assert!(so.contains("[stackoverflow q11227809]") || so.contains("[2"), "api or html fallback: {so}");

        // Plain web read still works (and search).
        let web = read_url("https://example.com").await.expect("web read");
        assert!(web.contains("Example Domain"), "{web}");
        // Web search is now KEYED-ONLY (Tavily → Jina). With a key configured it must return
        // results; with NO key the chain returns an actionable "needs an API key" error rather
        // than degrading to an unreliable scrape — both are correct, so only an unexpected error
        // fails the run.
        match search("rust tokio runtime", 5, None).await {
            Ok(s) => assert!(s.contains("1. "), "{s}"),
            Err(e) if e.to_string().contains("needs an API key") => {
                eprintln!("(web search skipped: no Tavily/Jina key configured — {e})");
            }
            Err(e) => panic!("web search: {e}"),
        }
        let gs = search("async runtime", 5, Some("github")).await.expect("github search");
        assert!(gs.contains("1. "), "{gs}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Target {
        classify(&reqwest::Url::parse(s).unwrap())
    }

    #[test]
    fn canonical_site_folds_aliases() {
        assert_eq!(canonical_site("gh"), canonical_site("github"));
        assert_eq!(canonical_site("hn"), canonical_site("hackernews"));
        assert_eq!(canonical_site("so"), canonical_site("stackoverflow"));
        assert_eq!(canonical_site("wiki"), canonical_site("wikipedia"));
        assert_eq!(canonical_site(""), "web");
        assert_eq!(canonical_site("bogus"), "unknown");
    }

    #[test]
    fn classifies_platform_urls() {
        assert!(matches!(t("https://www.youtube.com/watch?v=dQw4w9WgXcQ"), Target::YouTube(id) if id == "dQw4w9WgXcQ"));
        assert!(matches!(t("https://x.com/jack/status/20"), Target::Tweet(id) if id == "20"));
        assert!(matches!(t("https://github.com/rust-lang/rust"), Target::GitHub(_)));
        assert!(matches!(t("https://raw.githubusercontent.com/o/r/main/f.rs"), Target::GitHub(_)));
        assert!(matches!(t("https://news.ycombinator.com/item?id=8863"), Target::HnItem(8863)));
        assert!(matches!(t("https://news.ycombinator.com/"), Target::HnFront));
        assert!(matches!(t("https://en.wikipedia.org/wiki/Rust_(programming_language)"), Target::Wikipedia { .. }));
        assert!(matches!(t("https://stackoverflow.com/questions/12345/x"), Target::StackEx { qid: 12345, .. }));
        assert!(matches!(t("https://blog.rust-lang.org/feed.xml"), Target::Feed));
        assert!(matches!(t("https://export.arxiv.org/api/query?search_query=all:rust"), Target::Feed));
        // Everything else is the generic web (including the platforms' non-item pages).
        assert!(matches!(t("https://example.com/article"), Target::Web));
        assert!(matches!(t("https://www.youtube.com/@SomeChannel"), Target::Web));
        assert!(matches!(t("https://x.com/jack"), Target::Web), "profile pages ride the web/jina chain");
        assert!(matches!(t("https://github.com/rust-lang/rust/actions"), Target::Web));
        assert!(matches!(t("https://www.reddit.com/r/rust/"), Target::Web), "reddit is web(jina-first)");
    }
}
