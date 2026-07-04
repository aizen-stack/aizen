//! The `hackernews` channel — story + comment-tree reads for news.ycombinator.com URLs.
//!
//! Backend `algolia` (hn.algolia.com/api/v1, 10k req/h, whole thread in ONE call) →
//! backend `firebase` (hacker-news.firebaseio.com/v0, official, one item per request — the
//! degraded fallback returns the story without its comment tree). Both keyless; the most reliable
//! pair in the whole reach layer.

use super::http;
use crate::agent::web_tools::{html_to_text, truncate_chars};
use anyhow::{bail, Result};

const MAX_COMMENTS: usize = 15;
const COMMENT_CAP: usize = 1_200;

/// Extract an item id from `news.ycombinator.com/item?id=N`.
pub(crate) fn item_id(url: &reqwest::Url) -> Option<u64> {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    if host != "news.ycombinator.com" {
        return None;
    }
    if url.path() != "/item" {
        return None;
    }
    url.query_pairs().find(|(k, _)| k == "id").and_then(|(_, v)| v.parse().ok())
}

/// Is this the HN front page (or another listing page we render as the top stories)?
pub(crate) fn is_front_page(url: &reqwest::Url) -> bool {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    host == "news.ycombinator.com" && matches!(url.path(), "/" | "" | "/news" | "/front")
}

pub(crate) async fn read_item(id: u64) -> Result<String> {
    let c = http::client()?;
    match read_algolia(&c, id).await {
        Ok(s) => Ok(s),
        Err(e) => {
            super::note_err("hackernews", "algolia", &e.to_string());
            let s = read_firebase(&c, id).await?;
            super::note_ok("hackernews", "firebase");
            Ok(format!("{s}\n(comment tree unavailable — Algolia failed: {})", http::snippet(&e.to_string())))
        }
    }
}

pub(crate) async fn read_front_page() -> Result<String> {
    let c = http::client()?;
    let v = http::get_json(&c, "https://hn.algolia.com/api/v1/search?tags=front_page&hitsPerPage=30", &[]).await?;
    super::note_ok("hackernews", "algolia");
    let hits = v["hits"].as_array().cloned().unwrap_or_default();
    let mut s = String::from("[hackernews front page]\n");
    for (i, h) in hits.iter().enumerate() {
        s.push_str(&format!(
            "{}. {} ({} points, {} comments) https://news.ycombinator.com/item?id={}\n",
            i + 1,
            h["title"].as_str().unwrap_or("(untitled)"),
            h["points"].as_u64().unwrap_or(0),
            h["num_comments"].as_u64().unwrap_or(0),
            h["objectID"].as_str().unwrap_or("?"),
        ));
    }
    Ok(s.trim_end().to_string())
}

async fn read_algolia(c: &reqwest::Client, id: u64) -> Result<String> {
    let v = http::get_json(c, &format!("https://hn.algolia.com/api/v1/items/{id}"), &[]).await?;
    if v["id"].is_null() {
        bail!("HN item {id} not found");
    }
    super::note_ok("hackernews", "algolia");
    Ok(render_item(&v))
}

/// Render an Algolia item: header + story text + the comment tree flattened depth-first, indented,
/// capped at MAX_COMMENTS.
pub(crate) fn render_item(v: &serde_json::Value) -> String {
    let mut s = format!(
        "[hackernews {}] {} ({} points) by {} — {}\n",
        v["id"].as_u64().unwrap_or(0),
        v["title"].as_str().unwrap_or("(comment)"),
        v["points"].as_u64().unwrap_or(0),
        v["author"].as_str().unwrap_or("?"),
        v["created_at"].as_str().unwrap_or("?"),
    );
    if let Some(url) = v["url"].as_str() {
        s.push_str(&format!("link: {url}\n"));
    }
    if let Some(text) = v["text"].as_str() {
        s.push_str(&format!("{}\n", truncate_chars(&html_to_text(text), 4_000)));
    }
    let mut count = 0usize;
    let mut total = 0usize;
    collect_comments(&v["children"], 0, &mut count, &mut total, &mut s);
    if total > count {
        s.push_str(&format!("…and {} more comments\n", total - count));
    }
    s.trim_end().to_string()
}

fn collect_comments(children: &serde_json::Value, depth: usize, shown: &mut usize, total: &mut usize, out: &mut String) {
    // Depth cap: real threads run tens deep at most; a hostile payload nesting thousands deep
    // must not blow the stack.
    if depth > 50 {
        return;
    }
    let Some(list) = children.as_array() else { return };
    for c in list {
        if c["text"].is_null() && c["children"].as_array().map(|a| a.is_empty()).unwrap_or(true) {
            continue; // deleted/dead leaf
        }
        *total += 1;
        if *shown < MAX_COMMENTS {
            *shown += 1;
            let indent = "  ".repeat(depth.min(6));
            let text = truncate_chars(&html_to_text(c["text"].as_str().unwrap_or("")), COMMENT_CAP);
            out.push_str(&format!("{indent}▸ {}: {}\n", c["author"].as_str().unwrap_or("?"), text.replace('\n', " ")));
        }
        collect_comments(&c["children"], depth + 1, shown, total, out);
    }
}

async fn read_firebase(c: &reqwest::Client, id: u64) -> Result<String> {
    let v = http::get_json(c, &format!("https://hacker-news.firebaseio.com/v0/item/{id}.json"), &[]).await?;
    if v.is_null() {
        bail!("HN item {id} not found");
    }
    let mut s = format!(
        "[hackernews {}] {} ({} points) by {} — {} comments\n",
        id,
        v["title"].as_str().unwrap_or("(comment)"),
        v["score"].as_u64().unwrap_or(0),
        v["by"].as_str().unwrap_or("?"),
        v["descendants"].as_u64().unwrap_or(0),
    );
    if let Some(url) = v["url"].as_str() {
        s.push_str(&format!("link: {url}\n"));
    }
    if let Some(text) = v["text"].as_str() {
        s.push_str(&format!("{}\n", truncate_chars(&html_to_text(text), 4_000)));
    }
    Ok(s.trim_end().to_string())
}

// ── doctor probes (item 8863 = the 2007 "My YC app: Dropbox" story — stable forever) ──

pub(crate) async fn probe_algolia() -> super::Probe {
    let c = match http::client() {
        Ok(c) => c,
        Err(e) => return super::Probe::Fail(e.to_string()),
    };
    match http::get_json(&c, "https://hn.algolia.com/api/v1/items/8863", &[]).await {
        Ok(v) if v["id"].as_u64() == Some(8863) => super::Probe::Ok("stories + full comment trees OK".into()),
        Ok(_) => super::Probe::Fail("unexpected payload".into()),
        Err(e) => super::Probe::Fail(http::snippet(&e.to_string())),
    }
}

pub(crate) async fn probe_firebase() -> super::Probe {
    let c = match http::client() {
        Ok(c) => c,
        Err(e) => return super::Probe::Fail(e.to_string()),
    };
    match http::get_json(&c, "https://hacker-news.firebaseio.com/v0/item/8863.json", &[]).await {
        Ok(v) if v["id"].as_u64() == Some(8863) => super::Probe::Ok("official API OK (items only, no trees)".into()),
        Ok(_) => super::Probe::Fail("unexpected payload".into()),
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
    fn extracts_item_ids_and_front_page() {
        assert_eq!(item_id(&u("https://news.ycombinator.com/item?id=8863")), Some(8863));
        assert_eq!(item_id(&u("https://news.ycombinator.com/item?id=abc")), None);
        assert_eq!(item_id(&u("https://news.ycombinator.com/user?id=pg")), None);
        assert!(is_front_page(&u("https://news.ycombinator.com/")));
        assert!(is_front_page(&u("https://news.ycombinator.com/news")));
        assert!(!is_front_page(&u("https://news.ycombinator.com/item?id=1")));
    }

    #[test]
    fn renders_item_with_capped_flattened_comments() {
        let v = serde_json::json!({
            "id": 1, "title": "T", "points": 10, "author": "pg", "created_at": "2007",
            "url": "https://example.com",
            "children": [
                {"author": "a", "text": "<p>first &amp; foremost</p>", "children": [
                    {"author": "b", "text": "reply", "children": []}
                ]},
                {"author": null, "text": null, "children": []},
            ],
        });
        let s = render_item(&v);
        assert!(s.contains("[hackernews 1] T (10 points) by pg"));
        assert!(s.contains("link: https://example.com"));
        assert!(s.contains("▸ a: first & foremost"));
        assert!(s.contains("  ▸ b: reply"), "nested comment indented");
        assert!(!s.contains("null"));
    }

    #[test]
    fn comment_cap_reports_remainder() {
        let many: Vec<serde_json::Value> = (0..25)
            .map(|i| serde_json::json!({"author": format!("u{i}"), "text": "hi", "children": []}))
            .collect();
        let v = serde_json::json!({"id": 1, "title": "T", "points": 1, "author": "x", "created_at": "", "children": many});
        let s = render_item(&v);
        assert!(s.contains("…and 10 more comments"), "{s}");
    }
}
