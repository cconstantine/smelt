//! `webfetch`: navigates a real headless browser (`chromiumoxide`, driving
//! `chrome-headless-shell` over CDP — the same binary/setup
//! `src/browser_tests.rs` already uses) to a URL and returns its rendered,
//! readable text. See docs/projects/plans/webfetch.md for the full design,
//! including why a real browser (JS execution included) rather than a
//! plain HTTP GET, and why every request the page makes — not just the
//! top-level navigation — gets checked against an SSRF guard via the CDP
//! Fetch domain.

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use chromiumoxide::Browser;
use chromiumoxide::cdp::browser_protocol::fetch::{
    ContinueRequestParams, EnableParams, EventRequestPaused, FailRequestParams,
};
use chromiumoxide::cdp::browser_protocol::network::ErrorReason;
use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::OnceCell;

const CHROME_BINARY: &str =
    ".browser-check-cache/chrome/chrome-headless-shell-linux64/chrome-headless-shell";
const LIB_DIR: &str = ".browser-check-cache/libs/usr/lib/x86_64-linux-gnu";
/// Bounds page navigation+load — "bound the boundaries" (development-process.md):
/// a slow/hanging page shouldn't tie up a tool call indefinitely.
const NAV_TIMEOUT: Duration = Duration::from_secs(20);
/// Final extracted-text cap, same shape `fetch_command_summary`'s tail
/// truncation already established elsewhere in this codebase — a single
/// lookup, not a paginated stream, so one truncation flag fits better than
/// offset/limit.
const MAX_TEXT_CHARS: usize = 20_000;

#[derive(Debug, Serialize, PartialEq)]
pub struct FetchResult {
    pub url: String,
    pub text: String,
    pub truncated: bool,
}

static BROWSER: OnceCell<Browser> = OnceCell::const_new();

async fn shared_browser() -> Result<&'static Browser, String> {
    BROWSER.get_or_try_init(launch_browser).await
}

/// Launches the shared `chrome-headless-shell` instance — lazily, on first
/// `webfetch` call, not at server startup, so a server that never uses
/// `webfetch` never pays for a running Chrome process. Mirrors
/// `src/browser_tests.rs`'s own launch config, but passes
/// `LD_LIBRARY_PATH` via `BrowserConfigBuilder::env` (scoped to just the
/// spawned child process) rather than mutating this process's whole
/// environment — `browser_tests.rs` needed `unsafe` `std::env::set_var`
/// for that because chromiumoxide's builder appeared to have no env hook
/// at the time; it does (`.env`/`.envs`, confirmed against the vendored
/// 0.7.0 source), which avoids the unsafe/whole-process-env-mutation
/// concern entirely for this always-concurrent (lazy, not startup-time)
/// call site.
async fn launch_browser() -> Result<Browser, String> {
    let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let chrome_binary = repo_root.join(CHROME_BINARY);
    if !chrome_binary.is_file() {
        return Err(format!(
            "chrome-headless-shell not found at {} — run scripts/browser-check/setup.sh first",
            chrome_binary.display()
        ));
    }
    let lib_dir = repo_root.join(LIB_DIR);
    let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
    let ld_library_path = format!("{}:{}/dri:{existing}", lib_dir.display(), lib_dir.display());

    let config = chromiumoxide::BrowserConfig::builder()
        .chrome_executable(&chrome_binary)
        .no_sandbox()
        .arg("--disable-gpu")
        .env("LD_LIBRARY_PATH", ld_library_path)
        .window_size(1400, 900)
        .build()
        .map_err(|e| format!("invalid chrome-headless-shell launch config: {e}"))?;
    let (browser, mut handler) = Browser::launch(config)
        .await
        .map_err(|e| format!("chrome-headless-shell failed to launch: {e}"))?;
    // chromiumoxide requires this driven continuously to process the CDP
    // connection at all (command responses, events) — same pattern
    // `src/browser_tests.rs`'s own harness uses.
    tokio::spawn(async move { while handler.next().await.is_some() {} });
    Ok(browser)
}

/// Fetches `url` in a real browser and returns its rendered readable text.
/// Every request the page makes (this navigation, any redirect,
/// subresources, JS-initiated fetches) is checked via `is_request_allowed`
/// through the CDP Fetch domain before being let through — see the plan's
/// "SSRF guard via CDP request interception" for why this has to cover
/// more than just the top-level URL once the fetch is a real, JS-executing
/// browser rather than a plain HTTP GET.
pub async fn fetch(url: &str) -> Result<FetchResult, String> {
    fetch_with_guard(url, is_safe_fetch_addr).await
}

/// The real implementation behind `fetch`, parameterized on the
/// address-safety predicate purely so tests can exercise a relaxed variant
/// (allowing loopback, where this test suite's own local fixture server
/// necessarily lives — every locally-reachable address in this dev
/// container is either loopback or RFC1918-private, so there is no way to
/// stand up a "legitimate" test page without *some* seam here) while the
/// real `fetch` above always uses the strict, real `is_safe_fetch_addr`.
async fn fetch_with_guard(
    url: &str,
    is_addr_allowed: fn(IpAddr) -> bool,
) -> Result<FetchResult, String> {
    let browser = shared_browser().await?;
    let page = browser
        .new_page("about:blank")
        .await
        .map_err(|e| format!("failed to open a page: {e}"))?;

    page.execute(EnableParams::default())
        .await
        .map_err(|e| format!("failed to enable request interception: {e}"))?;
    let mut paused = page
        .event_listener::<EventRequestPaused>()
        .await
        .map_err(|e| format!("failed to listen for intercepted requests: {e}"))?;
    let intercept_page = page.clone();
    let intercept_task = tokio::spawn(async move {
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
                tracing::warn!("webfetch: failed to resolve intercepted request: {e}");
            }
        }
    });

    // `goto` itself already resolves only once the navigated URL is fully
    // loaded (confirmed against the vendored source's own doc comment) —
    // an extra `wait_for_navigation()` call after it waits on a *second*
    // navigation event that will never come for a single `goto`, and hangs
    // forever. Found the hard way: the first real run of this function
    // hung indefinitely until this redundant call was removed.
    let nav_result = tokio::time::timeout(NAV_TIMEOUT, async {
        page.goto(url).await?;
        page.evaluate("document.body.innerText").await
    })
    .await;

    intercept_task.abort();
    let _ = page.close().await;

    let value = match nav_result {
        Ok(Ok(v)) => v,
        Ok(Err(e)) => return Err(format!("failed to load {url}: {e}")),
        Err(_) => return Err(format!("timed out loading {url}")),
    };
    let text: String = value
        .into_value()
        .map_err(|e| format!("failed to read page content: {e}"))?;

    let (text, truncated) = truncate_text(text);
    Ok(FetchResult {
        url: url.to_string(),
        text,
        truncated,
    })
}

/// Caps `text` to `MAX_TEXT_CHARS`, same shape `fetch_command_summary`'s
/// tail truncation already established — a pure function, split out so
/// it's TDD-able without a real browser in the loop at all.
fn truncate_text(text: String) -> (String, bool) {
    if text.chars().count() > MAX_TEXT_CHARS {
        (text.chars().take(MAX_TEXT_CHARS).collect(), true)
    } else {
        (text, false)
    }
}

/// The core SSRF check: is `addr` safe to let the browser actually connect
/// to? `false` for loopback/link-local/private (RFC 1918)/unspecified/
/// multicast — every category of "not really an arbitrary public host."
/// IPv6 has no stable `is_private()`-equivalent for unique-local
/// (`fc00::/7`) addresses, so that range is checked manually — see
/// docs/projects/plans/webfetch.md's "Open questions."
fn is_safe_fetch_addr(addr: IpAddr) -> bool {
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
/// scheme (`file://`, `data:`, `javascript:`, ...) outright — the CDP
/// Fetch-domain interceptor calls this on *every* request the page makes
/// (top-level navigation, redirects, subresources, JS-initiated fetches),
/// not just the URL the model asked for.
fn parse_fetch_target(url: &str) -> Result<(String, u16), String> {
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
/// every time. The interception loop in `fetch_with_guard` calls
/// `is_request_allowed_with` directly (with whichever predicate that
/// specific `fetch`/`fetch_with_guard` call was given), never this.
#[cfg(test)]
async fn is_request_allowed(url: &str) -> bool {
    is_request_allowed_with(url, is_safe_fetch_addr).await
}

/// The real SSRF gate: parses `url`, resolves its host, and checks every
/// resolved address via `is_addr_allowed`. Called once per request the CDP
/// Fetch-domain interceptor pauses (navigation, redirect, subresource, or
/// JS-initiated fetch alike) — see `fetch_with_guard`'s interception loop.
/// A resolution failure or an empty address list is treated the same as
/// an unsafe address: `false`, not "assume fine." Parameterized on the
/// address-safety predicate purely for testability — see
/// `fetch_with_guard`'s doc comment for why that seam exists.
async fn is_request_allowed_with(url: &str, is_addr_allowed: fn(IpAddr) -> bool) -> bool {
    let Ok((host, port)) = parse_fetch_target(url) else {
        return false;
    };
    match tokio::net::lookup_host((host.as_str(), port)).await {
        Ok(addrs) => {
            let addrs: Vec<IpAddr> = addrs.map(|sa| sa.ip()).collect();
            !addrs.is_empty() && addrs.iter().all(|&a| is_addr_allowed(a))
        }
        Err(_) => false,
    }
}

/// Real, `chrome-headless-shell`-backed tests — needs `scripts/browser-check/setup.sh`
/// to have run first, same as `src/browser_tests.rs`. `#[ignore]`d and gated
/// behind the same `browser-test` feature for the same reason: not
/// something a plain `cargo test --features server` run should depend on
/// having a real browser binary available.
#[cfg(all(test, feature = "browser-test"))]
mod browser_tests {
    use super::*;

    /// Every locally-reachable address in this dev container is either
    /// loopback or RFC1918-private, so there is no way to serve a
    /// "legitimate" fixture page for these tests without loosening the
    /// guard *somewhere* — this loosens it only for loopback, only in
    /// these tests, never in `fetch`'s own real default (`is_safe_fetch_addr`).
    fn allow_loopback_too(addr: IpAddr) -> bool {
        is_safe_fetch_addr(addr) || addr.is_loopback()
    }

    async fn start_test_server(body: &'static str) -> (String, tokio::task::JoinHandle<()>) {
        let router = axum::Router::new().route(
            "/",
            axum::routing::get(move || async move {
                axum::response::Html(body)
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a test-local port");
        let port = listener.local_addr().expect("local addr").port();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("test server error");
        });
        (format!("http://127.0.0.1:{port}/"), task)
    }

    /// Deliberately one test, not several — same reason
    /// `src/browser_tests.rs`'s own doc comment already names: each
    /// `#[tokio::test]` fn gets its *own* independent tokio runtime, and
    /// the shared `BROWSER` static's background handler task is tied to
    /// whichever runtime first launched it; once that runtime tears down
    /// at the end of *that* test function, the handler task dies with it,
    /// and any later test reusing the same static from a *different*
    /// runtime gets "send failed because receiver is gone" — the same
    /// `OnceLock`/`OnceCell`-across-separate-runtimes hazard
    /// `docs/testing.md` already documents for `PgPool`. Found by hitting
    /// it directly: an earlier version of this suite split these into
    /// three separate `#[tokio::test]` fns, and the third one (whichever
    /// ran after the first had already launched the shared browser) failed
    /// with exactly that error.
    #[tokio::test]
    #[ignore]
    async fn test_fetch_scenarios() {
        // --- Scenario 1: a real page's rendered, readable text comes back. ---
        let (url, _server) =
            start_test_server("<html><body><h1>Hello from a real page</h1></body></html>").await;
        let result = fetch_with_guard(&url, allow_loopback_too)
            .await
            .expect("fetch should succeed");
        assert_eq!(result.url, url);
        assert!(
            result.text.contains("Hello from a real page"),
            "expected the rendered text, got: {:?}",
            result.text
        );
        assert!(!result.truncated);

        // --- Scenario 2: navigating straight to a loopback address is refused. ---
        let result = fetch("http://127.0.0.1:1/").await;
        assert!(
            result.is_err(),
            "expected navigating straight to a loopback address to be refused"
        );

        // --- Scenario 3: a page-initiated (JS `fetch()`) request to a
        // private address is blocked too — the case a plain top-level-URL
        // check (the original, reqwest-based plan) couldn't cover. Proves
        // the CDP Fetch-domain interception covers subresource/JS-initiated
        // requests, not just the top-level navigation. ---
        let (url, _server) = start_test_server(
            "<html><body><div id=\"out\">pending</div><script>
                fetch('http://169.254.169.254/').then(
                    () => { document.getElementById('out').innerText = 'reached'; },
                    () => { document.getElementById('out').innerText = 'blocked'; }
                );
            </script></body></html>",
        )
        .await;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let result = fetch_with_guard(&url, allow_loopback_too)
                .await
                .expect("fetch should succeed");
            if result.text.contains("blocked") {
                break;
            }
            assert!(
                !result.text.contains("reached"),
                "the page-initiated request to a private address should have been blocked, \
                 got: {:?}",
                result.text
            );
            if tokio::time::Instant::now() >= deadline {
                panic!("script never finished; last text: {:?}", result.text);
            }
        }
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
    fn test_truncate_text_leaves_short_text_untouched() {
        let (text, truncated) = truncate_text("hello".to_string());
        assert_eq!(text, "hello");
        assert!(!truncated);
    }

    #[test]
    fn test_truncate_text_caps_long_text_and_flags_it() {
        let long = "a".repeat(MAX_TEXT_CHARS + 500);
        let (text, truncated) = truncate_text(long);
        assert_eq!(text.chars().count(), MAX_TEXT_CHARS);
        assert!(truncated);
    }
}
