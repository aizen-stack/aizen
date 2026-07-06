//! Reach — platform-aware internet access for the agent (a Rust port of the *concept* behind
//! [Agent-Reach](https://github.com/Panniantong/Agent-Reach): per-platform channels, each with an
//! ORDERED chain of backends, plus a `doctor` that reports which backend currently serves each
//! platform).
//!
//! The port deliberately upgrades the original's architecture: Agent-Reach wraps external CLIs the
//! agent must invoke itself, so its fallbacks are prose "retry chains" the model follows by hand.
//! Aizen's backends are in-process HTTP recipes (pure Rust, keyless-first, all live-verified
//! 2026-07-04), so fallback happens automatically at CALL TIME — the model just calls `web_fetch`/
//! `web_search` and gets the best available path. What we keep from the original: the ordered
//! backend chains, the doctor contract (`status/name/message/tier/backends/active_backend`), tier
//! grouping (0 = zero-config, 1 = optional key), the reorder-not-pin backend override, crash-proof
//! per-channel checks, prescriptive single-action fix messages, and "the web channel is the always-
//! available floor and is never probed".
//!
//! Capability honesty (why some platforms are absent or partial):
//! - Reddit's anonymous `.json` endpoints are DEAD (403 everywhere since ~May 2026; API signups
//!   approval-gated) → reddit URLs route through the Jina reader as best-effort, no channel row.
//! - Twitter/X keyless = single-tweet reads only (syndication CDN / fxtwitter); search and
//!   timelines need cookies we don't harvest.
//! - CJK platforms (bilibili/xiaohongshu/…) and cookie/browser-session backends are out of scope.

pub mod feed;
pub mod github;
pub mod hn;
pub mod http;
pub mod route;
pub mod search;
pub mod stackex;
pub mod twitter;
pub mod web;
pub mod wikipedia;
pub mod youtube;

use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// ── channel registry ────────────────────────────────────────────────────────

pub struct ChannelSpec {
    pub name: &'static str,
    pub label: &'static str,
    /// 0 = zero-config, 1 = works keyless but an optional key raises limits/unlocks a backend.
    pub tier: u8,
    /// Ordered candidates — `backends[0]` is preferred, the rest are fallbacks.
    pub backends: &'static [&'static str],
}

pub const CHANNELS: &[ChannelSpec] = &[
    ChannelSpec { name: "web", label: "any web page", tier: 0, backends: &["direct", "jina"] },
    // KEYED-ONLY (tier 1): DuckDuckGo scraping + the Marginalia floor were dropped — DDG's anomaly
    // wall (HTTP 202) blocked keyless scraping too often to be a dependable primary. Tavily is the
    // real backend; jina-search is a secondary keyed fallback. With NO key, `web_search` returns an
    // actionable "add a Tavily key" error instead of degrading to an unreliable scrape.
    ChannelSpec { name: "search", label: "web search (needs a key)", tier: 1, backends: &["tavily", "jina-search"] },
    ChannelSpec { name: "github", label: "GitHub repos, files, issues/PRs", tier: 0, backends: &["api", "raw", "html"] },
    ChannelSpec { name: "youtube", label: "YouTube metadata + transcripts", tier: 0, backends: &["innertube", "oembed"] },
    ChannelSpec { name: "twitter", label: "Twitter/X single tweets", tier: 0, backends: &["fxtwitter", "syndication"] },
    ChannelSpec { name: "hackernews", label: "Hacker News stories + comments", tier: 0, backends: &["algolia", "firebase"] },
    ChannelSpec { name: "wikipedia", label: "Wikipedia summaries + articles", tier: 0, backends: &["rest", "html"] },
    ChannelSpec { name: "feed", label: "RSS/Atom feeds (incl. arXiv API)", tier: 0, backends: &["builtin"] },
    ChannelSpec { name: "stackexchange", label: "Stack Overflow / Stack Exchange Q&A", tier: 0, backends: &["api", "html"] },
];

pub fn channel(name: &str) -> Option<&'static ChannelSpec> {
    CHANNELS.iter().find(|c| c.name == name)
}

/// Candidate backends in probe/try order, honoring the user override
/// `AIZEN_REACH_<CHANNEL>_BACKEND`. Override semantics ported verbatim from Agent-Reach: the named
/// backend is moved to the FRONT (reorder, not pin — the rest stay as fallbacks), and an unknown
/// value is ignored so a stale override can never hide working backends.
pub fn ordered_backends(name: &str) -> Vec<&'static str> {
    let spec = match channel(name) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let mut candidates: Vec<&'static str> = spec.backends.to_vec();
    let var = format!("AIZEN_REACH_{}_BACKEND", name.to_ascii_uppercase());
    if let Ok(want) = std::env::var(&var) {
        let want = want.trim().to_ascii_lowercase();
        if !want.is_empty() {
            if let Some(i) = candidates.iter().position(|b| *b == want || b.starts_with(want.as_str())) {
                let b = candidates.remove(i);
                candidates.insert(0, b);
            }
        }
    }
    candidates
}

// ── passive backend-outcome state (what actually served the last calls) ───────

struct Outcome {
    ok: bool,
    note: String,
}

static OUTCOMES: Lazy<Mutex<HashMap<String, Outcome>>> = Lazy::new(|| Mutex::new(HashMap::new()));
static ACTIVE: Lazy<Mutex<HashMap<&'static str, &'static str>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Record a successful real call (not a probe) — marks the backend active for its channel.
pub(crate) fn note_ok(channel: &'static str, backend: &'static str) {
    let mut o = OUTCOMES.lock().unwrap_or_else(|e| e.into_inner());
    o.insert(format!("{channel}/{backend}"), Outcome { ok: true, note: String::new() });
    let mut a = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    a.insert(channel, backend);
}

/// Record a failed real call (kept for `/reach` display; the router has already moved on to the
/// next backend).
pub(crate) fn note_err(channel: &'static str, backend: &'static str, err: &str) {
    let mut o = OUTCOMES.lock().unwrap_or_else(|e| e.into_inner());
    o.insert(format!("{channel}/{backend}"), Outcome { ok: false, note: http::snippet(err) });
}

/// The backend that served this channel's most recent successful call this session.
pub fn last_active(channel: &str) -> Option<&'static str> {
    let a = ACTIVE.lock().unwrap_or_else(|e| e.into_inner());
    a.get(channel).copied()
}

// ── pace gate (politeness floors for rate-limited upstreams) ─────────────────

static PACE: Lazy<Mutex<HashMap<&'static str, Instant>>> = Lazy::new(|| Mutex::new(HashMap::new()));

/// Enforce a minimum interval between calls keyed by `key` (e.g. "ddg" 2 s, "arxiv" 3 s — the
/// latter is arXiv's Terms of Use). Sleeps only when called again too soon.
pub(crate) async fn pace(key: &'static str, min: Duration) {
    loop {
        let wait = {
            let mut p = PACE.lock().unwrap_or_else(|e| e.into_inner());
            let now = Instant::now();
            match p.get(key) {
                Some(last) if now.duration_since(*last) < min => min - now.duration_since(*last),
                _ => {
                    p.insert(key, now);
                    return;
                }
            }
        };
        tokio::time::sleep(wait).await;
    }
}

// ── in-process TTL cache (search results) ────────────────────────────────────
//
// A session that searches the same thing twice — a retry, a fan-out angle that overlaps a prior
// call, a sub-agent repeating the parent's query — should not re-hit the network or pay the `pace`
// politeness floor again (W23). Keyed by an opaque caller-built string; values expire after `TTL`.
// Bounded (LRU-ish by insertion time) so a long session can't grow it without limit.

struct Cached {
    value: String,
    at: Instant,
}

static SEARCH_CACHE: Lazy<Mutex<HashMap<String, Cached>>> = Lazy::new(|| Mutex::new(HashMap::new()));
const CACHE_TTL: Duration = Duration::from_secs(600); // 10 min — fresh enough for a work session
const CACHE_MAX: usize = 128;

/// Return a cached value for `key` if present and younger than [`CACHE_TTL`]; expired entries are
/// treated as absent (and dropped opportunistically on the next insert).
pub(crate) fn cache_get(key: &str) -> Option<String> {
    let c = SEARCH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    match c.get(key) {
        Some(e) if e.at.elapsed() < CACHE_TTL => Some(e.value.clone()),
        _ => None,
    }
}

/// Store `value` under `key`. Evicts expired entries first, then — if still at the cap — the single
/// oldest entry, so the map stays bounded without a full LRU structure.
pub(crate) fn cache_put(key: String, value: String) {
    let mut c = SEARCH_CACHE.lock().unwrap_or_else(|e| e.into_inner());
    c.retain(|_, e| e.at.elapsed() < CACHE_TTL);
    if c.len() >= CACHE_MAX {
        if let Some(oldest) = c.iter().min_by_key(|(_, e)| e.at).map(|(k, _)| k.clone()) {
            c.remove(&oldest);
        }
    }
    c.insert(key, Cached { value, at: Instant::now() });
}

#[cfg(test)]
pub(crate) fn cache_clear() {
    SEARCH_CACHE.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

// ── optional keys (everything works without them) ────────────────────────────

pub(crate) fn tavily_key() -> Option<String> {
    crate::core::cli_config::load().reach.unwrap_or_default().resolved_tavily_key()
}

pub(crate) fn jina_key() -> Option<String> {
    crate::core::cli_config::load().reach.unwrap_or_default().resolved_jina_key()
}

pub(crate) fn github_token() -> Option<String> {
    crate::core::cli_config::load().reach.unwrap_or_default().resolved_github_token()
}

// ── doctor ───────────────────────────────────────────────────────────────────

/// One backend probe's verdict. `Off` = not usable but *by configuration* (e.g. missing optional
/// key), which is not a failure.
pub(crate) enum Probe {
    Ok(String),
    Warn(String),
    Off(String),
    Fail(String),
}

pub struct ChannelReport {
    pub name: &'static str,
    pub label: &'static str,
    pub tier: u8,
    pub backends: Vec<&'static str>,
    pub active_backend: Option<&'static str>,
    /// "ok" | "warn" | "off" | "error"
    pub status: &'static str,
    pub message: String,
}

/// Two-phase selection ported from Agent-Reach: prefer the first `Ok` in backend order, only then
/// the first `Warn` — so an installed-but-degraded preferred backend can't shadow a fully-working
/// fallback. When a fallback wins while an earlier backend failed, the failure is surfaced as a
/// suffix (prescriptive but single-action).
fn derive_report(spec: &'static ChannelSpec, probes: Vec<(&'static str, Probe)>) -> ChannelReport {
    let backends: Vec<&'static str> = probes.iter().map(|(b, _)| *b).collect();
    let first_ok = probes.iter().position(|(_, p)| matches!(p, Probe::Ok(_)));
    let first_warn = probes.iter().position(|(_, p)| matches!(p, Probe::Warn(_)));
    let (status, active, mut message) = match (first_ok, first_warn) {
        (Some(i), _) => {
            let (b, Probe::Ok(m)) = &probes[i] else { unreachable!() };
            ("ok", Some(*b), m.clone())
        }
        (None, Some(i)) => {
            let (b, Probe::Warn(m)) = &probes[i] else { unreachable!() };
            ("warn", Some(*b), m.clone())
        }
        (None, None) => {
            // No usable backend: a real failure beats "off" for visibility.
            let fail = probes.iter().find_map(|(_, p)| match p {
                Probe::Fail(m) => Some(m.clone()),
                _ => None,
            });
            match fail {
                Some(m) => ("error", None, m),
                None => {
                    let off = probes.iter().find_map(|(_, p)| match p {
                        Probe::Off(m) => Some(m.clone()),
                        _ => None,
                    });
                    ("off", None, off.unwrap_or_else(|| "no backend available".into()))
                }
            }
        }
    };
    // Surface a broken EARLIER backend even when a fallback won (the user should know the primary
    // path is degraded), but never more than one — single-action messages.
    if let Some(win) = active {
        let win_pos = probes.iter().position(|(b, _)| *b == win).unwrap_or(0);
        if let Some((b, Probe::Fail(m))) = probes[..win_pos].iter().find(|(_, p)| matches!(p, Probe::Fail(_))) {
            message.push_str(&format!("  [earlier backend '{b}' failing: {}]", http::snippet(m)));
        }
    }
    ChannelReport { name: spec.name, label: spec.label, tier: spec.tier, backends, active_backend: active, status, message }
}

/// Run live health probes for every channel (concurrently, ~8 s cap per probe) and report which
/// backend serves each. On-demand only — nothing here runs automatically. Probes are chosen to be
/// quota-cheap (GitHub uses the FREE `/rate_limit`; the `web`/`feed` floors are never probed, an
/// Agent-Reach lesson: the always-available channel must keep doctor fast and zero-overhead).
pub async fn doctor() -> Vec<ChannelReport> {
    const PROBE_TIMEOUT: Duration = Duration::from_secs(8);
    async fn run(fut: impl std::future::Future<Output = Probe>) -> Probe {
        match tokio::time::timeout(PROBE_TIMEOUT, fut).await {
            Ok(p) => p,
            Err(_) => Probe::Fail(format!("probe timed out (> {}s)", PROBE_TIMEOUT.as_secs())),
        }
    }

    let mut reports = Vec::with_capacity(CHANNELS.len());
    for spec in CHANNELS {
        let order = ordered_backends(spec.name);
        // Probe this channel's backends concurrently; channels are sequential so one slow platform
        // can't starve the rest of connections (total wall clock stays ~seconds).
        let futs = order.iter().map(|b| {
            let b = *b;
            async move {
                let p = match (spec.name, b) {
                    ("web", "direct") => Probe::Ok("direct fetch — always-available floor (not probed)".into()),
                    ("web", "jina") => run(web::probe_jina()).await,
                    ("search", "tavily") => run(search::probe_tavily()).await,
                    ("search", "jina-search") => run(search::probe_jina_search()).await,
                    ("github", "api") => run(github::probe_api()).await,
                    ("github", "raw") => run(github::probe_raw()).await,
                    ("github", "html") => Probe::Ok("plain page fetch fallback (not probed)".into()),
                    ("youtube", "innertube") => run(youtube::probe_innertube()).await,
                    ("youtube", "oembed") => run(youtube::probe_oembed()).await,
                    ("twitter", "fxtwitter") => run(twitter::probe_fxtwitter()).await,
                    ("twitter", "syndication") => run(twitter::probe_syndication()).await,
                    ("hackernews", "algolia") => run(hn::probe_algolia()).await,
                    ("hackernews", "firebase") => run(hn::probe_firebase()).await,
                    ("wikipedia", "rest") => run(wikipedia::probe_rest()).await,
                    ("wikipedia", "html") => Probe::Ok("plain page fetch fallback (not probed)".into()),
                    ("feed", "builtin") => Probe::Ok("built-in RSS/Atom parser — no network needed (not probed)".into()),
                    ("stackexchange", "api") => run(stackex::probe_api()).await,
                    ("stackexchange", "html") => Probe::Ok("plain page fetch fallback (not probed)".into()),
                    _ => Probe::Fail("unknown backend".into()),
                };
                (b, p)
            }
        });
        let probes = futures_util::future::join_all(futs).await;
        reports.push(derive_report(spec, probes));
    }
    reports
}

/// Human doctor report, Agent-Reach style: legend, per-channel lines grouped by tier, N/M summary.
pub fn render_report(reports: &[ChannelReport]) -> String {
    let mut s = String::from("reach — platform-aware web access (web_fetch / web_search route through this)\n");
    s.push_str("  legend: ✅ ok  [!] degraded  [·] off (optional)  [x] error\n");
    for tier in [0u8, 1] {
        let rows: Vec<&ChannelReport> = reports.iter().filter(|r| r.tier == tier).collect();
        if rows.is_empty() {
            continue;
        }
        s.push_str(if tier == 0 { "  zero-config:\n" } else { "  optional (key raises limits):\n" });
        for r in rows {
            let icon = match r.status {
                "ok" => "✅",
                "warn" => "[!]",
                "off" => "[·]",
                _ => "[x]",
            };
            let chain = r.backends.join(" · ");
            let active = match r.active_backend {
                Some(b) if r.backends.len() > 1 => format!("  (active: {b})"),
                _ => String::new(),
            };
            s.push_str(&format!("  {icon} {:<13} {} — {}{}\n", r.name, chain, r.message, active));
        }
    }
    let avail = reports.iter().filter(|r| r.status == "ok" || r.status == "warn").count();
    s.push_str(&format!("  {avail}/{} channels available", reports.len()));
    if tavily_key().is_none() {
        s.push_str("\n  tip: web search NEEDS a key — set TAVILY_API_KEY (https://tavily.com) to enable it; a Jina key (JINA_API_KEY) adds a search fallback + raises the reader quota; GITHUB_TOKEN raises GitHub to 5000 req/h");
    } else if jina_key().is_none() {
        s.push_str("\n  tip: a Jina key (JINA_API_KEY) adds a search fallback + raises the reader quota; GITHUB_TOKEN raises GitHub to 5000 req/h");
    }
    s
}

/// Zero-network status view (`/reach` / `/reach status`): the channel table annotated with this
/// session's real outcomes (✓ served / ✗ failed per backend). Live probing is `/reach doctor`.
pub fn render_passive() -> String {
    let outcomes = OUTCOMES.lock().unwrap_or_else(|e| e.into_inner());
    let mut s = String::from("reach — platform-aware web access (web_fetch / web_search route through this)\n");
    let mut failures: Vec<String> = Vec::new();
    for spec in CHANNELS {
        let chain = ordered_backends(spec.name)
            .iter()
            .map(|b| match outcomes.get(&format!("{}/{b}", spec.name)) {
                Some(Outcome { ok: true, .. }) => format!("{b}✓"),
                Some(Outcome { ok: false, note }) => {
                    failures.push(format!("{}/{b}: {note}", spec.name));
                    format!("{b}✗")
                }
                None => b.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" · ");
        let served = last_active(spec.name).map(|b| format!("  (last served by {b})")).unwrap_or_default();
        s.push_str(&format!("  {:<13} {chain} — {}{served}\n", spec.name, spec.label));
    }
    for f in failures.iter().take(5) {
        s.push_str(&format!("  last error {f}\n"));
    }
    s.push_str("  (✓/✗ = this session's real calls; run `/reach doctor` to live-probe every backend)");
    s
}

/// Machine doctor report — the Agent-Reach `doctor --json` contract:
/// `{channel: {status, name, message, tier, backends, active_backend}}`.
pub fn report_json(reports: &[ChannelReport]) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for r in reports {
        map.insert(
            r.name.to_string(),
            serde_json::json!({
                "status": r.status,
                "name": r.label,
                "message": r.message,
                "tier": r.tier,
                "backends": r.backends,
                "active_backend": r.active_backend,
            }),
        );
    }
    serde_json::Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_reorders_and_never_pins() {
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("AIZEN_REACH_TWITTER_BACKEND");
        assert_eq!(ordered_backends("twitter"), vec!["fxtwitter", "syndication"]);
        std::env::set_var("AIZEN_REACH_TWITTER_BACKEND", "syndication");
        // Reorder, not pin: the previous primary stays as a fallback.
        assert_eq!(ordered_backends("twitter"), vec!["syndication", "fxtwitter"]);
        // Unknown values are ignored — a stale override can't hide working backends.
        std::env::set_var("AIZEN_REACH_TWITTER_BACKEND", "nitter");
        assert_eq!(ordered_backends("twitter"), vec!["fxtwitter", "syndication"]);
        // Prefix match (like the original's startswith).
        std::env::set_var("AIZEN_REACH_TWITTER_BACKEND", "synd");
        assert_eq!(ordered_backends("twitter")[0], "syndication");
        std::env::remove_var("AIZEN_REACH_TWITTER_BACKEND");
    }

    #[test]
    fn derive_prefers_first_ok_then_first_warn() {
        let spec = channel("twitter").unwrap();
        // Primary failed, fallback ok → fallback active, primary failure surfaced as suffix.
        let r = derive_report(
            spec,
            vec![("fxtwitter", Probe::Fail("HTTP 503".into())), ("syndication", Probe::Ok("tweet reads OK".into()))],
        );
        assert_eq!(r.status, "ok");
        assert_eq!(r.active_backend, Some("syndication"));
        assert!(r.message.contains("tweet reads OK"));
        assert!(r.message.contains("fxtwitter") && r.message.contains("503"));
        // Warn beats off/fail but loses to ok.
        let r = derive_report(
            spec,
            vec![("fxtwitter", Probe::Warn("degraded".into())), ("syndication", Probe::Ok("fine".into()))],
        );
        assert_eq!(r.active_backend, Some("syndication"));
        // All off → off.
        let r = derive_report(spec, vec![("fxtwitter", Probe::Off("needs key".into())), ("syndication", Probe::Off("needs key".into()))]);
        assert_eq!(r.status, "off");
        // Fail beats off for visibility.
        let r = derive_report(spec, vec![("fxtwitter", Probe::Off("needs key".into())), ("syndication", Probe::Fail("boom".into()))]);
        assert_eq!(r.status, "error");
    }

    #[test]
    fn report_json_matches_the_agent_reach_contract() {
        let spec = channel("hackernews").unwrap();
        let r = derive_report(spec, vec![("algolia", Probe::Ok("ok".into())), ("firebase", Probe::Ok("ok".into()))]);
        let j = report_json(&[r]);
        let hn = &j["hackernews"];
        for key in ["status", "name", "message", "tier", "backends", "active_backend"] {
            assert!(hn.get(key).is_some(), "missing key {key}");
        }
        assert_eq!(hn["status"], "ok");
        assert_eq!(hn["active_backend"], "algolia");
    }

    #[test]
    fn render_groups_and_summarizes() {
        let spec = channel("web").unwrap();
        let r = derive_report(spec, vec![("direct", Probe::Ok("floor".into())), ("jina", Probe::Fail("429".into()))]);
        let out = render_report(&[r]);
        assert!(out.contains("1/1 channels available"));
        assert!(out.contains("direct · jina"));
        assert!(out.contains("(active: direct)"));
    }

    #[test]
    fn outcome_notes_track_active_backend() {
        note_ok("twitter", "fxtwitter");
        assert_eq!(last_active("twitter"), Some("fxtwitter"));
        note_err("twitter", "fxtwitter", "HTTP 500");
        note_ok("twitter", "syndication");
        assert_eq!(last_active("twitter"), Some("syndication"));
    }

    #[test]
    fn search_channel_is_keyed_only_tavily_then_jina() {
        // DuckDuckGo scraping + the Marginalia floor were dropped: the chain is now keyed-only,
        // Tavily first (the real backend), jina-search as the secondary keyed fallback. No keyless
        // scrape remains — with no key `web_search` returns an actionable error, it doesn't degrade.
        let order = ordered_backends("search");
        assert_eq!(order, vec!["tavily", "jina-search"], "keyed-only chain");
        assert!(!order.contains(&"ddg-html") && !order.contains(&"ddg-lite"), "DDG dropped");
        assert!(!order.contains(&"marginalia"), "Marginalia floor dropped");
        assert_eq!(channel("search").unwrap().tier, 1, "search is now a tier-1 (keyed) channel");
    }

    #[test]
    fn ttl_cache_stores_and_evicts() {
        cache_clear();
        assert_eq!(cache_get("k1"), None, "empty on miss");
        cache_put("k1".into(), "v1".into());
        assert_eq!(cache_get("k1"), Some("v1".into()), "hit within TTL");
        // Bound enforcement: fill past CACHE_MAX and confirm the map never exceeds the cap.
        for i in 0..(CACHE_MAX + 20) {
            cache_put(format!("bulk-{i}"), "x".into());
        }
        let len = SEARCH_CACHE.lock().unwrap().len();
        assert!(len <= CACHE_MAX, "cache must stay bounded, got {len}");
        cache_clear();
    }
}
