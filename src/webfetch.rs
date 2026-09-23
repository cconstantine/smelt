//! `webfetch`: navigates a real headless browser (`chromiumoxide`, driving
//! `chrome-headless-shell` over CDP — the same binary/setup
//! `src/browser_tests.rs` already uses) to a URL and returns its rendered,
//! readable text. See docs/projects/plans/webfetch.md for the full design,
//! including why a real browser (JS execution included) rather than a
//! plain HTTP GET, and why every request the page makes — not just the
//! top-level navigation — gets checked against an SSRF guard via the CDP
//! Fetch domain. The guard itself (and response-text truncation) lives in
//! `src/fetch_guard.rs`, shared with `src/http_request.rs`'s plain-HTTP
//! tool — one source of truth for SSRF logic, not two copies that could
//! drift apart.

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use chromiumoxide::Browser;
use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::OnceCell;

use crate::fetch_guard::{self, is_safe_fetch_addr};

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

/// `pub(crate)` — `src/browsing.rs`'s persistent sessions run on this same
/// shared browser instance rather than launching a second one.
pub(crate) async fn shared_browser() -> Result<&'static Browser, String> {
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
/// subresources, JS-initiated fetches) is checked via
/// `fetch_guard::is_request_allowed_with` through the CDP Fetch domain
/// before being let through — see the plan's "SSRF guard via CDP request
/// interception" for why this has to cover more than just the top-level
/// URL once the fetch is a real, JS-executing browser rather than a plain
/// HTTP GET.
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
    // The request interceptor only sees loads that touch the network, so it
    // can't stop a `data:` (or similar) URL — check the scheme here.
    fetch_guard::parse_fetch_target(url)?;
    let browser = shared_browser().await?;
    let page = browser
        .new_page("about:blank")
        .await
        .map_err(|e| format!("failed to open a page: {e}"))?;

    let intercept_task = fetch_guard::spawn_request_interceptor(&page, is_addr_allowed).await?;

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

    let (text, truncated) = fetch_guard::truncate(text, MAX_TEXT_CHARS);
    Ok(FetchResult {
        url: url.to_string(),
        text,
        truncated,
    })
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
    /// `docs/testing.md` already documents. Found by hitting it directly:
    /// an earlier version of this suite split these into three separate
    /// `#[tokio::test]` fns, and the third one (whichever ran after the
    /// first had already launched the shared browser) failed with exactly
    /// that error.
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

        // --- Scenario 2b: a data: URL is refused too — it never touches
        // the network, so the request interceptor can't be what stops it. ---
        let result = fetch("data:text/html,<h1>DATA-SCHEME-LOADED</h1>").await;
        assert!(result.is_err(), "expected a data: URL to be refused, got: {result:?}");

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

        // `src/browsing.rs`'s own real-browser scenarios run on this same
        // shared browser static — see that module's `browser_tests` doc
        // comment for why they run here, as a plain async fn, rather than
        // as their own `#[tokio::test]`.
        crate::browsing::browser_tests::run_session_scenarios().await;
    }
}
