//! The `feed` channel — RSS 2.0 / Atom parsing with a built-in best-effort parser (house style:
//! regex reduction like `html_to_text`, no XML crate — feed-rs would drag in ammonia/html5ever).
//!
//! Detection is two-stage: URL shape (`.rss`/`.atom`/`.xml`/`/feed`/known feed hosts like the
//! arXiv export API) routes here directly, and the web channel calls `sniff()` on fetched bodies
//! so a feed served from any URL still renders as a feed. arXiv's API
//! (`export.arxiv.org/api/query`) returns Atom, so paper search rides this channel for free
//! (paced 3 s per arXiv's Terms of Use).

use super::http;
use crate::agent::web_tools::{decode_entities, strip_tags, truncate_chars};
use anyhow::{bail, Result};
use once_cell::sync::Lazy;
use regex::Regex;
use std::time::Duration;

const MAX_ITEMS: usize = 20;
const ITEM_SUMMARY_CAP: usize = 700;

/// Does this URL look like a feed? (Routing hint — content sniffing is the authority.)
pub(crate) fn looks_like_feed_url(url: &reqwest::Url) -> bool {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    if host == "export.arxiv.org" {
        return true;
    }
    let path = url.path().to_ascii_lowercase();
    path.ends_with(".rss")
        || path.ends_with(".atom")
        || path.ends_with("/feed")
        || path.ends_with("/feed/")
        || path.ends_with("/rss")
        || path.ends_with("/rss/")
        || path.ends_with("feed.xml")
        || path.ends_with("rss.xml")
        || path.ends_with("atom.xml")
        || path.ends_with("index.xml")
}

/// Does this BODY look like a feed? Cheap sniff over the first chunk (UTF-8 BOM tolerated —
/// plenty of real feeds ship one).
pub(crate) fn sniff(body: &str) -> bool {
    let head: String = body
        .trim_start_matches('\u{feff}')
        .trim_start()
        .chars()
        .take(600)
        .collect::<String>()
        .to_ascii_lowercase();
    (head.starts_with("<?xml") || head.starts_with("<rss") || head.starts_with("<feed"))
        && (head.contains("<rss") || head.contains("<feed") || head.contains("<rdf:rdf"))
}

pub(crate) async fn read(url: &str) -> Result<String> {
    let c = http::client()?;
    if url.contains("export.arxiv.org") {
        super::pace("arxiv", Duration::from_secs(3)).await; // arXiv ToU: ≤1 request per 3 s
    }
    let f = http::get(&c, url, &[]).await?;
    if !f.is_success() {
        super::note_err("feed", "builtin", &format!("HTTP {}", f.status));
        bail!("HTTP {} fetching feed {url}", f.status);
    }
    let body = f.text();
    if !sniff(&body) {
        bail!("{url} does not look like an RSS/Atom feed");
    }
    let out = render(&body, url)?;
    super::note_ok("feed", "builtin");
    Ok(out)
}

pub(crate) struct FeedItem {
    pub title: String,
    pub link: String,
    pub date: String,
    pub summary: String,
}

/// Parse + render a feed body.
pub(crate) fn render(xml: &str, url: &str) -> Result<String> {
    let (feed_title, items) = parse(xml);
    if items.is_empty() {
        bail!("no items found in feed {url} (unsupported dialect?)");
    }
    let shown = items.len().min(MAX_ITEMS);
    let mut s = format!(
        "[feed {url}] {} — {} item(s){}:\n",
        feed_title,
        items.len(),
        if items.len() > shown {
            format!(" (showing {shown})")
        } else {
            String::new()
        }
    );
    for it in items.iter().take(MAX_ITEMS) {
        s.push_str(&format!("\n• {}", it.title));
        if !it.date.is_empty() {
            s.push_str(&format!("  ({})", it.date));
        }
        s.push('\n');
        if !it.link.is_empty() {
            s.push_str(&format!("  {}\n", it.link));
        }
        if !it.summary.is_empty() {
            s.push_str(&format!("  {}\n", it.summary));
        }
    }
    Ok(s.trim_end().to_string())
}

/// Best-effort RSS2/Atom/RDF item extraction. Regexes over well-formed-enough XML — the same
/// tradeoff as `html_to_text` (good enough to feed prose to the model, not a validator).
pub(crate) fn parse(xml: &str) -> (String, Vec<FeedItem>) {
    static ITEM: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?is)<(?:item|entry)\b[^>]*>(.*?)</(?:item|entry)>").unwrap());
    static TITLE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?is)<title[^>]*>(.*?)</title>").unwrap());
    static LINK_HREF: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?is)<link[^>]*href\s*=\s*"([^"]+)"[^>]*/?>"#).unwrap());
    static LINK_TEXT: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"(?is)<link[^>]*>([^<]+)</link>").unwrap());
    static DATE: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r"(?is)<(?:pubDate|updated|published|dc:date)[^>]*>(.*?)</(?:pubDate|updated|published|dc:date)>").unwrap()
    });
    static SUMMARY: Lazy<Regex> = Lazy::new(|| {
        Regex::new(
            r"(?is)<(?:description|summary|content)\b[^>]*>(.*?)</(?:description|summary|content)>",
        )
        .unwrap()
    });

    // Feed-level title = the first <title> BEFORE the first item block.
    let first_item = ITEM.find(xml).map(|m| m.start()).unwrap_or(xml.len());
    let feed_title = TITLE
        .captures(&xml[..first_item])
        .map(|c| clean_text(&c[1]))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "(untitled feed)".to_string());

    let items = ITEM
        .captures_iter(xml)
        .map(|c| {
            let block = &c[1];
            let title = TITLE
                .captures(block)
                .map(|c| clean_text(&c[1]))
                .unwrap_or_else(|| "(untitled)".into());
            // Atom prefers the alternate/href link; RSS uses text content.
            let link = LINK_HREF
                .captures_iter(block)
                .map(|c| c[1].to_string())
                .find(|_| true)
                .or_else(|| LINK_TEXT.captures(block).map(|c| c[1].trim().to_string()))
                .unwrap_or_default();
            let date = DATE
                .captures(block)
                .map(|c| clean_text(&c[1]))
                .unwrap_or_default();
            let summary = SUMMARY
                .captures(block)
                .map(|c| truncate_chars(&clean_text(&c[1]), ITEM_SUMMARY_CAP))
                .unwrap_or_default();
            FeedItem {
                title,
                link,
                date,
                summary,
            }
        })
        .collect();
    (feed_title, items)
}

/// CDATA unwrap → entity decode → tag strip → whitespace collapse.
fn clean_text(s: &str) -> String {
    let s = s.trim();
    let s = s
        .strip_prefix("<![CDATA[")
        .and_then(|x| x.strip_suffix("]]>"))
        .unwrap_or(s);
    // Double-decode handles feeds that ship entity-encoded HTML in descriptions (`&lt;p&gt;…`).
    let decoded = decode_entities(&decode_entities(s));
    strip_tags(&decoded)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSS: &str = r#"<?xml version="1.0"?>
    <rss version="2.0"><channel>
      <title>Example Blog</title>
      <link>https://blog.example.com</link>
      <item>
        <title><![CDATA[First <b>post</b>]]></title>
        <link>https://blog.example.com/1</link>
        <pubDate>Mon, 01 Jul 2026 10:00:00 GMT</pubDate>
        <description>&lt;p&gt;Hello &amp;amp; welcome&lt;/p&gt;</description>
      </item>
      <item>
        <title>Second post</title>
        <link>https://blog.example.com/2</link>
      </item>
    </channel></rss>"#;

    const ATOM: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
    <feed xmlns="http://www.w3.org/2005/Atom">
      <title>ArXiv Query: search_query=all:rust</title>
      <entry>
        <title>A Paper About Rust</title>
        <link href="http://arxiv.org/abs/2401.00001v1" rel="alternate"/>
        <published>2026-01-01T00:00:00Z</published>
        <summary>  We study memory safety.  </summary>
      </entry>
    </feed>"#;

    #[test]
    fn parses_rss2() {
        let (title, items) = parse(RSS);
        assert_eq!(title, "Example Blog");
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].title, "First post",
            "CDATA unwrapped, inner tags stripped"
        );
        assert_eq!(items[0].link, "https://blog.example.com/1");
        assert!(items[0].date.contains("2026"));
        assert_eq!(
            items[0].summary, "Hello & welcome",
            "entity-encoded HTML decoded then stripped"
        );
        assert_eq!(items[1].summary, "");
    }

    #[test]
    fn parses_atom_arxiv_shape() {
        let (title, items) = parse(ATOM);
        assert!(title.contains("ArXiv"));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "A Paper About Rust");
        assert_eq!(items[0].link, "http://arxiv.org/abs/2401.00001v1");
        assert_eq!(items[0].summary, "We study memory safety.");
    }

    #[test]
    fn feed_title_never_steals_the_first_item_title() {
        let xml = r#"<feed><entry><title>Only Item</title></entry></feed>"#;
        let (title, items) = parse(xml);
        assert_eq!(title, "(untitled feed)");
        assert_eq!(items[0].title, "Only Item");
    }

    #[test]
    fn sniffs_feeds_not_html() {
        assert!(sniff(RSS));
        assert!(sniff(ATOM));
        assert!(sniff(&format!("\u{feff}{RSS}")), "UTF-8 BOM tolerated");
        assert!(!sniff("<!DOCTYPE html><html><head></head></html>"));
        assert!(!sniff("plain text"));
        // XML that is not a feed (e.g. an SVG) is not sniffed as one.
        assert!(!sniff(
            r#"<?xml version="1.0"?><svg xmlns="http://www.w3.org/2000/svg"></svg>"#
        ));
    }

    #[test]
    fn url_shapes_route_to_feed() {
        for s in [
            "https://blog.rust-lang.org/feed.xml",
            "https://example.com/rss",
            "https://example.com/blog/atom.xml",
            "https://example.com/feed/",
            "https://export.arxiv.org/api/query?search_query=all:rust",
        ] {
            assert!(looks_like_feed_url(&reqwest::Url::parse(s).unwrap()), "{s}");
        }
        assert!(!looks_like_feed_url(
            &reqwest::Url::parse("https://example.com/article").unwrap()
        ));
    }

    #[test]
    fn render_caps_items() {
        let mut xml = String::from("<rss><channel><title>T</title>");
        for i in 0..30 {
            xml.push_str(&format!("<item><title>Item {i}</title></item>"));
        }
        xml.push_str("</channel></rss>");
        let out = render(&xml, "https://x/feed").unwrap();
        assert!(out.contains("30 item(s) (showing 20)"));
        assert!(out.contains("Item 19"));
        assert!(!out.contains("Item 20\n"), "capped at 20");
    }
}
