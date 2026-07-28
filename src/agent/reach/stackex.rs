//! The `stackexchange` channel — question + top answers for Stack Overflow (and any Stack
//! Exchange site) URLs.
//!
//! Backend `api`: api.stackexchange.com 2.3 — keyless quota is 300 requests/day per IP, responses
//! are gzip-compressed unconditionally (inflated in `http::read_body`), and the `backoff` field
//! must be honored (ignoring it earns IP bans — we surface it and record it in the pace gate).
//! Backend `html`: the plain page fetch (SO is server-rendered) when the API fails or the quota
//! is gone.

use super::http;
use crate::agent::web_tools::{decode_entities, html_to_text, truncate_chars};
use anyhow::{bail, Result};
use std::time::Duration;

const MAX_ANSWERS: usize = 3;
const POST_CAP: usize = 6_000;

/// Map a host to its API `site` parameter: `stackoverflow.com` → `stackoverflow`,
/// `superuser.com`/`serverfault.com`/`askubuntu.com` → themselves, `<x>.stackexchange.com` → `<x>`.
pub(crate) fn site_of(host: &str) -> Option<String> {
    let h = host.to_ascii_lowercase();
    let h = h.trim_start_matches("www.");
    match h {
        "stackoverflow.com" => Some("stackoverflow".into()),
        "superuser.com" => Some("superuser".into()),
        "serverfault.com" => Some("serverfault".into()),
        "askubuntu.com" => Some("askubuntu".into()),
        "mathoverflow.net" => Some("mathoverflow.net".into()),
        _ => h
            .strip_suffix(".stackexchange.com")
            .filter(|s| !s.is_empty() && !s.contains('.'))
            .map(str::to_string),
    }
}

/// Extract (site, question_id) from `/questions/<id>/…` or the short `/q/<id>` form.
pub(crate) fn question(url: &reqwest::Url) -> Option<(String, u64)> {
    let site = site_of(url.host_str()?)?;
    let segs: Vec<&str> = url.path_segments()?.filter(|p| !p.is_empty()).collect();
    let id = match segs.as_slice() {
        ["questions", id, ..] | ["q", id, ..] => id.parse().ok()?,
        _ => return None,
    };
    Some((site, id))
}

pub(crate) async fn read(url: &reqwest::Url, site: &str, qid: u64) -> Result<String> {
    let c = http::client()?;
    match read_api(&c, site, qid).await {
        Ok(s) => Ok(s),
        Err(e) => {
            super::note_err("stackexchange", "api", &e.to_string());
            let s = super::web::direct_read(&c, url.as_str())
                .await
                .map_err(|de| {
                    anyhow::anyhow!("StackExchange API failed ({e}); page fetch also failed: {de}")
                })?;
            super::note_ok("stackexchange", "html");
            Ok(s)
        }
    }
}

async fn read_api(c: &reqwest::Client, site: &str, qid: u64) -> Result<String> {
    super::pace("stackexchange", Duration::from_millis(200)).await;
    let qurl =
        format!("https://api.stackexchange.com/2.3/questions/{qid}?site={site}&filter=withbody");
    let qv = http::get_json(c, &qurl, &api_headers()).await?;
    honor_backoff(&qv).await;
    check_quota(&qv)?;
    let q = qv["items"].as_array().and_then(|a| a.first()).cloned();
    let Some(q) = q else {
        bail!("question {qid} not found on {site}");
    };

    let mut s = format!(
        "[{site} q{qid}] {} (score {}, {} answers{})\nasked by {} — {}\n\n{}\n",
        decode_entities(q["title"].as_str().unwrap_or("(untitled)")),
        q["score"].as_i64().unwrap_or(0),
        q["answer_count"].as_u64().unwrap_or(0),
        if q["is_answered"].as_bool().unwrap_or(false) {
            ", accepted"
        } else {
            ""
        },
        q["owner"]["display_name"].as_str().unwrap_or("?"),
        q["link"].as_str().unwrap_or(""),
        truncate_chars(
            &html_to_text(q["body"].as_str().unwrap_or("(no body)")),
            POST_CAP
        ),
    );

    // Top answers by votes (one more call, capped).
    let aurl = format!(
        "https://api.stackexchange.com/2.3/questions/{qid}/answers?site={site}&order=desc&sort=votes&filter=withbody&pagesize={MAX_ANSWERS}"
    );
    if let Ok(av) = http::get_json(c, &aurl, &api_headers()).await {
        honor_backoff(&av).await;
        for a in av["items"].as_array().cloned().unwrap_or_default() {
            s.push_str(&format!(
                "\n── answer (score {}{}) by {} ──\n{}\n",
                a["score"].as_i64().unwrap_or(0),
                if a["is_accepted"].as_bool().unwrap_or(false) {
                    ", ✓ accepted"
                } else {
                    ""
                },
                a["owner"]["display_name"].as_str().unwrap_or("?"),
                truncate_chars(&html_to_text(a["body"].as_str().unwrap_or("")), POST_CAP),
            ));
        }
    }
    super::note_ok("stackexchange", "api");
    Ok(s.trim_end().to_string())
}

pub(crate) fn api_headers() -> Vec<(&'static str, String)> {
    // The API compresses unconditionally; being explicit keeps proxies honest.
    vec![("Accept-Encoding", "gzip".to_string())]
}

/// The API's `backoff` field (seconds) is mandatory etiquette — sleep it off here so the NEXT call
/// this session can't trip the ban hammer.
async fn honor_backoff(v: &serde_json::Value) {
    if let Some(secs) = v["backoff"].as_u64() {
        tokio::time::sleep(Duration::from_secs(secs.min(35))).await;
    }
}

fn check_quota(v: &serde_json::Value) -> Result<()> {
    if let Some(rem) = v["quota_remaining"].as_u64() {
        if rem == 0 {
            bail!("StackExchange keyless quota exhausted (300/day per IP) — falling back to page fetch");
        }
    }
    Ok(())
}

// ── doctor probe ────────────────────────────────────────────────────────────

pub(crate) async fn probe_api() -> super::Probe {
    let c = match http::client() {
        Ok(c) => c,
        Err(e) => return super::Probe::Fail(e.to_string()),
    };
    match http::get_json(
        &c,
        "https://api.stackexchange.com/2.3/info?site=stackoverflow",
        &api_headers(),
    )
    .await
    {
        Ok(v) => {
            let rem = v["quota_remaining"].as_u64().unwrap_or(0);
            let max = v["quota_max"].as_u64().unwrap_or(300);
            if rem == 0 {
                super::Probe::Warn(format!(
                    "quota exhausted (0/{max} today) — html fallback active"
                ))
            } else {
                super::Probe::Ok(format!("Q&A reads OK — quota {rem}/{max} today"))
            }
        }
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
    fn maps_hosts_to_api_sites() {
        assert_eq!(
            site_of("stackoverflow.com").as_deref(),
            Some("stackoverflow")
        );
        assert_eq!(
            site_of("www.stackoverflow.com").as_deref(),
            Some("stackoverflow")
        );
        assert_eq!(site_of("superuser.com").as_deref(), Some("superuser"));
        assert_eq!(site_of("rust.stackexchange.com").as_deref(), Some("rust"));
        assert_eq!(
            site_of("meta.rust.stackexchange.com"),
            None,
            "nested subdomains refused"
        );
        assert_eq!(site_of("example.com"), None);
    }

    #[test]
    fn extracts_question_ids() {
        assert_eq!(
            question(&u("https://stackoverflow.com/questions/12345/how-do-i-x")),
            Some(("stackoverflow".into(), 12345))
        );
        assert_eq!(
            question(&u("https://stackoverflow.com/q/999")),
            Some(("stackoverflow".into(), 999))
        );
        assert_eq!(
            question(&u("https://stackoverflow.com/users/1/someone")),
            None
        );
        assert_eq!(
            question(&u("https://stackoverflow.com/questions/tagged/rust")),
            None,
            "non-numeric id"
        );
    }

    #[test]
    fn quota_zero_is_an_error() {
        assert!(check_quota(&serde_json::json!({"quota_remaining": 0})).is_err());
        assert!(check_quota(&serde_json::json!({"quota_remaining": 5})).is_ok());
        assert!(
            check_quota(&serde_json::json!({})).is_ok(),
            "absent field is not an error"
        );
    }
}
