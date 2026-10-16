//! Shared helpers for anything that makes an outbound HTTP-ish request on
//! the model's behalf from smelt's own server process
//! (`src/webfetch.rs`'s real-browser `webfetch`, `src/http_request.rs`'s
//! plain-HTTP `http_request`, `src/browsing.rs`'s sessions): the URL parse
//! every one of them does, the sandbox-route helpers, and response-size
//! capping. Kept as one module deliberately — one source of truth, not
//! several copies that could drift apart.
//!
//! There used to be an SSRF guard here too, refusing every address that
//! isn't the public internet; SME-155 removed it. What's left is not an
//! address restriction: which hosts mean the conversation's pod
//! (`sandbox_host`), who may send requests to a pod server
//! (`allows_request_from`), and the size cap.

/// Whether `host` names "this machine" the way a model writes it —
/// `localhost`, `127.0.0.1` or `::1` (bracketed or not). In a browser
/// context with a sandbox route, these mean the conversation's own pod
/// rather than smelt's server (SME-42). The one definition shared by the
/// egress proxy and the pre-navigation checks, so they can't drift apart.
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
    pub referer: Option<String>,
    /// `Upgrade-Insecure-Requests: 1`, which browsers send on page loads
    /// only and which a page can't add to its own requests.
    pub upgrade_insecure_requests: bool,
}

/// Whether a browser sends `Sec-Fetch-*` headers to `host` over plain
/// http: only to an address it counts as "potentially trustworthy", which
/// for http means `localhost`, a `*.localhost` name, or a loopback address.
/// A container's address isn't one, so requests to it carry none (SME-90).
pub fn browser_sends_sec_fetch(host: &str) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host = host.strip_suffix('.').unwrap_or(host).to_ascii_lowercase();
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
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
/// `Sec-Fetch-Site` decides: a cross-site request gets through only as a
/// GET/HEAD page load (a link someone followed). `same-site` says nothing
/// on its own: every preview is same-site with every other, and with smelt
/// when they share a domain. So a same-site request also needs to be a
/// GET/HEAD page load, or a GET/HEAD whose `Referer` is home (SME-173).
/// smelt's own preview links send no `Referer` (`noreferrer`), so they get
/// through as page loads.
///
/// `sends_sec_fetch` says whether the browser sends `Sec-Fetch-*` to the
/// address it used (`browser_sends_sec_fetch`). Where it does, a request
/// with neither header isn't from a browser page at all. Where it doesn't
/// (a container's plain-http address), another site's `<img>` arrives with
/// neither, so only a GET/HEAD page load (`Upgrade-Insecure-Requests`) or
/// a request whose `Referer` is home gets through (SME-90).
pub fn allows_request_from(
    source: &RequestSource,
    sends_sec_fetch: bool,
    is_home: impl Fn(&str, Option<u16>) -> bool,
) -> bool {
    // "null" (an opaque origin) and anything unparseable aren't home.
    let url_is_home = |url: &str| {
        url::Url::parse(url)
            .ok()
            .and_then(|url| url.host_str().map(|host| is_home(host, url.port())))
            .unwrap_or(false)
    };
    if let Some(origin) = &source.origin {
        return url_is_home(origin);
    }
    let get_or_head = matches!(source.method.to_ascii_uppercase().as_str(), "GET" | "HEAD");
    let navigation = get_or_head && source.sec_fetch_mode.as_deref() == Some("navigate");
    let referer_is_home = get_or_head && source.referer.as_deref().is_some_and(url_is_home);
    match source.sec_fetch_site.as_deref() {
        Some("same-origin" | "none") => true,
        // Every preview shares a site with the others (and with smelt when
        // they share a domain), so `same-site` alone says nothing (SME-173).
        Some("same-site") => navigation || referer_is_home,
        // `cross-site`, and any value browsers don't send.
        Some(_) => navigation,
        None if sends_sec_fetch => true,
        None => referer_is_home || (get_or_head && source.upgrade_insecure_requests),
    }
}

/// Parses `url` and, if it's a well-formed `http`/`https` URL, returns its
/// `(host, port)`. Rejects every other scheme (`file://`, `data:`,
/// `javascript:`, ...) outright — the check every fetch, request and
/// navigation runs before anything is loaded.
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

/// Resolves `host:port` and returns its addresses; a failed or empty
/// resolution is an error, never "assume fine." Callers that go on to
/// connect should connect to one of the returned addresses rather than
/// resolving again, so a DNS answer that changes between check and connect
/// can't slip through (SME-51 B4).
pub async fn resolve_target(host: &str, port: u16) -> Result<Vec<std::net::SocketAddr>, String> {
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
    Ok(addrs)
}

/// Names only `resolve_target` knows, for tests that need its answer to
/// differ from the system resolver's (as a DNS-rebinding name's would).
/// Always empty outside tests.
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

    #[tokio::test]
    async fn test_resolve_target_returns_a_resolved_name_and_errors_otherwise() {
        test_dns::set("resolve.smelt-test.invalid", vec!["10.0.0.5".parse().unwrap()]);
        let addrs = resolve_target("resolve.smelt-test.invalid", 80)
            .await
            .expect("the test entry should resolve");
        assert_eq!(addrs, vec![std::net::SocketAddr::new(
            std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 5)),
            80
        )]);
        let error = resolve_target("this-host-does-not-exist.invalid", 80)
            .await
            .expect_err("a name the system can't resolve is an error, not \"assume fine\"");
        assert!(error.contains("could not resolve"), "got {error:?}");
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

    fn source(method: &str, origin: Option<&str>, site: Option<&str>, mode: Option<&str>) -> RequestSource {
        RequestSource {
            method: method.to_string(),
            origin: origin.map(str::to_string),
            sec_fetch_site: site.map(str::to_string),
            sec_fetch_mode: mode.map(str::to_string),
            ..Default::default()
        }
    }

    /// What Chrome sends to a plain-http private address (a container's),
    /// where it sends no `Sec-Fetch-*`: a spike with the real
    /// chrome-headless-shell (SME-90).
    fn plain(method: &str, referer: Option<&str>, page_load: bool) -> RequestSource {
        RequestSource {
            method: method.to_string(),
            referer: referer.map(str::to_string),
            upgrade_insecure_requests: page_load,
            ..Default::default()
        }
    }

    fn sandbox_home(host: &str, _port: Option<u16>) -> bool {
        is_sandbox_host(host)
    }

    /// Home as the egress proxy's sandbox route judges it: the pod's
    /// localhost or one of its containers.
    fn sandbox_or_container(host: &str, _port: Option<u16>) -> bool {
        sandbox_host(host).is_some()
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
            assert!(allows_request_from(&s, true, sandbox_home), "{what} should be allowed");
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
            assert!(!allows_request_from(&s, true, sandbox_home), "{what} should be refused");
        }
    }

    /// `source` with the page's `Referer` and `Upgrade-Insecure-Requests`.
    fn from_page(site: &str, mode: &str, referer: Option<&str>, page_load: bool) -> RequestSource {
        RequestSource {
            referer: referer.map(str::to_string),
            upgrade_insecure_requests: page_load,
            ..source("GET", None, Some(site), Some(mode))
        }
    }

    /// Home as conversation 42's preview judges it: any of its own
    /// `<port>-42.preview.localhost` hosts.
    fn preview_42(host: &str, _port: Option<u16>) -> bool {
        host.strip_suffix(".preview.localhost")
            .and_then(|label| label.rsplit_once('-'))
            .is_some_and(|(_, conversation)| conversation == "42")
    }

    /// Every preview is same-site with every other (`preview.localhost` is
    /// one site), so `same-site` says nothing about who sent it. The header
    /// sets are the ones real Chromium 1243 sent in SME-173's spike.
    #[test]
    fn test_same_site_requests_from_other_previews_or_smelt_are_refused() {
        let cases = [
            ("conversation 7's <img>", from_page("same-site", "no-cors", Some("http://3000-7.preview.localhost:8181/"), false)),
            ("an <img> with no referrer (smelt's markdown, or a no-referrer page)", from_page("same-site", "no-cors", None, false)),
            ("conversation 7's no-cors fetch", from_page("same-site", "no-cors", Some("http://3000-7.preview.localhost:8181/app"), false)),
            ("an image on smelt's own page", from_page("same-site", "no-cors", Some("http://localhost:8180/c/1"), false)),
            ("an unparseable referrer", from_page("same-site", "no-cors", Some("not a url"), false)),
            ("a same-site POST with no Origin", RequestSource { method: "POST".to_string(), ..from_page("same-site", "navigate", None, true) }),
            ("an unknown Sec-Fetch-Site value", from_page("same-ish", "no-cors", None, false)),
        ];
        let let_in: Vec<&str> = cases
            .iter()
            .filter(|(_, s)| allows_request_from(s, true, preview_42))
            .map(|(what, _)| *what)
            .collect();
        assert!(let_in.is_empty(), "these should be refused: {let_in:#?}");
    }

    #[test]
    fn test_same_site_navigations_and_the_previews_own_pages_are_allowed() {
        for (what, s) in [
            ("the sandbox panel's link (noreferrer)", from_page("same-site", "navigate", None, true)),
            ("an <iframe> in smelt", from_page("same-site", "navigate", Some("http://localhost:8180/c/42"), false)),
            ("the preview's own script from :5173", from_page("same-site", "no-cors", Some("http://3000-42.preview.localhost:8181/"), false)),
            ("a url() in the preview's own stylesheet", from_page("same-site", "no-cors", Some("http://5173-42.preview.localhost:8181/style.css"), false)),
            ("the preview's own image", from_page("same-origin", "no-cors", Some("http://3000-42.preview.localhost:8181/"), false)),
            ("a typed URL", from_page("none", "navigate", None, true)),
        ] {
            assert!(allows_request_from(&s, true, preview_42), "{what} should be allowed");
        }
        // The model's browser: a sandbox page's image from another port.
        let image = from_page("same-site", "no-cors", Some("http://localhost:4400/"), false);
        assert!(allows_request_from(&image, true, sandbox_or_container), "the sandbox page's own image");
    }

    #[test]
    fn test_browsers_send_sec_fetch_only_to_localhost_and_loopback() {
        for host in ["localhost", "LOCALHOST", "3000-42.preview.localhost", "localhost.", "127.0.0.1", "127.1.2.3", "::1", "[::1]"] {
            assert!(browser_sends_sec_fetch(host), "{host}");
        }
        for host in ["172.20.0.2", "10.0.0.1", "evil.example", "3000-42.preview.example.com", "notlocalhost", "::ffff:172.20.0.2", "[fd00::1]"] {
            assert!(!browser_sends_sec_fetch(host), "{host}");
        }
    }

    #[test]
    fn test_without_sec_fetch_a_page_load_or_the_sandboxs_own_page_is_allowed() {
        let home = "http://172.20.0.2:8080/page";
        for (what, s) in [
            ("the model's navigation", plain("GET", None, true)),
            ("a page load's HEAD", plain("HEAD", None, true)),
            ("a link from another site", plain("GET", Some("http://evil.test:8703/"), true)),
            ("the page's own image", plain("GET", Some(home), false)),
            ("the page's own fetch", plain("GET", Some(home), false)),
            ("a page on localhost", plain("GET", Some("http://localhost:5173/"), false)),
            ("the page's own POST", source("POST", Some("http://172.20.0.2:8080"), None, None)),
        ] {
            assert!(allows_request_from(&s, false, sandbox_or_container), "{what} should be allowed");
        }
    }

    #[test]
    fn test_without_sec_fetch_other_sites_requests_are_refused() {
        for (what, s) in [
            ("another site's image", plain("GET", Some("http://evil.test:8703/"), false)),
            ("another site's image with no referrer", plain("GET", None, false)),
            ("another site's no-cors HEAD", plain("HEAD", None, false)),
            ("a POST claiming to be a page load", plain("POST", None, true)),
            ("a POST with only a home referrer", plain("POST", Some("http://172.20.0.2:8080/"), false)),
            ("an unparseable referrer", plain("GET", Some("not a url"), false)),
            ("another site's POST", source("POST", Some("http://evil.test:8703"), None, None)),
        ] {
            assert!(!allows_request_from(&s, false, sandbox_or_container), "{what} should be refused");
        }
    }
}
