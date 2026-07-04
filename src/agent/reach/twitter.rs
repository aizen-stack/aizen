//! The `twitter` channel — keyless SINGLE-TWEET reads (capability honesty: search and timelines
//! need cookies/login; profile and other x.com URLs go through the web channel's Jina-first path).
//!
//! Backend `fxtwitter`: `https://api.fxtwitter.com/status/<id>` — the FixTweet community API,
//! richer payload (views/media/author stats), third-party goodwill (can vanish; hence the chain).
//! Backend `syndication`: `https://cdn.syndication.twimg.com/tweet-result?id=<id>&token=<t>` — the
//! CDN that powers X's own embed widgets; survived every purge since 2023. Live-verified
//! 2026-07-04: the token only has to be present and non-empty; the classic derivation
//! `((id/1e15)*π).toString(36)` minus zeros/dots is kept as cheap insurance anyway.

use super::http;
use anyhow::{bail, Result};

/// Extract a numeric status id from tweet URL shapes (`/<user>/status/<id>`, `/i/status/<id>`,
/// legacy `/statuses/<id>`), on twitter.com / x.com / mobile hosts.
pub(crate) fn tweet_id(url: &reqwest::Url) -> Option<String> {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let host = host.trim_start_matches("www.").trim_start_matches("mobile.");
    if !matches!(host, "twitter.com" | "x.com") {
        return None;
    }
    let segs: Vec<&str> = url.path_segments().map(|s| s.filter(|p| !p.is_empty()).collect()).unwrap_or_default();
    let id = match segs.as_slice() {
        // "/i/web/status/<id>" — the canonical share-link shape from the official apps — must be
        // matched BEFORE the generic "/<user>/status/<id>" arms ("i" is a reserved prefix, not a user).
        ["i", "web", "status", id, ..] => Some(*id),
        [_user, "status", id, ..] => Some(*id),
        [_user, "statuses", id, ..] => Some(*id),
        _ => None,
    }?;
    let digits: String = id.chars().take_while(|c| c.is_ascii_digit()).collect();
    (!digits.is_empty()).then_some(digits)
}

pub(crate) async fn read(id: &str) -> Result<String> {
    let c = http::client()?;
    let mut failures = Vec::new();
    for backend in super::ordered_backends("twitter") {
        let out = match backend {
            "fxtwitter" => read_fxtwitter(&c, id).await,
            "syndication" => read_syndication(&c, id).await,
            _ => continue,
        };
        match out {
            Ok(s) => {
                super::note_ok("twitter", backend);
                return Ok(s);
            }
            Err(e) => {
                super::note_err("twitter", backend, &e.to_string());
                failures.push(format!("{backend}: {}", http::snippet(&e.to_string())));
            }
        }
    }
    bail!("could not read tweet {id} — {}", failures.join("; "))
}

async fn read_fxtwitter(c: &reqwest::Client, id: &str) -> Result<String> {
    let v = http::get_json(c, &format!("https://api.fxtwitter.com/status/{id}"), &[]).await?;
    if v["code"].as_u64() != Some(200) {
        bail!("fxtwitter code {}: {}", v["code"], v["message"].as_str().unwrap_or(""));
    }
    Ok(render_fxtweet(&v["tweet"]))
}

pub(crate) fn render_fxtweet(t: &serde_json::Value) -> String {
    let author = &t["author"];
    let mut s = format!(
        "[tweet {}] @{} ({}) — {}\n{}\n",
        t["id"].as_str().unwrap_or("?"),
        author["screen_name"].as_str().unwrap_or("?"),
        author["name"].as_str().unwrap_or("?"),
        t["created_at"].as_str().unwrap_or("?"),
        t["text"].as_str().unwrap_or("(no text)"),
    );
    s.push_str(&format!(
        "♥ {}  ⇄ {}  💬 {}  views {}\n",
        t["likes"].as_u64().unwrap_or(0),
        t["retweets"].as_u64().unwrap_or(0),
        t["replies"].as_u64().unwrap_or(0),
        t["views"].as_u64().map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
    ));
    if let Some(photos) = t["media"]["photos"].as_array() {
        for p in photos {
            if let Some(u) = p["url"].as_str() {
                s.push_str(&format!("photo: {u}\n"));
            }
        }
    }
    if let Some(videos) = t["media"]["videos"].as_array() {
        for vdo in videos {
            if let Some(u) = vdo["url"].as_str() {
                s.push_str(&format!("video: {u}\n"));
            }
        }
    }
    if let Some(q) = t.get("quote") {
        if q.is_object() {
            s.push_str(&format!(
                "quoting @{}: {}\n",
                q["author"]["screen_name"].as_str().unwrap_or("?"),
                q["text"].as_str().unwrap_or("")
            ));
        }
    }
    s.trim_end().to_string()
}

async fn read_syndication(c: &reqwest::Client, id: &str) -> Result<String> {
    let url = format!("https://cdn.syndication.twimg.com/tweet-result?id={id}&token={}&lang=en", syndication_token(id));
    let f = http::get(c, &url, &[("Accept", "application/json".to_string())]).await?;
    if f.status == 404 {
        bail!("tweet {id} not found (deleted or never existed)");
    }
    if !f.is_success() || f.body.is_empty() {
        bail!("syndication CDN returned HTTP {} ({} bytes)", f.status, f.body.len());
    }
    let v: serde_json::Value = serde_json::from_slice(&f.body)?;
    match v["__typename"].as_str() {
        Some("Tweet") => Ok(render_syndication(&v)),
        Some(other) => bail!("tweet {id} unavailable ({other} — age-restricted or withheld)"),
        None => bail!("unexpected syndication payload"),
    }
}

pub(crate) fn render_syndication(v: &serde_json::Value) -> String {
    let user = &v["user"];
    let mut s = format!(
        "[tweet {}] @{} ({}) — {}\n{}\n",
        v["id_str"].as_str().unwrap_or("?"),
        user["screen_name"].as_str().unwrap_or("?"),
        user["name"].as_str().unwrap_or("?"),
        v["created_at"].as_str().unwrap_or("?"),
        v["text"].as_str().unwrap_or("(no text)"),
    );
    s.push_str(&format!(
        "♥ {}  💬 {}\n",
        v["favorite_count"].as_u64().unwrap_or(0),
        v["conversation_count"].as_u64().unwrap_or(0),
    ));
    if let Some(photos) = v["photos"].as_array() {
        for p in photos {
            if let Some(u) = p["url"].as_str() {
                s.push_str(&format!("photo: {u}\n"));
            }
        }
    }
    if v["is_note_tweet"].as_bool() == Some(true) {
        s.push_str("(long-form note tweet — text may be truncated by this endpoint)\n");
    }
    s.trim_end().to_string()
}

/// Best-effort port of the classic embed-widget token: `((id/1e15)*π).toString(36)` with zeros and
/// the dot stripped. The endpoint currently accepts ANY non-empty token (live-verified 2026-07-04),
/// so an approximation is fine — this just keeps us close to what real embeds send. Capped at 8
/// base36 digits (JS emits ~that many significant digits for real tweet ids).
pub(crate) fn syndication_token(id: &str) -> String {
    let id: f64 = id.parse().unwrap_or(0.0);
    let mut x = (id / 1e15) * std::f64::consts::PI;
    let mut digits = String::new();
    // Integer part (nonzero for post-2022 ids: they exceed 1e15/π).
    let int_part = x.trunc() as u64;
    if int_part > 0 {
        let mut n = int_part;
        let mut buf = Vec::new();
        while n > 0 {
            buf.push(std::char::from_digit((n % 36) as u32, 36).unwrap());
            n /= 36;
        }
        digits.extend(buf.iter().rev());
    }
    x = x.fract();
    for _ in 0..12 {
        x *= 36.0;
        digits.push(std::char::from_digit(x.trunc() as u32 % 36, 36).unwrap());
        x = x.fract();
    }
    let token: String = digits.chars().filter(|c| *c != '0').take(8).collect();
    if token.is_empty() {
        "a".to_string() // any non-empty token works; never send an empty one (that 404s)
    } else {
        token
    }
}

// ── doctor probes (tweet id 20 = "just setting up my twttr", stable since 2006) ──

pub(crate) async fn probe_fxtwitter() -> super::Probe {
    let c = match http::client() {
        Ok(c) => c,
        Err(e) => return super::Probe::Fail(e.to_string()),
    };
    match read_fxtwitter(&c, "20").await {
        Ok(_) => super::Probe::Ok("single-tweet reads OK (fxtwitter; no search/timelines keyless)".into()),
        Err(e) => super::Probe::Fail(http::snippet(&e.to_string())),
    }
}

pub(crate) async fn probe_syndication() -> super::Probe {
    let c = match http::client() {
        Ok(c) => c,
        Err(e) => return super::Probe::Fail(e.to_string()),
    };
    match read_syndication(&c, "20").await {
        Ok(_) => super::Probe::Ok("single-tweet reads OK (syndication CDN)".into()),
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
    fn extracts_tweet_ids() {
        assert_eq!(tweet_id(&u("https://twitter.com/jack/status/20")).as_deref(), Some("20"));
        assert_eq!(tweet_id(&u("https://x.com/jack/status/20?s=46")).as_deref(), Some("20"));
        assert_eq!(tweet_id(&u("https://x.com/i/status/1585841080431321088")).as_deref(), Some("1585841080431321088"));
        assert_eq!(tweet_id(&u("https://twitter.com/i/web/status/1585841080431321088")).as_deref(), Some("1585841080431321088"), "canonical share-link shape");
        assert_eq!(tweet_id(&u("https://mobile.twitter.com/a/statuses/99")).as_deref(), Some("99"));
        assert_eq!(tweet_id(&u("https://x.com/jack/status/20/photo/1")).as_deref(), Some("20"));
        assert_eq!(tweet_id(&u("https://x.com/jack")), None, "profile URL is not a tweet");
        assert_eq!(tweet_id(&u("https://example.com/a/status/20")), None, "wrong host");
    }

    #[test]
    fn token_is_nonempty_zero_free_and_stable() {
        for id in ["20", "1585841080431321088", "0"] {
            let t = syndication_token(id);
            assert!(!t.is_empty(), "id {id}");
            assert!(!t.contains('0'), "id {id}: {t}");
            assert_eq!(t, syndication_token(id), "deterministic for {id}");
        }
        // Classic derivation for id 20 starts "6dq" (cross-checked against the JS formula).
        assert!(syndication_token("20").starts_with("6dq"), "got {}", syndication_token("20"));
    }

    #[test]
    fn renders_fxtweet_payload() {
        let t = serde_json::json!({
            "id": "20", "text": "just setting up my twttr", "created_at": "2006-03-21",
            "author": {"screen_name": "jack", "name": "jack"},
            "likes": 308436, "retweets": 100, "replies": 5, "views": 1000,
            "media": {"photos": [{"url": "https://pic/1.jpg"}]},
        });
        let s = render_fxtweet(&t);
        assert!(s.contains("@jack"));
        assert!(s.contains("just setting up my twttr"));
        assert!(s.contains("♥ 308436"));
        assert!(s.contains("photo: https://pic/1.jpg"));
    }

    #[test]
    fn renders_syndication_payload_and_flags_notes() {
        let v = serde_json::json!({
            "__typename": "Tweet", "id_str": "20", "text": "hi", "created_at": "2006",
            "user": {"screen_name": "jack", "name": "jack"},
            "favorite_count": 1, "conversation_count": 2, "is_note_tweet": true,
        });
        let s = render_syndication(&v);
        assert!(s.contains("@jack"));
        assert!(s.contains("long-form note tweet"));
    }
}
