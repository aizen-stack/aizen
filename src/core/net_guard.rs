//! SSRF floor for the outward-facing web tools (`web_fetch` / `web_crawl` / `ng crawl`).
//!
//! These tools GET arbitrary URLs the model (or a prompt-injected page) chooses, and feed the body
//! straight back into the model context. Without a guard, `http://169.254.169.254/…` (cloud
//! instance-metadata → credentials), `http://127.0.0.1:…` (local admin panels), or any RFC1918
//! internal service is reachable — the canonical agentic-CLI credential-theft path.
//!
//! So before any such fetch we resolve the host and REFUSE loopback / private / link-local /
//! unspecified / CGNAT targets. Literal-IP hosts are checked directly; named hosts are resolved
//! (getaddrinfo / tokio lookup) and EVERY returned address must pass. Opt out for legitimate
//! local-dev use with `AIZEN_ALLOW_PRIVATE_NET=1`.
//!
//! Limitation (documented, acceptable for v0): a DNS-rebinding host could resolve to a public IP
//! here and a private one when reqwest re-resolves (TOCTOU). This floor stops the overwhelmingly
//! common static/literal SSRF; pinning the resolved IP into the request is a future hardening.

use anyhow::{bail, Result};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Env escape hatch: permit private/loopback/link-local targets (local-dev docs, internal services).
pub fn private_net_allowed() -> bool {
    matches!(
        std::env::var("AIZEN_ALLOW_PRIVATE_NET").ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Is this resolved IP in a range we refuse to fetch (the SSRF floor)?
pub fn is_blocked_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_blocked_v4(v4),
        IpAddr::V6(v6) => is_blocked_v6(v6),
    }
}

fn is_blocked_v4(ip: &Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_loopback()            // 127.0.0.0/8
        || ip.is_private()      // 10/8, 172.16/12, 192.168/16
        || ip.is_link_local()   // 169.254.0.0/16 (incl. cloud metadata 169.254.169.254)
        || ip.is_unspecified()  // 0.0.0.0
        || ip.is_broadcast()    // 255.255.255.255
        || o[0] == 0            // 0.0.0.0/8 "this network"
        || (o[0] == 100 && (o[1] & 0xc0) == 64) // 100.64.0.0/10 CGNAT
}

fn is_blocked_v6(ip: &Ipv6Addr) -> bool {
    // IPv4-mapped (::ffff:a.b.c.d) → judge by the embedded v4 (e.g. ::ffff:169.254.169.254).
    if let Some(v4) = ip.to_ipv4_mapped() {
        return is_blocked_v4(&v4);
    }
    let seg0 = ip.segments()[0];
    ip.is_loopback()                 // ::1
        || ip.is_unspecified()       // ::
        || (seg0 & 0xfe00) == 0xfc00 // ULA fc00::/7
        || (seg0 & 0xffc0) == 0xfe80 // link-local fe80::/10
}

/// Parse a URL into (host, port), rejecting non-http(s) schemes.
fn host_and_port(url: &str) -> Result<(String, u16)> {
    let u = url::Url::parse(url.trim()).map_err(|e| anyhow::anyhow!("invalid URL: {e}"))?;
    match u.scheme() {
        "http" | "https" => {}
        s => bail!("unsupported URL scheme '{s}' (only http/https)"),
    }
    let host = u.host_str().ok_or_else(|| anyhow::anyhow!("URL has no host"))?.to_string();
    let port = u.port_or_known_default().unwrap_or(if u.scheme() == "https" { 443 } else { 80 });
    Ok((host, port))
}

fn blocked_msg(host: &str) -> String {
    format!(
        "refusing to fetch '{host}': resolves to a private/loopback/link-local address (SSRF guard). \
         Set AIZEN_ALLOW_PRIVATE_NET=1 to allow local/internal targets."
    )
}

/// A literal-IP host, or an obviously-local name, can be decided without DNS. Returns
/// `Some(Err)` if it must be blocked, `Some(Ok)` if it's a safe literal IP, `None` if it needs
/// a DNS resolution to decide.
fn pre_dns_verdict(host: &str) -> Option<Result<()>> {
    if let Ok(ip) = host.parse::<IpAddr>() {
        return Some(if is_blocked_ip(&ip) { Err(anyhow::anyhow!(blocked_msg(host))) } else { Ok(()) });
    }
    let lower = host.to_ascii_lowercase();
    if lower == "localhost" || lower.ends_with(".localhost") {
        return Some(Err(anyhow::anyhow!(blocked_msg(host))));
    }
    None
}

/// Synchronous guard (the `Tool::execute` path is sync): resolve via getaddrinfo and reject any
/// blocked address. Call BEFORE issuing the request.
pub fn guard_url(url: &str) -> Result<()> {
    if private_net_allowed() {
        return Ok(());
    }
    let (host, port) = host_and_port(url)?;
    if let Some(v) = pre_dns_verdict(&host) {
        return v;
    }
    use std::net::ToSocketAddrs;
    let addrs = (host.as_str(), port)
        .to_socket_addrs()
        .map_err(|e| anyhow::anyhow!("cannot resolve host '{host}': {e}"))?;
    let mut any = false;
    for sa in addrs {
        any = true;
        if is_blocked_ip(&sa.ip()) {
            bail!(blocked_msg(&host));
        }
    }
    if !any {
        bail!("host '{host}' did not resolve to any address");
    }
    Ok(())
}

/// Async guard (the crawler runs inside an async context): same policy via tokio's resolver, so we
/// don't block the reactor. Used to vet every seed + followed link.
pub async fn guard_url_async(url: &str) -> Result<()> {
    if private_net_allowed() {
        return Ok(());
    }
    let (host, port) = host_and_port(url)?;
    if let Some(v) = pre_dns_verdict(&host) {
        return v;
    }
    let addrs = tokio::net::lookup_host((host.as_str(), port))
        .await
        .map_err(|e| anyhow::anyhow!("cannot resolve host '{host}': {e}"))?;
    let mut any = false;
    for sa in addrs {
        any = true;
        if is_blocked_ip(&sa.ip()) {
            bail!(blocked_msg(&host));
        }
    }
    if !any {
        bail!("host '{host}' did not resolve to any address");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    #[test]
    fn blocks_the_dangerous_ranges() {
        assert!(is_blocked_ip(&ip("127.0.0.1")));
        assert!(is_blocked_ip(&ip("169.254.169.254")), "cloud metadata endpoint");
        assert!(is_blocked_ip(&ip("10.0.0.5")));
        assert!(is_blocked_ip(&ip("172.16.3.4")));
        assert!(is_blocked_ip(&ip("192.168.1.1")));
        assert!(is_blocked_ip(&ip("0.0.0.0")));
        assert!(is_blocked_ip(&ip("100.64.0.1")), "CGNAT");
        assert!(is_blocked_ip(&ip("::1")));
        assert!(is_blocked_ip(&ip("fe80::1")));
        assert!(is_blocked_ip(&ip("fc00::1")), "ULA");
        assert!(is_blocked_ip(&ip("::ffff:127.0.0.1")), "v4-mapped loopback");
        assert!(is_blocked_ip(&ip("::ffff:169.254.169.254")), "v4-mapped metadata");
    }

    #[test]
    fn allows_public_addresses() {
        assert!(!is_blocked_ip(&ip("1.1.1.1")));
        assert!(!is_blocked_ip(&ip("8.8.8.8")));
        assert!(!is_blocked_ip(&ip("93.184.216.34")), "example.com");
        assert!(!is_blocked_ip(&ip("2606:4700:4700::1111")), "public v6");
        assert!(!is_blocked_ip(&ip("99.64.0.1")), "just outside CGNAT");
    }

    #[test]
    fn guard_url_blocks_literal_private_and_localhost() {
        assert!(guard_url("http://127.0.0.1/").is_err());
        assert!(guard_url("http://169.254.169.254/latest/meta-data/").is_err());
        assert!(guard_url("http://localhost:8080/admin").is_err());
        assert!(guard_url("https://foo.localhost/").is_err());
        assert!(guard_url("http://[::1]/").is_err());
        assert!(guard_url("http://192.168.0.1/").is_err());
    }

    #[test]
    fn guard_url_allows_literal_public_ip() {
        assert!(guard_url("http://1.1.1.1/").is_ok());
    }

    #[test]
    fn guard_url_rejects_non_http_scheme() {
        assert!(guard_url("file:///etc/passwd").is_err());
        assert!(guard_url("ftp://example.com/").is_err());
        assert!(guard_url("not a url").is_err());
    }

    #[test]
    fn opt_out_env_disables_the_guard() {
        // Serialize against other env-mutating tests via the shared home lock (any global lock works).
        let _g = crate::core::config::TEST_HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("AIZEN_ALLOW_PRIVATE_NET", "1");
        assert!(guard_url("http://127.0.0.1/").is_ok(), "opt-out env permits private");
        std::env::remove_var("AIZEN_ALLOW_PRIVATE_NET");
        assert!(guard_url("http://127.0.0.1/").is_err(), "and re-blocks once unset");
    }
}
