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
/// `is_addr_allowed` (via `is_request_allowed_with`), continuing it if
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
            let allowed = is_request_allowed_with(&event.request.url, is_addr_allowed).await;
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
/// to? `false` for loopback/link-local/private (RFC 1918)/unspecified/
/// multicast — every category of "not really an arbitrary public host."
/// IPv6 has no stable `is_private()`-equivalent for unique-local
/// (`fc00::/7`) addresses, so that range is checked manually.
pub fn is_safe_fetch_addr(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => {
            !(v4.is_loopback()
                || v4.is_link_local()
                || v4.is_private()
                || v4.is_unspecified()
                || v4.is_multicast())
        }
        IpAddr::V6(v6) => {
            let is_unique_local = (v6.segments()[0] & 0xfe00) == 0xfc00;
            !(v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || is_unique_local
                || v6.to_ipv4_mapped().is_some_and(|v4| !is_safe_fetch_addr(IpAddr::V4(v4))))
        }
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
/// every time. Real callers (`webfetch`'s interception loop,
/// `http_request`'s redirect loop) call `is_request_allowed_with` directly
/// with whichever predicate that specific call was given.
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
pub async fn is_request_allowed_with(url: &str, is_addr_allowed: fn(IpAddr) -> bool) -> bool {
    let Ok((host, port)) = parse_fetch_target(url) else {
        return false;
    };
    resolve_allowed(&host, port, is_addr_allowed).await.is_ok()
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
    let addrs: Vec<std::net::SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|e| format!("could not resolve {host}: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("{host} resolved to no addresses"));
    }
    if let Some(refused) = addrs.iter().find(|a| !is_addr_allowed(a.ip())) {
        return Err(format!("{host} resolves to a refused address ({})", refused.ip()));
    }
    Ok(addrs)
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
}
