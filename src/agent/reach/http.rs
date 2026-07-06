//! Shared HTTP plumbing for the reach layer — ONE canonical guarded fetch used by every backend.
//!
//! Same security posture as `web_tools` (which now routes through here): rustls-only client,
//! auto-redirects DISABLED, redirects followed manually with a per-hop `net_guard` SSRF re-vet, so
//! a public URL 302-ing to `http://169.254.169.254/…` is refused. Bodies come back as bytes (the
//! StackExchange API gzips unconditionally; gzip is inflated here with pure-Rust flate2/miniz_oxide).

use anyhow::{bail, Context, Result};
use std::time::Duration;

const UA: &str = concat!("aizen/", env!("CARGO_PKG_VERSION"));
pub(crate) const REQUEST_TIMEOUT_SECS: u64 = 20;
const MAX_REDIRECTS: usize = 5;
/// Refuse to buffer bodies past this (a tarpit/giant file can't eat the process). 8 MiB covers the
/// largest legitimate reach payload (an InnerTube player response is ~300 KiB).
const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// A short-lived client for one reach call (own pool, bound to the runtime that first drives it).
/// UA note: several APIs (GitHub, Wikimedia, arXiv) *want* an identifiable client UA — and DDG's
/// bot wall blocks curl-shaped UAs but accepts this one (live-verified 2026-07-04).
pub(crate) fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(UA)
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .context("building HTTP client")
}

pub(crate) struct Fetched {
    pub status: u16,
    pub ctype: String,
    pub body: Vec<u8>,
}

impl Fetched {
    /// Body as (lossy) text.
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// GET `url` with optional extra headers, following redirects manually and re-vetting every hop
/// against the SSRF floor. The caller must have vetted the INITIAL url (`net_guard::guard_url`).
/// Credential headers (Authorization) are sent only to the ORIGINAL host — a redirect to another
/// host must not receive the caller's bearer token (the same rule reqwest's own redirect policy
/// applies).
pub(crate) async fn get(c: &reqwest::Client, url: &str, headers: &[(&str, String)]) -> Result<Fetched> {
    fn host_of(u: &str) -> Option<String> {
        reqwest::Url::parse(u).ok()?.host_str().map(str::to_ascii_lowercase)
    }
    let origin_host = host_of(url);
    let mut url = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let same_host = host_of(&url) == origin_host;
        let mut req = c.get(&url);
        for (k, v) in headers {
            if !same_host && k.eq_ignore_ascii_case("authorization") {
                continue;
            }
            req = req.header(*k, v);
        }
        let resp = req.send().await.with_context(|| format!("GET {url} failed"))?;
        if let Some(next) = redirect_target(&resp, &url)? {
            crate::core::net_guard::guard_url_async(&next).await?;
            url = next;
            continue;
        }
        return read_body(resp).await;
    }
    bail!("too many redirects (> {MAX_REDIRECTS})")
}

/// POST a JSON body (the InnerTube shape). Redirects are NOT followed (as above).
pub(crate) async fn post_json(
    c: &reqwest::Client,
    url: &str,
    headers: &[(&str, String)],
    body: &serde_json::Value,
) -> Result<Fetched> {
    let mut req = c.post(url).json(body);
    for (k, v) in headers {
        req = req.header(*k, v);
    }
    let resp = req.send().await.with_context(|| format!("POST {url} failed"))?;
    read_body(resp).await
}

/// Extract a vetted-later redirect target, resolving a relative `Location` against the current URL.
fn redirect_target(resp: &reqwest::Response, current: &str) -> Result<Option<String>> {
    if !matches!(resp.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
        return Ok(None);
    }
    let loc = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(|| anyhow::anyhow!("redirect ({}) with no usable Location header", resp.status()))?;
    let next = reqwest::Url::parse(current)
        .ok()
        .and_then(|base| base.join(loc).ok())
        .ok_or_else(|| anyhow::anyhow!("bad redirect target '{loc}'"))?
        .to_string();
    Ok(Some(next))
}

/// Buffer the body BOUNDEDLY (streamed chunk-by-chunk so a tarpit/giant file aborts at the cap
/// instead of being buffered whole first), inflating gzip when the server says so (or the magic
/// bytes do — api.stackexchange.com gzips unconditionally but not every proxy keeps the header).
async fn read_body(mut resp: reqwest::Response) -> Result<Fetched> {
    let status = resp.status().as_u16();
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let gzipped_hdr = resp
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.to_ascii_lowercase().contains("gzip"))
        .unwrap_or(false);
    if let Some(len) = resp.content_length() {
        if len > MAX_BODY_BYTES as u64 {
            bail!("response body too large ({len} bytes > {MAX_BODY_BYTES})");
        }
    }
    let mut bytes: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.context("reading response body")? {
        if bytes.len() + chunk.len() > MAX_BODY_BYTES {
            bail!("response body too large (> {MAX_BODY_BYTES} bytes)");
        }
        bytes.extend_from_slice(&chunk);
    }
    let body = if gzipped_hdr || bytes.starts_with(&[0x1f, 0x8b]) {
        gunzip(&bytes)?
    } else {
        bytes
    };
    Ok(Fetched { status, ctype, body })
}

/// Inflate a gzip body (pure-Rust miniz_oxide backend), bounded like the raw read.
fn gunzip(bytes: &[u8]) -> Result<Vec<u8>> {
    use std::io::Read;
    let mut out = Vec::new();
    let mut dec = flate2::read::GzDecoder::new(bytes).take(MAX_BODY_BYTES as u64 + 1);
    dec.read_to_end(&mut out).context("inflating gzip response")?;
    if out.len() > MAX_BODY_BYTES {
        bail!("decompressed body too large (> {MAX_BODY_BYTES} bytes)");
    }
    Ok(out)
}

/// GET returning parsed JSON (with the reach-standard error message shape).
pub(crate) async fn get_json(
    c: &reqwest::Client,
    url: &str,
    headers: &[(&str, String)],
) -> Result<serde_json::Value> {
    let f = get(c, url, headers).await?;
    if !f.is_success() {
        bail!("HTTP {} from {url}: {}", f.status, snippet(&f.text()));
    }
    serde_json::from_slice(&f.body).with_context(|| format!("parsing JSON from {url}"))
}

/// First ~200 chars of an error body — enough to diagnose, never floods the model.
pub(crate) fn snippet(s: &str) -> String {
    let t = s.trim();
    let cut: String = t.chars().take(200).collect();
    if t.chars().count() > 200 {
        format!("{cut}…")
    } else {
        cut
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gunzip_roundtrip() {
        use std::io::Write;
        let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(b"{\"items\":[1,2,3]}").unwrap();
        let gz = enc.finish().unwrap();
        assert_eq!(gunzip(&gz).unwrap(), b"{\"items\":[1,2,3]}");
    }

    #[test]
    fn gunzip_rejects_garbage() {
        assert!(gunzip(&[0x1f, 0x8b, 0xff, 0xff]).is_err());
    }

    #[test]
    fn snippet_caps() {
        let long = "x".repeat(500);
        let s = snippet(&long);
        assert!(s.chars().count() <= 201);
        assert!(s.ends_with('…'));
        assert_eq!(snippet("short"), "short");
    }
}
