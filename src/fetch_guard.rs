//! Shared SSRF guard and response-size capping for anything that makes an
//! outbound HTTP-ish request on the model's behalf from smelt's own server
//! process (`src/webfetch.rs`'s real-browser `webfetch`, `src/http_request.rs`'s
//! plain-HTTP `http_request`). Kept as one module deliberately — SSRF logic
//! is exactly the kind of thing that should have a single source of truth,
//! not two copies that could drift apart. See
//! SME-21 and SME-21.

use std::net::IpAddr;

use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::fetch::{
    ContinueRequestParams, EnableParams, EventRequestPaused, FailRequestParams,
};
use chromiumoxide::cdp::browser_protocol::network::ErrorReason;
use futures_util::StreamExt;

/// Enables CDP Fetch-domain request interception on `page` and spawns a
/// background task that checks every paused request's URL against
/// `is_addr_allowed` (via `check_load`, which also lets a `localhost`-style
/// host through when `sandbox_routed` — see there), continuing it if
/// safe and failing it (`ErrorReason::BlockedByClient`) otherwise. Shared
/// by `src/webfetch.rs` (one page, used for the duration of a single
/// `fetch` call) and `src/browsing.rs` (one page, kept open across many
/// tool calls) — the interception itself works identically either way:
/// once enabled, it stays active for every request that page makes,
/// including subresources, redirects, and later navigations, until the
/// page closes or the returned task is aborted. Caller owns the
/// `JoinHandle` and is responsible for aborting it when the page is done
/// with (`webfetch` aborts it right after its one `goto`+extract;
/// `browsing` keeps it running for the session's whole life).
pub async fn spawn_request_interceptor(
    page: &Page,
    is_addr_allowed: fn(IpAddr) -> bool,
    sandbox_routed: bool,
) -> Result<tokio::task::JoinHandle<()>, String> {
    page.execute(EnableParams::default())
        .await
        .map_err(|e| format!("failed to enable request interception: {e}"))?;
    let mut paused = page
        .event_listener::<EventRequestPaused>()
        .await
        .map_err(|e| format!("failed to listen for intercepted requests: {e}"))?;
    let intercept_page = page.clone();
    Ok(tokio::spawn(async move {
        while let Some(event) = paused.next().await {
            let allowed = check_load(&event.request.url, is_addr_allowed, sandbox_routed)
                .await
                .is_ok();
            let result = if allowed {
                intercept_page
                    .execute(ContinueRequestParams::new(event.request_id.clone()))
                    .await
                    .map(|_| ())
            } else {
                intercept_page
                    .execute(FailRequestParams::new(
                        event.request_id.clone(),
                        ErrorReason::BlockedByClient,
                    ))
                    .await
                    .map(|_| ())
            };
            if let Err(e) = result {
                tracing::warn!("fetch_guard: failed to resolve intercepted request: {e}");
            }
        }
    }))
}

/// The core SSRF check: is `addr` safe to let a request actually connect
/// to? Only the public internet is: `false` for loopback, private (RFC
/// 1918), link-local, CGNAT (`100.64.0.0/10`, which Tailscale uses, so a
/// homelab tailnet), benchmarking, documentation, reserved and multicast
/// ranges, in IPv4 and IPv6 (SME-51 B4). An IPv6 address that carries an
/// IPv4 one (mapped, IPv4-compatible, NAT64, 6to4) is judged by that IPv4
/// address. `std`'s `is_global` would do this but isn't stable yet.
pub fn is_safe_fetch_addr(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            let [a, b, c, _] = v4.octets();
            !(v4.is_loopback()
                || v4.is_link_local()
                || v4.is_private()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_documentation()
                || a == 0 // "this network"
                || (a == 100 && (64..128).contains(&b)) // CGNAT
                || (a == 192 && b == 0 && c == 0) // IETF protocol assignments
                || (a == 198 && (b == 18 || b == 19)) // benchmarking
                || a >= 240) // reserved
        }
        IpAddr::V6(v6) => {
            if let Some(v4) = embedded_ipv4(v6) {
                return is_safe_fetch_addr(IpAddr::V4(v4));
            }
            let first = v6.segments()[0];
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (first & 0xfe00) == 0xfc00 // unique local
                || (first & 0xffc0) == 0xfe80 // link-local
                || (first & 0xffc0) == 0xfec0 // site-local (deprecated)
                || (first == 0x2001 && v6.segments()[1] == 0x0db8)) // documentation
        }
    }
}

/// The IPv4 address an IPv6 address stands for, if it's one of the forms
/// that reach an IPv4 host: mapped (`::ffff:a.b.c.d`), IPv4-compatible
/// (`::a.b.c.d`), NAT64 (`64:ff9b::/96`) or 6to4 (`2002:aabb:ccdd::/48`).
fn embedded_ipv4(v6: std::net::Ipv6Addr) -> Option<std::net::Ipv4Addr> {
    if let Some(v4) = v6.to_ipv4_mapped() {
        return Some(v4);
    }
    let s = v6.segments();
    let tail = std::net::Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8);
    let v4_compatible = s[..6].iter().all(|&x| x == 0) && !v6.is_loopback() && !v6.is_unspecified();
    let nat64 = s[0] == 0x64 && s[1] == 0xff9b && s[2..6].iter().all(|&x| x == 0);
    if v4_compatible || nat64 {
        return Some(tail);
    }
    if s[0] == 0x2002 {
        return Some(std::net::Ipv4Addr::new((s[1] >> 8) as u8, s[1] as u8, (s[2] >> 8) as u8, s[2] as u8));
    }
    None
}

/// Whether `host` names "this machine" the way a model writes it —
/// `localhost`, `127.0.0.1` or `::1` (bracketed or not). In a browser
/// context with a sandbox route, these mean the conversation's own pod
/// rather than smelt's server (SME-42); everywhere else they stay refused
/// like any other loopback address. The one definition shared by the
/// egress proxy and the per-page checks, so the two can't drift apart.
pub fn is_sandbox_host(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host = host.strip_suffix('.').unwrap_or(host);
    host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "::1"
}

/// Where in the conversation's pod `host` goes, in a browser context with
/// a sandbox route: a `localhost` name (`is_sandbox_host`) is the pod's own
/// localhost, and an address in the pod's Docker range is that container
/// (SME-33). Anything else isn't the sandbox's.
pub fn sandbox_host(host: &str) -> Option<crate::sandbox::PodHost> {
    if is_sandbox_host(host) {
        return Some(crate::sandbox::PodHost::Localhost);
    }
    crate::docker_net::container_address(host).map(crate::sandbox::PodHost::Container)
}

/// What a request says about the page that sent it — the headers a browser
/// sets itself, which a page can't forge (SME-42).
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RequestSource {
    pub method: String,
    pub origin: Option<String>,
    pub sec_fetch_site: Option<String>,
    pub sec_fetch_mode: Option<String>,
}

/// Whether a request to a sandbox server came from somewhere allowed to
/// send it: that server's own pages ("home", as `is_home` judges an
/// origin's host and port), the model or user asking directly, or a plain
/// top-level page load. Without this, any other site open in the same
/// browser could send requests to the sandbox's servers.
///
/// Browsers send `Origin` on every cross-origin request that could change
/// something and on every WebSocket handshake (which carries no
/// `Sec-Fetch-*`), so a present `Origin` must be home. Otherwise
/// `Sec-Fetch-Site` decides: only a cross-site GET/HEAD page load (a link
/// someone followed) gets through. A request with neither header isn't
/// from a browser page at all.
pub fn allows_request_from(source: &RequestSource, is_home: impl Fn(&str, Option<u16>) -> bool) -> bool {
    if let Some(origin) = &source.origin {
        // "null" (an opaque origin) and anything unparseable aren't home.
        return url::Url::parse(origin)
            .ok()
            .and_then(|url| url.host_str().map(|host| is_home(host, url.port())))
            .unwrap_or(false);
    }
    match source.sec_fetch_site.as_deref() {
        Some("cross-site") => {
            source.sec_fetch_mode.as_deref() == Some("navigate")
                && matches!(source.method.to_ascii_uppercase().as_str(), "GET" | "HEAD")
        }
        _ => true,
    }
}

/// Parses `url` and, if it's a well-formed `http`/`https` URL, returns its
/// `(host, port)` — pure and synchronous; only the DNS resolution that
/// follows (`is_request_allowed`) needs to be async. Rejects every other
/// scheme (`file://`, `data:`, `javascript:`, ...) outright.
pub fn parse_fetch_target(url: &str) -> Result<(String, u16), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    if parsed.scheme() != "http" && parsed.scheme() != "https" {
        return Err(format!("unsupported scheme: {}", parsed.scheme()));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| "URL has no host".to_string())?
        .to_string();
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| "could not determine a port".to_string())?;
    Ok((host, port))
}

/// Test-only convenience: `is_request_allowed_with` against the real
/// default guard, so a test doesn't have to spell out `is_safe_fetch_addr`
/// every time. `http_request` calls `resolve_allowed` itself, to connect
/// to the addresses it checked; browser pages use `check_load`.
#[cfg(test)]
pub async fn is_request_allowed(url: &str) -> bool {
    is_request_allowed_with(url, is_safe_fetch_addr).await
}

/// The real SSRF gate: parses `url`, resolves its host, and checks every
/// resolved address via `is_addr_allowed`. A resolution failure or an
/// empty address list is treated the same as an unsafe address: `false`,
/// not "assume fine." Parameterized on the address-safety predicate purely
/// for testability — every locally-reachable address in this dev
/// container is either loopback or RFC1918-private, so a test exercising
/// "does a legitimate request actually get through" needs a relaxed
/// variant (allowing loopback) while the real default (`is_safe_fetch_addr`)
/// stays strict.
#[cfg(test)]
pub async fn is_request_allowed_with(url: &str, is_addr_allowed: fn(IpAddr) -> bool) -> bool {
    let Ok((host, port)) = parse_fetch_target(url) else {
        return false;
    };
    resolve_allowed(&host, port, is_addr_allowed).await.is_ok()
}

/// Whether a page may load `url` — the check both the per-page request
/// interceptor and the pre-navigation checks make. With `sandbox_routed`
/// (the page's browser context has a sandbox route, SME-42), a
/// `localhost`-style host or a container address in the pod
/// (`sandbox_host`, SME-33) is let through without resolving: the
/// context's own proxy sends it to the conversation's pod, never to this
/// machine or its network.
/// Every other host must resolve only to addresses `is_addr_allowed`
/// accepts. The error says why, for the model to read.
pub async fn check_load(
    url: &str,
    is_addr_allowed: fn(IpAddr) -> bool,
    sandbox_routed: bool,
) -> Result<(), String> {
    let (host, port) = parse_fetch_target(url)?;
    if sandbox_routed && sandbox_host(&host).is_some() {
        return Ok(());
    }
    resolve_allowed(&host, port, is_addr_allowed).await.map(|_| ())
}

/// Resolves `host:port` and returns its addresses only if every one of them
/// passes `is_addr_allowed` — a host with any refused address is refused
/// outright, and a failed or empty resolution is an error, never "assume
/// fine." Callers that go on to connect should connect to one of the
/// returned addresses rather than resolving again, so a DNS answer that
/// changes between check and connect can't slip through.
pub async fn resolve_allowed(
    host: &str,
    port: u16,
    is_addr_allowed: fn(IpAddr) -> bool,
) -> Result<Vec<std::net::SocketAddr>, String> {
    let addrs: Vec<std::net::SocketAddr> = match test_dns::lookup(host, port) {
        Some(addrs) => addrs,
        None => tokio::net::lookup_host((host, port))
            .await
            .map_err(|e| format!("could not resolve {host}: {e}"))?
            .collect(),
    };
    if addrs.is_empty() {
        return Err(format!("{host} resolved to no addresses"));
    }
    if let Some(refused) = addrs.iter().find(|a| !is_addr_allowed(a.ip())) {
        return Err(format!("{host} resolves to a refused address ({})", refused.ip()));
    }
    Ok(addrs)
}

/// Names only `resolve_allowed` knows, for tests that need the guard's
/// answer to differ from the system resolver's (as a DNS-rebinding name's
/// would). Always empty outside tests.
pub mod test_dns {
    #[cfg(test)]
    static ENTRIES: std::sync::LazyLock<
        std::sync::Mutex<std::collections::HashMap<String, Vec<std::net::IpAddr>>>,
    > = std::sync::LazyLock::new(Default::default);

    #[cfg(test)]
    pub fn set(host: &str, addrs: Vec<std::net::IpAddr>) {
        ENTRIES.lock().unwrap().insert(host.to_string(), addrs);
    }

    #[cfg(test)]
    pub(super) fn lookup(host: &str, port: u16) -> Option<Vec<std::net::SocketAddr>> {
        ENTRIES
            .lock()
            .unwrap()
            .get(host)
            .map(|ips| ips.iter().map(|ip| std::net::SocketAddr::new(*ip, port)).collect())
    }

    #[cfg(not(test))]
    pub(super) fn lookup(_host: &str, _port: u16) -> Option<Vec<std::net::SocketAddr>> {
        None
    }
}

/// Caps `text` to `max_chars`, same shape `fetch_command_summary`'s tail
/// truncation already established — a pure function, TDD-able with no
/// network/browser in the loop at all.
pub fn truncate(text: String, max_chars: usize) -> (String, bool) {
    if text.chars().count() > max_chars {
        (text.chars().take(max_chars).collect(), true)
    } else {
        (text, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_fetch_target_accepts_http_and_https() {
        assert_eq!(
            parse_fetch_target("http://example.com/page").unwrap(),
            ("example.com".to_string(), 80)
        );
        assert_eq!(
            parse_fetch_target("https://example.com/page").unwrap(),
            ("example.com".to_string(), 443)
        );
        assert_eq!(
            parse_fetch_target("https://example.com:8443/page").unwrap(),
            ("example.com".to_string(), 8443)
        );
    }

    #[test]
    fn test_parse_fetch_target_rejects_non_http_schemes() {
        assert!(parse_fetch_target("file:///etc/passwd").is_err());
        assert!(parse_fetch_target("data:text/html,<h1>hi</h1>").is_err());
        assert!(parse_fetch_target("javascript:alert(1)").is_err());
        assert!(parse_fetch_target("chrome://version").is_err());
    }

    #[test]
    fn test_parse_fetch_target_rejects_malformed_urls() {
        assert!(parse_fetch_target("not a url").is_err());
    }

    #[test]
    fn test_is_safe_fetch_addr_rejects_loopback() {
        assert!(!is_safe_fetch_addr("127.0.0.1".parse().unwrap()));
        assert!(!is_safe_fetch_addr("::1".parse().unwrap()));
    }

    #[test]
    fn test_is_safe_fetch_addr_rejects_rfc1918_private_ranges() {
        assert!(!is_safe_fetch_addr("10.0.0.5".parse().unwrap()));
        assert!(!is_safe_fetch_addr("172.16.0.5".parse().unwrap()));
        assert!(!is_safe_fetch_addr("192.168.1.5".parse().unwrap()));
    }

    #[test]
    fn test_is_safe_fetch_addr_rejects_link_local_and_unspecified() {
        assert!(!is_safe_fetch_addr("169.254.1.1".parse().unwrap()));
        assert!(!is_safe_fetch_addr("0.0.0.0".parse().unwrap()));
        assert!(!is_safe_fetch_addr("::".parse().unwrap()));
    }

    #[test]
    fn test_is_safe_fetch_addr_rejects_multicast() {
        assert!(!is_safe_fetch_addr("224.0.0.1".parse().unwrap()));
    }

    #[test]
    fn test_is_safe_fetch_addr_rejects_ipv6_unique_local() {
        assert!(!is_safe_fetch_addr("fc00::1".parse().unwrap()));
        assert!(!is_safe_fetch_addr("fd12:3456:789a::1".parse().unwrap()));
    }

    #[test]
    fn test_is_safe_fetch_addr_rejects_ipv4_mapped_private() {
        assert!(!is_safe_fetch_addr("::ffff:127.0.0.1".parse().unwrap()));
    }

    /// SME-51 B4: every range that isn't the public internet is refused,
    /// not just the private and local ones: CGNAT (Tailscale's range, so a
    /// homelab tailnet), IPv6 link-local, benchmarking, reserved and
    /// documentation ranges, and IPv6 forms that embed a refused IPv4
    /// address (NAT64, 6to4, IPv4-compatible).
    #[test]
    fn test_is_safe_fetch_addr_refuses_every_non_public_range() {
        for addr in [
            "100.64.0.1", "100.101.102.103", "100.127.255.254", // CGNAT
            "192.0.0.8", "192.0.2.1", "198.51.100.7", "203.0.113.9", // IETF, documentation
            "198.18.0.1", "198.19.255.255", // benchmarking
            "240.0.0.1", "255.255.255.255", "0.1.2.3", // reserved, broadcast, "this network"
            "fe80::1", "febf::1", "fec0::1", // link-local, site-local
            "64:ff9b::a00:1", "64:ff9b::7f00:1", // NAT64 of 10.0.0.1, 127.0.0.1
            "2002:a00:1::1", "2002:c0a8:101::1", // 6to4 of 10.0.0.1, 192.168.1.1
            "::a00:1", // IPv4-compatible 10.0.0.1
            "2001:db8::1", // documentation
            "::ffff:100.64.0.1", // mapped CGNAT
        ] {
            assert!(!is_safe_fetch_addr(addr.parse().unwrap()), "{addr} should be refused");
        }
        // Embedding a public IPv4 address is fine.
        assert!(is_safe_fetch_addr("64:ff9b::101:101".parse().unwrap()), "NAT64 of 1.1.1.1");
        assert!(is_safe_fetch_addr("2002:101:101::1".parse().unwrap()), "6to4 of 1.1.1.1");
        assert!(is_safe_fetch_addr("100.63.255.255".parse().unwrap()), "just below CGNAT");
    }

    #[test]
    fn test_is_safe_fetch_addr_allows_ordinary_public_addresses() {
        assert!(is_safe_fetch_addr("1.1.1.1".parse().unwrap()));
        assert!(is_safe_fetch_addr("8.8.8.8".parse().unwrap()));
        assert!(is_safe_fetch_addr("2606:4700:4700::1111".parse().unwrap()));
    }

    #[tokio::test]
    async fn test_is_request_allowed_rejects_a_loopback_ip_literal() {
        assert!(!is_request_allowed("http://127.0.0.1:9999/").await);
    }

    #[tokio::test]
    async fn test_is_request_allowed_rejects_a_private_ip_literal() {
        assert!(!is_request_allowed("http://10.0.0.5/").await);
    }

    #[tokio::test]
    async fn test_is_request_allowed_allows_a_public_ip_literal() {
        assert!(is_request_allowed("http://1.1.1.1/").await);
    }

    #[tokio::test]
    async fn test_is_request_allowed_rejects_a_non_http_scheme() {
        assert!(!is_request_allowed("file:///etc/passwd").await);
    }

    #[tokio::test]
    async fn test_is_request_allowed_rejects_an_unresolvable_host() {
        assert!(!is_request_allowed("http://this-host-does-not-exist.invalid/").await);
    }

    #[test]
    fn test_truncate_leaves_short_text_untouched() {
        let (text, truncated) = truncate("hello".to_string(), 20_000);
        assert_eq!(text, "hello");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_caps_long_text_and_flags_it() {
        let long = "a".repeat(20_500);
        let (text, truncated) = truncate(long, 20_000);
        assert_eq!(text.chars().count(), 20_000);
        assert!(truncated);
    }

    #[test]
    fn test_is_sandbox_host_matches_the_ways_a_model_writes_this_machine() {
        for host in ["localhost", "LocalHost", "localhost.", "127.0.0.1", "::1", "[::1]"] {
            assert!(is_sandbox_host(host), "{host} should mean the sandbox");
        }
    }

    #[test]
    fn test_is_sandbox_host_leaves_every_other_host_alone() {
        for host in ["example.com", "127.0.0.2", "app.localhost", "localhost.example.com", "10.0.0.1", "0.0.0.0", ""] {
            assert!(!is_sandbox_host(host), "{host:?} should not mean the sandbox");
        }
    }

    #[tokio::test]
    async fn test_check_load_lets_a_routed_context_load_localhost() {
        for url in ["http://localhost:3000/", "http://127.0.0.1:5173/x", "http://[::1]:8000/"] {
            assert_eq!(check_load(url, is_safe_fetch_addr, true).await, Ok(()), "{url}");
        }
    }

    #[test]
    fn test_sandbox_host_maps_localhost_and_container_addresses_into_the_pod() {
        use crate::sandbox::PodHost;
        assert_eq!(sandbox_host("localhost"), Some(PodHost::Localhost));
        assert_eq!(sandbox_host("[::1]"), Some(PodHost::Localhost));
        assert_eq!(
            sandbox_host("172.21.0.2"),
            Some(PodHost::Container(std::net::Ipv4Addr::new(172, 21, 0, 2)))
        );
        for host in ["example.com", "172.24.0.1", "172.16.0.5", "10.43.0.1", "web"] {
            assert_eq!(sandbox_host(host), None, "{host:?}");
        }
    }

    #[tokio::test]
    async fn test_check_load_lets_a_routed_context_load_a_container_address() {
        assert_eq!(check_load("http://172.21.0.2:3000/", is_safe_fetch_addr, true).await, Ok(()));
    }

    #[tokio::test]
    async fn test_check_load_refuses_a_container_address_without_a_sandbox_route() {
        assert!(check_load("http://172.21.0.2:3000/", is_safe_fetch_addr, false).await.is_err());
    }

    #[tokio::test]
    async fn test_check_load_refuses_localhost_without_a_sandbox_route() {
        let error = check_load("http://localhost:3000/", is_safe_fetch_addr, false)
            .await
            .expect_err("loopback without a route must be refused");
        assert!(error.contains("refused address"), "got {error:?}");
    }

    #[tokio::test]
    async fn test_check_load_still_refuses_other_private_hosts_in_a_routed_context() {
        for url in [
            "http://10.0.0.5/",
            "http://169.254.169.254/",
            "http://127.0.0.2:3000/",
            // Private, but outside the pod's Docker range.
            "http://172.16.0.5/",
            "http://172.24.0.1/",
        ] {
            assert!(check_load(url, is_safe_fetch_addr, true).await.is_err(), "{url}");
        }
    }

    #[tokio::test]
    async fn test_check_load_refuses_non_http_schemes_even_when_routed() {
        assert!(check_load("file:///etc/passwd", is_safe_fetch_addr, true).await.is_err());
        assert!(check_load("data:text/html,hi", is_safe_fetch_addr, true).await.is_err());
    }

    #[tokio::test]
    async fn test_check_load_allows_a_public_address() {
        assert_eq!(check_load("http://1.1.1.1/", is_safe_fetch_addr, false).await, Ok(()));
    }

    fn source(method: &str, origin: Option<&str>, site: Option<&str>, mode: Option<&str>) -> RequestSource {
        RequestSource {
            method: method.to_string(),
            origin: origin.map(str::to_string),
            sec_fetch_site: site.map(str::to_string),
            sec_fetch_mode: mode.map(str::to_string),
        }
    }

    fn sandbox_home(host: &str, _port: Option<u16>) -> bool {
        is_sandbox_host(host)
    }

    /// The header combinations Chrome really sent through the sandbox
    /// route (SME-42's review spike).
    #[test]
    fn test_requests_from_the_sandbox_or_the_model_are_allowed() {
        for (what, s) in [
            ("the model's navigation", source("GET", None, Some("none"), Some("navigate"))),
            ("the page's own POST", source("POST", Some("http://localhost:4400"), Some("same-origin"), Some("cors"))),
            ("another sandbox port", source("GET", Some("http://localhost:4400"), Some("same-site"), Some("cors"))),
            ("the page's WebSocket", source("GET", Some("http://localhost:4400"), None, None)),
            ("a page on 127.0.0.1", source("GET", Some("http://127.0.0.1:5173"), Some("cross-site"), Some("cors"))),
            ("a link from another site", source("GET", None, Some("cross-site"), Some("navigate"))),
            ("not a browser", source("POST", None, None, None)),
        ] {
            assert!(allows_request_from(&s, sandbox_home), "{what} should be allowed");
        }
    }

    #[test]
    fn test_requests_from_other_sites_are_refused() {
        for (what, s) in [
            ("another site's image", source("GET", None, Some("cross-site"), Some("no-cors"))),
            ("another site's POST", source("POST", Some("http://127.0.0.2:7000"), Some("cross-site"), Some("no-cors"))),
            ("another site's WebSocket", source("GET", Some("http://127.0.0.2:7000"), None, None)),
            ("another site's form POST", source("POST", Some("https://evil.example"), Some("cross-site"), Some("navigate"))),
            ("a cross-site POST page load", source("POST", None, Some("cross-site"), Some("navigate"))),
            ("an opaque origin", source("POST", Some("null"), Some("cross-site"), Some("cors"))),
        ] {
            assert!(!allows_request_from(&s, sandbox_home), "{what} should be refused");
        }
    }
}
