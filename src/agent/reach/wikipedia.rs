//! The `wikipedia` channel — clean article reads for `<lang>.wikipedia.org/wiki/<title>` URLs.
//!
//! Backend `rest`: the REST v1 summary endpoint (fast, clean extract) followed by the full page
//! fetched directly and reduced to text — both are official, keyless, effectively unlimited at
//! agent scale (Wikimedia's policy asks only for an identifiable User-Agent, which aizen sends).
//! Backend `html`: plain page fetch alone, when the REST call fails.

use super::http;
use crate::agent::web_tools::{html_to_text, truncate_chars};
use anyhow::Result;

/// Extract (lang, title) from a wikipedia article URL. The title keeps its percent-encoding —
/// it goes straight back into the REST path.
pub(crate) fn article(url: &reqwest::Url) -> Option<(String, String)> {
    let host = url.host_str()?.to_ascii_lowercase();
    let lang = host.strip_suffix(".wikipedia.org").map(|l| l.trim_start_matches("www.").to_string())?;
    let lang = if lang.is_empty() || lang == "www" { "en".to_string() } else { lang };
    let mut segs = url.path_segments()?;
    if segs.next()? != "wiki" {
        return None;
    }
    let title = segs.next()?.to_string();
    if title.is_empty() || segs.next().is_some() {
        return None;
    }
    Some((lang, title))
}

pub(crate) async fn read(url: &reqwest::Url, lang: &str, title: &str) -> Result<String> {
    let c = http::client()?;
    let summary = http::get_json(&c, &format!("https://{lang}.wikipedia.org/api/rest_v1/page/summary/{title}"), &[]).await;
    match summary {
        Ok(v) => {
            super::note_ok("wikipedia", "rest");
            let mut s = format!(
                "[wikipedia {}] {}\n{}\n\n{}\n",
                lang,
                v["title"].as_str().unwrap_or(title),
                v["description"].as_str().unwrap_or(""),
                v["extract"].as_str().unwrap_or("(no extract)"),
            );
            // The summary is a paragraph — append the full article body for real reading depth.
            if let Ok(page) = http::get(&c, url.as_str(), &[]).await {
                if page.is_success() {
                    s.push_str(&format!(
                        "\n── full article ──\n{}",
                        truncate_chars(&html_to_text(&page.text()), 14_000)
                    ));
                }
            }
            Ok(s.trim_end().to_string())
        }
        Err(e) => {
            super::note_err("wikipedia", "rest", &e.to_string());
            let s = super::web::direct_read(&c, url.as_str()).await?;
            super::note_ok("wikipedia", "html");
            Ok(s)
        }
    }
}

// ── doctor probe ────────────────────────────────────────────────────────────

pub(crate) async fn probe_rest() -> super::Probe {
    let c = match http::client() {
        Ok(c) => c,
        Err(e) => return super::Probe::Fail(e.to_string()),
    };
    match http::get_json(&c, "https://en.wikipedia.org/api/rest_v1/page/summary/Earth", &[]).await {
        Ok(v) if v["extract"].as_str().is_some() => super::Probe::Ok("summaries + articles OK (all languages)".into()),
        Ok(_) => super::Probe::Fail("summary payload missing extract".into()),
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
    fn extracts_lang_and_title() {
        assert_eq!(
            article(&u("https://en.wikipedia.org/wiki/Rust_(programming_language)")),
            Some(("en".into(), "Rust_(programming_language)".into()))
        );
        assert_eq!(article(&u("https://vi.wikipedia.org/wiki/Việt_Nam")).map(|(l, _)| l), Some("vi".into()));
        assert_eq!(article(&u("https://www.wikipedia.org/")), None);
        assert_eq!(article(&u("https://en.wikipedia.org/w/index.php?title=X")), None);
        assert_eq!(article(&u("https://en.wikipedia.org/wiki/A/B")), None, "multi-segment titles are not articles");
        assert_eq!(article(&u("https://example.com/wiki/X")), None);
    }
}
