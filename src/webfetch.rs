//! `webfetch`: navigates a real headless browser (`chromiumoxide`, driving
//! `chrome-headless-shell` over CDP — the same binary/setup
//! `src/browser_tests.rs` already uses) to a URL and returns its rendered,
//! readable text. See SME-21 for the full design,
//! including why a real browser (JS execution included) rather than a
//! plain HTTP GET, and why every request the page makes — not just the
//! top-level navigation — gets checked against an SSRF guard via the CDP
//! Fetch domain — and, since that only covers the one page, why the browser
//! itself runs behind `crate::egress_proxy`. The guard itself (and
//! response-text truncation) lives in
//! `src/fetch_guard.rs`, shared with `src/http_request.rs`'s plain-HTTP
//! tool — one source of truth for SSRF logic, not two copies that could
//! drift apart.

use std::net::IpAddr;
use std::time::Duration;

use chromiumoxide::cdp::browser_protocol::browser::BrowserContextId;
use chromiumoxide::cdp::browser_protocol::target::{CreateBrowserContextParams, CreateTargetParams};
use chromiumoxide::{Browser, Page};
use serde::Serialize;
use tokio::sync::OnceCell;

use crate::fetch_guard::{self, is_safe_fetch_addr};

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

/// What the browser-wide egress proxy (`crate::egress_proxy`) lets through.
/// Test builds also allow loopback, where every test fixture server lives;
/// the strict per-call guard (`fetch_guard::spawn_request_interceptor`)
/// still applies on top in every build.
#[cfg(not(test))]
const BROWSER_EGRESS_GUARD: fn(IpAddr) -> bool = is_safe_fetch_addr;
#[cfg(test)]
const BROWSER_EGRESS_GUARD: fn(IpAddr) -> bool =
    |addr| is_safe_fetch_addr(addr) || addr.is_loopback();

/// `pub(crate)` — `src/browsing.rs`'s persistent sessions run on this same
/// shared browser instance rather than launching a second one.
pub(crate) async fn shared_browser() -> Result<&'static Browser, String> {
    BROWSER.get_or_try_init(launch_browser).await
}

/// A blank page in a browser context of its own: its own cookies, site
/// storage, cache and service workers, shared with no other page. Pass
/// the context to `dispose_isolated` when done, which deletes all of that
/// along with the page — nothing one conversation (or one `webfetch` call)
/// does in the browser is visible to another.
pub(crate) async fn new_isolated_page() -> Result<(Page, BrowserContextId), String> {
    let browser = shared_browser().await?;
    let context = browser
        .create_browser_context(CreateBrowserContextParams::default())
        .await
        .map_err(|e| format!("failed to create a browser context: {e}"))?;
    let mut target = CreateTargetParams::new("about:blank");
    target.browser_context_id = Some(context.clone());
    match browser.new_page(target).await {
        Ok(page) => Ok((page, context)),
        Err(e) => {
            dispose_isolated(context).await;
            Err(format!("failed to open a page: {e}"))
        }
    }
}

/// Deletes a context from `new_isolated_page`, closing its pages and
/// discarding everything stored in it.
pub(crate) async fn dispose_isolated(context: BrowserContextId) {
    if let Ok(browser) = shared_browser().await
        && let Err(e) = browser.dispose_browser_context(context).await
    {
        tracing::warn!("failed to dispose a browser context: {e}");
    }
}

/// Launches the shared `chrome-headless-shell` instance — lazily, on first
/// use, not at server startup, so a server that never browses never pays for
/// a running Chrome. Launched via `crate::headless_chrome`, so it dies with
/// this process.
async fn launch_browser() -> Result<Browser, String> {
    // All of the browser's traffic goes through the egress proxy, which
    // applies the SSRF guard to everything — including popups, WebSockets
    // and service workers, which per-page CDP interception never sees.
    // `<-loopback>` stops Chrome's default of skipping the proxy for
    // localhost. Popups are refused outright rather than left to pile up,
    // and WebRTC may not send UDP around the proxy.
    let proxy = crate::egress_proxy::start(BROWSER_EGRESS_GUARD)
        .await
        .map_err(|e| format!("failed to start the browser's egress proxy: {e}"))?;
    crate::headless_chrome::launch(&[
        format!("--proxy-server=http://{proxy}"),
        "--proxy-bypass-list=<-loopback>".to_string(),
        "--block-new-web-contents".to_string(),
        "--force-webrtc-ip-handling-policy=disable_non_proxied_udp".to_string(),
    ])
    .await
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
    let (host, port) = fetch_guard::parse_fetch_target(url)?;
    // Refuse a private or local address before loading, with a reason the
    // model can act on rather than Chrome's "net::ERR_BLOCKED_BY_CLIENT"
    // (SME-40 F15). The interceptor still guards everything the page loads.
    fetch_guard::resolve_allowed(&host, port, is_addr_allowed)
        .await
        .map_err(|e| format!("refused {url}: {e}; smelt doesn't load private or local addresses"))?;
    let (page, context) = new_isolated_page().await?;
    let intercept_task = match fetch_guard::spawn_request_interceptor(&page, is_addr_allowed).await {
        Ok(task) => task,
        Err(e) => {
            dispose_isolated(context).await;
            return Err(e);
        }
    };

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
    dispose_isolated(context).await;

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

    const OWNER_HELPER_ENV: &str = "SMELT_CHROME_OWNER_HELPER";

    /// Not a test on its own: `test_chrome_exits_with_its_owning_process`
    /// runs this in a child process (with `OWNER_HELPER_ENV` set) to own a
    /// shared browser it can then kill. A no-op otherwise.
    #[tokio::test]
    #[ignore]
    async fn chrome_owner_helper() {
        if std::env::var(OWNER_HELPER_ENV).is_err() {
            return;
        }
        shared_browser().await.expect("launch the shared browser");
        println!("CHROME_OWNER_READY");
        std::future::pending::<()>().await;
    }

    /// Pids of `parent`'s direct children that are Chrome processes.
    fn chrome_children_of(parent: u32) -> Vec<u32> {
        std::fs::read_dir("/proc")
            .expect("read /proc")
            .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
            .filter(|pid| {
                let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
                // Fields after the parenthesised command name: state, ppid, ...
                let ppid = stat
                    .rsplit_once(')')
                    .and_then(|(_, rest)| rest.split_whitespace().nth(1)?.parse::<u32>().ok());
                let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
                ppid == Some(parent) && String::from_utf8_lossy(&cmdline).contains("chrome-headless-shell")
            })
            .collect()
    }

    /// Alive and not a zombie waiting to be reaped.
    fn process_is_running(pid: u32) -> bool {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| stat.rsplit_once(')').map(|(_, rest)| rest.trim_start().starts_with('Z')))
            .is_some_and(|zombie| !zombie)
    }

    /// The shared browser is a process-wide static that's never dropped, so
    /// nothing in smelt shuts Chrome down when smelt exits — every server
    /// restart and test run used to leave a whole Chrome behind. Kills an
    /// owning process the hard way (SIGKILL, as a dev-server rebuild does)
    /// and checks its Chrome goes with it.
    #[tokio::test]
    #[ignore]
    async fn test_chrome_exits_with_its_owning_process() {
        use std::io::BufRead;
        let mut owner = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "webfetch::browser_tests::chrome_owner_helper",
                "--exact",
                "--ignored",
                "--nocapture",
            ])
            .env(OWNER_HELPER_ENV, "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn the owning process");
        let stdout = owner.stdout.take().expect("owner stdout");
        let ready = std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
            .any(|line| line.contains("CHROME_OWNER_READY"));
        assert!(ready, "the owning process never launched its browser");
        let chrome = chrome_children_of(owner.id());
        assert_eq!(chrome.len(), 1, "expected exactly one Chrome under the owner, got {chrome:?}");
        let chrome = chrome[0];

        owner.kill().expect("SIGKILL the owner");
        owner.wait().expect("reap the owner");

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while process_is_running(chrome) && std::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let survived = process_is_running(chrome);
        if survived {
            // Don't leave it behind ourselves.
            let _ = std::process::Command::new("kill").arg(chrome.to_string()).status();
        }
        assert!(!survived, "Chrome (pid {chrome}) outlived the process that launched it");
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
        let proxied_before = crate::egress_proxy::requests_handled();
        let result = fetch_with_guard(&url, allow_loopback_too)
            .await
            .expect("fetch should succeed");
        assert!(
            crate::egress_proxy::requests_handled() > proxied_before,
            "the browser reached a loopback page without going through the egress proxy"
        );
        assert_eq!(result.url, url);
        assert!(
            result.text.contains("Hello from a real page"),
            "expected the rendered text, got: {:?}",
            result.text
        );
        assert!(!result.truncated);

        // --- Scenario 2: navigating straight to a loopback address is refused. ---
        let result = fetch("http://127.0.0.1:1/").await;
        let error = result.expect_err("expected navigating straight to a loopback address to be refused");
        // And says why, not Chrome's "net::ERR_BLOCKED_BY_CLIENT" (SME-40 F15).
        assert!(error.contains("private or local"), "the refusal should say why: {error}");

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

        // --- Scenario 4: a fetched page can't reach a refused address by
        // any route the per-page interception doesn't see — a WebSocket or
        // a service worker, both started as soon as the page loads. ---
        let forbidden = crate::browsing::browser_tests::start_forbidden_server().await;
        let (escape_url, _escape_server) =
            crate::browsing::browser_tests::start_escape_test_server(&forbidden).await;
        fetch_with_guard(&escape_url, allow_loopback_too)
            .await
            .expect("fetching the escape page should succeed");
        tokio::time::sleep(Duration::from_secs(3)).await;
        let hits = forbidden.hits.lock().unwrap().clone();
        assert!(
            hits.is_empty(),
            "a fetched page reached the forbidden address {}: {hits:?}",
            forbidden.addr
        );

        // --- Scenario 5: one webfetch call leaves nothing behind for the
        // next — not cookies, not site storage — so nothing leaks between
        // conversations through webfetch either. ---
        {
            use crate::browsing::browser_tests::{SEES_NO_DATA, SHOW_DATA_PAGE};
            let router = axum::Router::new()
                .route(
                    "/set-data",
                    axum::routing::get(|| async {
                        (
                            [(axum::http::header::SET_COOKIE, "smelt_probe=from-first; Path=/")],
                            axum::response::Html(
                                "<html><body>data set<script>\
                                 localStorage.setItem('smelt_probe', 'from-first');\
                                 </script></body></html>",
                            ),
                        )
                    }),
                )
                .route(
                    "/show-data",
                    axum::routing::get(|| async { axum::response::Html(SHOW_DATA_PAGE) }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind a test-local port");
            let base = format!("http://{}", listener.local_addr().expect("local addr"));
            let _server = tokio::spawn(async move {
                axum::serve(listener, router).await.expect("test server error");
            });
            fetch_with_guard(&format!("{base}/set-data"), allow_loopback_too)
                .await
                .expect("the first fetch should succeed");
            let second = fetch_with_guard(&format!("{base}/show-data"), allow_loopback_too)
                .await
                .expect("the second fetch should succeed");
            assert!(
                second.text.contains(SEES_NO_DATA),
                "a webfetch call saw data left by an earlier one: {:?}",
                second.text
            );
        }

        // `src/browsing.rs`'s own real-browser scenarios run on this same
        // shared browser static — see that module's `browser_tests` doc
        // comment for why they run here, as a plain async fn, rather than
        // as their own `#[tokio::test]`.
        crate::browsing::browser_tests::run_session_scenarios().await;
    }
}
