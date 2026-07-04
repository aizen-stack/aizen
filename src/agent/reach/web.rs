//! The `web` channel — the always-available floor every other channel can fall back to.
//!
//! Chain: `direct` (plain GET → readable text) → `jina` (the Jina Reader, `https://r.jina.ai/<url>`,
//! keyless ≈20 req/min) when the direct fetch is blocked, JS-only, or thin. A few hosts that are
//! KNOWN to be useless for a plain client (reddit's 403 wall, linkedin, the x.com JS shell) go
//! jina-FIRST — that is the honest replacement for the platforms Agent-Reach reaches via cookies
//! or browser sessions.
//!
//! Privacy note: the jina backend sends the target URL to a third-party service (jina.ai). It is
//! only ever used for public http(s) URLs that already passed the SSRF floor.

use super::http;
use crate::agent::web_tools::{html_to_text, truncate_chars, FETCH_CAP};
use anyhow::{bail, Result};
use std::time::Duration;

/// Hosts where a direct fetch is known-dead (bot wall / JS shell) — skip straight to the reader.
/// reddit: anonymous access removed ~May 2026 (403 on every UA); linkedin/x: JS + login walls.
const JINA_FIRST: &[&str] = &["reddit.com", "linkedin.com", "x.com", "twitter.com"];

fn jina_first(host: &str) -> bool {
    let h = host.to_ascii_lowercase();
    JINA_FIRST.iter().any(|d| h == *d || h.ends_with(&format!(".{d}")))
}

/// Read any URL: direct fetch with automatic Jina-reader fallback (or jina-first for known-blocked
/// hosts). The caller (route) has already run the SSRF guard on `url`.
pub(crate) async fn read(url: &str) -> Result<String> {
    let c = http::client()?;
    let host = reqwest::Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_string)).unwrap_or_default();
    if jina_first(&host) {
        match jina_read(&c, url).await {
            Ok(s) => return Ok(s),
            Err(e) => {
                super::note_err("web", "jina", &e.to_string());
                // Fall through to direct — maybe the host serves plain clients after all.
            }
        }
        return direct_read(&c, url).await;
    }
    match direct_read(&c, url).await {
        Ok(s) if !looks_blocked_or_thin(&s) => Ok(s),
        direct_result => {
            // Blocked/thin/failed → try the reader before giving up.
            match jina_read(&c, url).await {
                Ok(s) => Ok(s),
                Err(je) => {
                    super::note_err("web", "jina", &je.to_string());
                    // Prefer the direct result (even a thin one) over a double failure.
                    direct_result.map_err(|de| anyhow::anyhow!("{de}; jina fallback also failed: {je}"))
                }
            }
        }
    }
}

/// Plain GET → readable text, `[status url]` header line (the classic `web_fetch` output shape).
/// A body that turns out to be an RSS/Atom feed renders as a feed instead (content sniff — the
/// URL-shape routing can't catch feeds served from arbitrary paths).
pub(crate) async fn direct_read(c: &reqwest::Client, url: &str) -> Result<String> {
    match http::get(c, url, &[]).await {
        Ok(f) => {
            let text = f.text();
            if f.is_success() && super::feed::sniff(&text) {
                if let Ok(rendered) = super::feed::render(&text, url) {
                    super::note_ok("feed", "builtin");
                    return Ok(rendered);
                }
            }
            let looks_html = f.ctype.contains("html") || text.trim_start().starts_with('<');
            let content = if looks_html { html_to_text(&text) } else { text };
            super::note_ok("web", "direct");
            Ok(format!("[{} {}]\n{}", f.status, url, truncate_chars(&content, FETCH_CAP)))
        }
        Err(e) => {
            super::note_err("web", "direct", &e.to_string());
            Err(e)
        }
    }
}

/// Heuristic: did the direct fetch hit a bot wall / JS shell? Conservative — only clear signals,
/// so a legitimately short page doesn't get re-fetched through a third party.
fn looks_blocked_or_thin(rendered: &str) -> bool {
    let status: u16 = rendered
        .strip_prefix('[')
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse().ok())
        .unwrap_or(200);
    if matches!(status, 401 | 403 | 407 | 429 | 451 | 503) {
        return true;
    }
    let body = rendered.split_once('\n').map(|x| x.1).unwrap_or("");
    let head: String = body.chars().take(600).collect::<String>().to_ascii_lowercase();
    let challenge = ["just a moment", "enable javascript", "verify you are human", "attention required", "checking your browser"];
    if challenge.iter().any(|m| head.contains(m)) {
        return true;
    }
    // A page that reduced to almost nothing = a JS-only shell. Low floor (40 chars) so a terse but
    // real response (an API health line, say) never takes a pointless third-party round-trip.
    status == 200 && body.trim().chars().count() < 40
}

/// Read through the Jina Reader. Keyless tier is ~20 req/min → paced at 3 s (a configured key
/// lifts the quota, so pacing relaxes).
pub(crate) async fn jina_read(c: &reqwest::Client, url: &str) -> Result<String> {
    let key = super::jina_key();
    super::pace("jina", if key.is_some() { Duration::from_millis(500) } else { Duration::from_secs(3) }).await;
    let mut headers: Vec<(&str, String)> = Vec::new();
    if let Some(k) = &key {
        headers.push(("Authorization", format!("Bearer {k}")));
    }
    let f = http::get(c, &format!("https://r.jina.ai/{url}"), &headers).await?;
    if f.status == 429 {
        bail!("jina reader rate-limited (HTTP 429; keyless tier is ~20 req/min — set JINA_API_KEY to raise it)");
    }
    if !f.is_success() {
        bail!("jina reader returned HTTP {}: {}", f.status, http::snippet(&f.text()));
    }
    super::note_ok("web", "jina");
    Ok(format!("[jina {}]\n{}", url, truncate_chars(&f.text(), FETCH_CAP)))
}

// ── doctor probe ────────────────────────────────────────────────────────────

pub(crate) async fn probe_jina() -> super::Probe {
    let c = match http::client() {
        Ok(c) => c,
        Err(e) => return super::Probe::Fail(e.to_string()),
    };
    let keyed = super::jina_key().is_some();
    match jina_read(&c, "https://example.com").await {
        Ok(_) => super::Probe::Ok(if keyed {
            "reader OK (key set — raised quota)".into()
        } else {
            "reader OK (keyless ≈20 req/min; JINA_API_KEY raises it)".into()
        }),
        Err(e) if e.to_string().contains("429") => super::Probe::Warn(format!("rate-limited: {}", http::snippet(&e.to_string()))),
        Err(e) => super::Probe::Fail(http::snippet(&e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jina_first_hosts_match_subdomains() {
        assert!(jina_first("reddit.com"));
        assert!(jina_first("www.reddit.com"));
        assert!(jina_first("old.reddit.com"));
        assert!(jina_first("www.linkedin.com"));
        assert!(jina_first("x.com"));
        assert!(!jina_first("example.com"));
        assert!(!jina_first("notreddit.com"), "suffix must match on a label boundary");
    }

    #[test]
    fn blocked_detection_is_conservative() {
        assert!(looks_blocked_or_thin("[403 https://a.com]\nForbidden"));
        assert!(looks_blocked_or_thin("[200 https://a.com]\nJust a moment... Enable JavaScript and cookies to continue"));
        assert!(looks_blocked_or_thin("[200 https://a.com]\n  \n"), "empty shell");
        let real = format!("[200 https://a.com]\n{}", "Real page content here. ".repeat(20));
        assert!(!looks_blocked_or_thin(&real));
        // A short-but-real page (an API health endpoint, say) over the 40-char floor stays direct.
        let short = format!("[200 https://a.com]\n{}", "ok — service healthy, version 1.2.3, uptime 99 days");
        assert!(!looks_blocked_or_thin(&short));
    }
}
