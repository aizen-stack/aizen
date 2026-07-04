//! The `youtube` channel — metadata + transcript for a video URL, no yt-dlp required.
//!
//! Backend `innertube`: POST `https://www.youtube.com/youtubei/v1/player` with an ANDROID client
//! context (live-verified 2026-07-04: works keyless — no scraped API key, and the ANDROID client
//! currently avoids YouTube's PO-token gate that blocks web clients). The response carries
//! `videoDetails` (title/author/length/views) and `captions…captionTracks[]`; each track's signed
//! `baseUrl` returns timedtext XML (`<p t="ms" d="ms">line</p>`) with a ~6 h TTL — fetched
//! immediately, never cached. Track choice: manual captions before auto-generated (`kind:"asr"`),
//! English before others (manual-en → manual-first → asr-en → asr-first).
//!
//! Backend `oembed`: `https://www.youtube.com/oembed?…` — official, keyless, metadata only. Serves
//! as the degraded fallback when the player call breaks (scraping-adjacent surfaces do drift:
//! `CLIENT_VERSION` below is config-pinned and may need an occasional bump).

use super::http;
use crate::agent::web_tools::{decode_entities, truncate_chars};
use anyhow::{bail, Context, Result};
use once_cell::sync::Lazy;
use regex::Regex;

const CLIENT_NAME: &str = "ANDROID";
const CLIENT_VERSION: &str = "20.10.38";
/// Transcripts can be long; cap separately from the metadata header.
const TRANSCRIPT_CAP: usize = 16_000;

/// Extract a video id from the URL forms that carry one.
pub(crate) fn video_id(url: &reqwest::Url) -> Option<String> {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host);
    let segs: Vec<&str> = url.path_segments().map(|s| s.filter(|p| !p.is_empty()).collect()).unwrap_or_default();
    let id = match host {
        "youtu.be" => segs.first().map(|s| s.to_string()),
        "youtube.com" | "m.youtube.com" | "music.youtube.com" => match segs.as_slice() {
            ["watch", ..] => url.query_pairs().find(|(k, _)| k == "v").map(|(_, v)| v.into_owned()),
            ["shorts", id, ..] | ["embed", id, ..] | ["live", id, ..] | ["v", id, ..] => Some(id.to_string()),
            _ => None,
        },
        _ => None,
    };
    // Canonical ids are 11 chars of [A-Za-z0-9_-]; refuse anything else (it would be interpolated
    // into request bodies/URLs).
    id.filter(|s| s.len() == 11 && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'))
}

pub(crate) async fn read(vid: &str) -> Result<String> {
    let c = http::client()?;
    match read_innertube(&c, vid).await {
        Ok(s) => Ok(s),
        Err(e) => {
            super::note_err("youtube", "innertube", &e.to_string());
            let meta = read_oembed(&c, vid)
                .await
                .with_context(|| format!("innertube failed ({e}); oembed fallback also failed"))?;
            super::note_ok("youtube", "oembed");
            Ok(format!("{meta}\n(transcript unavailable: {})", http::snippet(&e.to_string())))
        }
    }
}

async fn read_innertube(c: &reqwest::Client, vid: &str) -> Result<String> {
    let body = serde_json::json!({
        "context": {"client": {"clientName": CLIENT_NAME, "clientVersion": CLIENT_VERSION, "androidSdkVersion": 30}},
        "videoId": vid,
    });
    let f = http::post_json(c, "https://www.youtube.com/youtubei/v1/player", &[], &body).await?;
    if !f.is_success() {
        bail!("InnerTube player returned HTTP {}", f.status);
    }
    let v: serde_json::Value = serde_json::from_slice(&f.body).context("parsing InnerTube player JSON")?;

    let playable = v["playabilityStatus"]["status"].as_str().unwrap_or("UNKNOWN");
    if playable != "OK" {
        let reason = v["playabilityStatus"]["reason"].as_str().unwrap_or("(no reason given)");
        bail!("video not playable ({playable}): {reason}");
    }
    let mut out = render_details(&v["videoDetails"], vid);

    match pick_track(&v["captions"]["playerCaptionsTracklistRenderer"]["captionTracks"]) {
        Some((base_url, lang, asr)) => {
            // Defense-in-depth: the track URL comes from YouTube's response, but it IS a fetched
            // URL — run it past the SSRF floor like any other.
            crate::core::net_guard::guard_url_async(&base_url).await?;
            let track = http::get(c, &base_url, &[]).await?;
            if !track.is_success() || track.body.is_empty() {
                bail!("caption track fetch failed (HTTP {}, {} bytes) — possibly a PO-token demand", track.status, track.body.len());
            }
            let text = timedtext_to_text(&track.text());
            if text.trim().is_empty() {
                bail!("caption track was empty — possibly a PO-token demand");
            }
            out.push_str(&format!(
                "\n── transcript ({lang}{}) ──\n{}",
                if asr { ", auto-generated" } else { "" },
                truncate_chars(&text, TRANSCRIPT_CAP)
            ));
        }
        None => out.push_str("\n(no caption tracks — the video has no transcript)"),
    }
    super::note_ok("youtube", "innertube");
    Ok(out)
}

fn render_details(d: &serde_json::Value, vid: &str) -> String {
    let secs = d["lengthSeconds"].as_str().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
    let mut s = format!(
        "[youtube {vid}] {}\nby {} · {}m{}s · {} views\n",
        d["title"].as_str().unwrap_or("(unknown title)"),
        d["author"].as_str().unwrap_or("(unknown author)"),
        secs / 60,
        secs % 60,
        d["viewCount"].as_str().unwrap_or("?"),
    );
    if let Some(desc) = d["shortDescription"].as_str() {
        if !desc.trim().is_empty() {
            s.push_str(&format!("{}\n", truncate_chars(desc.trim(), 800)));
        }
    }
    s
}

/// Pick the best caption track: manual (non-asr) beats auto-generated, English beats other
/// languages within each class. Returns (baseUrl, languageCode, is_asr).
pub(crate) fn pick_track(tracks: &serde_json::Value) -> Option<(String, String, bool)> {
    let tracks = tracks.as_array()?;
    let score = |t: &serde_json::Value| -> u8 {
        let asr = t["kind"].as_str() == Some("asr");
        let en = t["languageCode"].as_str().map(|l| l.starts_with("en")).unwrap_or(false);
        match (asr, en) {
            (false, true) => 0,
            (false, false) => 1,
            (true, true) => 2,
            (true, false) => 3,
        }
    };
    let best = tracks.iter().filter(|t| t["baseUrl"].as_str().is_some()).min_by_key(|t| score(t))?;
    Some((
        best["baseUrl"].as_str().unwrap().to_string(),
        best["languageCode"].as_str().unwrap_or("?").to_string(),
        best["kind"].as_str() == Some("asr"),
    ))
}

/// Reduce timedtext XML to plain lines: `<p t d>text</p>` elements (format 3), entities decoded,
/// `<s>` word-level children flattened, `<br>`-ish whitespace collapsed.
pub(crate) fn timedtext_to_text(xml: &str) -> String {
    static P: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?is)<p\b[^>]*>(.*?)</p>").unwrap());
    static TAG: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<[^>]+>").unwrap());
    let mut out = String::new();
    for cap in P.captures_iter(xml) {
        let inner = TAG.replace_all(&cap[1], "");
        let line = decode_entities(&inner).split_whitespace().collect::<Vec<_>>().join(" ");
        if !line.is_empty() {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out.trim_end().to_string()
}

async fn read_oembed(c: &reqwest::Client, vid: &str) -> Result<String> {
    let url = format!("https://www.youtube.com/oembed?url=https://www.youtube.com/watch?v={vid}&format=json");
    let v = http::get_json(c, &url, &[]).await?;
    Ok(format!(
        "[youtube {vid}] {}\nby {} ({})",
        v["title"].as_str().unwrap_or("(unknown title)"),
        v["author_name"].as_str().unwrap_or("(unknown author)"),
        v["author_url"].as_str().unwrap_or(""),
    ))
}

// ── doctor probes ───────────────────────────────────────────────────────────

/// A real InnerTube probe against a stable well-known video (checks the whole path incl. captions).
pub(crate) async fn probe_innertube() -> super::Probe {
    let c = match http::client() {
        Ok(c) => c,
        Err(e) => return super::Probe::Fail(e.to_string()),
    };
    let body = serde_json::json!({
        "context": {"client": {"clientName": CLIENT_NAME, "clientVersion": CLIENT_VERSION, "androidSdkVersion": 30}},
        "videoId": "dQw4w9WgXcQ",
    });
    match http::post_json(&c, "https://www.youtube.com/youtubei/v1/player", &[], &body).await {
        Ok(f) if f.is_success() => {
            let v: serde_json::Value = match serde_json::from_slice(&f.body) {
                Ok(v) => v,
                Err(e) => return super::Probe::Fail(format!("player JSON unparseable: {e}")),
            };
            if v["playabilityStatus"]["status"].as_str() != Some("OK") {
                return super::Probe::Warn(format!(
                    "player responds but probe video not playable ({}) — client version may need a bump",
                    v["playabilityStatus"]["status"].as_str().unwrap_or("?")
                ));
            }
            if pick_track(&v["captions"]["playerCaptionsTracklistRenderer"]["captionTracks"]).is_some() {
                super::Probe::Ok("metadata + transcripts OK (InnerTube, keyless)".into())
            } else {
                super::Probe::Warn("metadata OK but no caption tracks on the probe video — transcripts may be gated".into())
            }
        }
        Ok(f) => super::Probe::Fail(format!("InnerTube HTTP {}", f.status)),
        Err(e) => super::Probe::Fail(http::snippet(&e.to_string())),
    }
}

pub(crate) async fn probe_oembed() -> super::Probe {
    let c = match http::client() {
        Ok(c) => c,
        Err(e) => return super::Probe::Fail(e.to_string()),
    };
    match read_oembed(&c, "dQw4w9WgXcQ").await {
        Ok(_) => super::Probe::Ok("metadata OK (official oEmbed; no transcripts)".into()),
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
    fn extracts_video_ids_from_all_url_forms() {
        for url in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com/watch?t=10&v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ?t=42",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/embed/dQw4w9WgXcQ",
            "https://m.youtube.com/watch?v=dQw4w9WgXcQ",
        ] {
            assert_eq!(video_id(&u(url)).as_deref(), Some("dQw4w9WgXcQ"), "{url}");
        }
        assert_eq!(video_id(&u("https://www.youtube.com/@SomeChannel")), None);
        assert_eq!(video_id(&u("https://www.youtube.com/watch?v=short")), None, "malformed id refused");
        assert_eq!(video_id(&u("https://example.com/watch?v=dQw4w9WgXcQ")), None, "wrong host");
    }

    #[test]
    fn track_selection_prefers_manual_english() {
        let tracks = serde_json::json!([
            {"baseUrl": "https://yt/asr-en", "languageCode": "en", "kind": "asr"},
            {"baseUrl": "https://yt/manual-de", "languageCode": "de"},
            {"baseUrl": "https://yt/manual-en", "languageCode": "en"},
        ]);
        let (url, lang, asr) = pick_track(&tracks).unwrap();
        assert_eq!(url, "https://yt/manual-en");
        assert_eq!(lang, "en");
        assert!(!asr);
        // Without a manual track, asr-en wins over manual-absent languages… i.e. asr-de.
        let tracks = serde_json::json!([
            {"baseUrl": "https://yt/asr-de", "languageCode": "de", "kind": "asr"},
            {"baseUrl": "https://yt/asr-en", "languageCode": "en", "kind": "asr"},
        ]);
        assert_eq!(pick_track(&tracks).unwrap().0, "https://yt/asr-en");
        assert!(pick_track(&serde_json::json!(null)).is_none());
        assert!(pick_track(&serde_json::json!([])).is_none());
    }

    #[test]
    fn timedtext_reduces_to_lines() {
        let xml = r#"<?xml version="1.0"?><timedtext format="3">
            <body>
              <p t="18640" d="3240">&#39;We&#39;re no strangers to love</p>
              <p t="22000" d="2000"><s>You</s> <s>know</s> the rules</p>
              <p t="24000" d="1000"></p>
            </body></timedtext>"#;
        let text = timedtext_to_text(xml);
        assert_eq!(text, "'We're no strangers to love\nYou know the rules");
    }

    #[test]
    fn render_details_formats_metadata() {
        let d = serde_json::json!({
            "title": "T", "author": "A", "lengthSeconds": "125", "viewCount": "1000",
            "shortDescription": "desc",
        });
        let s = render_details(&d, "dQw4w9WgXcQ");
        assert!(s.contains("[youtube dQw4w9WgXcQ] T"));
        assert!(s.contains("2m5s"));
        assert!(s.contains("1000 views"));
        assert!(s.contains("desc"));
    }
}
