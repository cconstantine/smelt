//! `http_request`: a plain HTTP client tool for hitting APIs directly — no
//! browser, no JS execution, much cheaper than `webfetch` for a JSON
//! endpoint or any page that doesn't need real rendering. Shares its SSRF
//! guard and response truncation with `webfetch` via `src/fetch_guard.rs`
//! — same reasoning as that module's own doc comment: SSRF logic should
//! have one source of truth. Unlike `webfetch`, `reqwest`'s own automatic
//! redirect-following is disabled and redirects are followed manually, one
//! hop at a time, re-checking each target against the SSRF guard — a
//! plain HTTP client never executes page JS, so there's no CDP
//! interception layer to lean on here; the guard has to sit at the
//! redirect-loop level instead.

use std::net::IpAddr;
use std::time::Duration;

use serde::Serialize;

use crate::fetch_guard;

/// Bounds each individual HTTP request — "bound the boundaries"
/// (development-process.md): a slow/hanging endpoint shouldn't tie up a
/// tool call indefinitely.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// A redirect chain longer than this is treated as a failure rather than
/// followed forever — same defensive-bound reasoning as `REQUEST_TIMEOUT`.
const MAX_REDIRECTS: u32 = 5;
/// Response body cap, same shape `webfetch`'s `MAX_TEXT_CHARS` and
/// `fetch_command_summary`'s tail truncation already established.
const MAX_BODY_CHARS: usize = 20_000;

#[derive(Debug, Serialize, PartialEq)]
pub struct HttpResponseResult {
    /// The URL the response actually came from — the original `url` after
    /// following any redirects, so the model can tell when it happened.
    pub url: String,
    pub status: u16,
    pub content_type: Option<String>,
    pub body: String,
    pub truncated: bool,
}

/// Issues a plain HTTP request and returns its response — no browser, no
/// JS execution. A non-2xx status is still a successful `Ok` (that's real
/// data the model should see, e.g. a 404 or a 500 with a JSON error body),
/// never a tool error; only a genuine transport/SSRF/timeout failure is.
pub async fn request(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
) -> Result<HttpResponseResult, String> {
    request_with_guard(method, url, headers, body, fetch_guard::is_safe_fetch_addr).await
}

/// The real implementation behind `request`, parameterized on the
/// address-safety predicate — same reason `webfetch::fetch_with_guard` has
/// this seam: every locally-reachable address in this dev container is
/// either loopback or RFC1918-private, so testing "does a legitimate
/// request actually get through" needs a relaxed variant, while the real
/// `request` above always uses the strict, real `is_safe_fetch_addr`.
async fn request_with_guard(
    method: &str,
    url: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    is_addr_allowed: fn(IpAddr) -> bool,
) -> Result<HttpResponseResult, String> {
    let method = parse_method(method)?;
    // `reqwest`'s own automatic redirect-following is disabled — unlike
    // `webfetch`'s CDP Fetch-domain interception, a plain HTTP client has
    // no per-request hook to check a redirect target *before* connecting
    // to it, so redirects are followed manually here, one hop at a time,
    // re-running the SSRF guard against each target.
    let mut current_url = url.to_string();
    let mut current_method = method;
    for _ in 0..=MAX_REDIRECTS {
        // Resolved once, and the client then connects only to those
        // addresses: letting reqwest resolve the name again would let a
        // DNS-rebinding name pass the check with one address and connect
        // to another (SME-51 B4).
        let checked = match fetch_guard::parse_fetch_target(&current_url) {
            Ok((host, port)) => fetch_guard::resolve_allowed(&host, port, is_addr_allowed)
                .await
                .ok()
                .map(|addrs| (host, addrs)),
            Err(_) => None,
        };
        let Some((host, addrs)) = checked else {
            // localhost means the sandbox only in the browser tools (SME-42):
            // say so, rather than leave the model guessing.
            let sandbox_hint = fetch_guard::parse_fetch_target(&current_url)
                .is_ok_and(|(host, _)| fetch_guard::is_sandbox_host(&host));
            return Err(if sandbox_hint {
                format!(
                    "refusing to request {current_url}: http_request can't reach localhost. For a \
                     server in your sandbox, use webfetch or a browsing session (there localhost \
                     is your sandbox), or curl in a terminal."
                )
            } else {
                format!("refusing to request {current_url}: not a safe address")
            });
        };
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(REQUEST_TIMEOUT)
            .resolve_to_addrs(&host, &addrs)
            .build()
            .map_err(|e| format!("failed to build HTTP client: {e}"))?;
        let mut req = client.request(current_method.clone(), &current_url);
        for (name, value) in headers {
            req = req.header(name, value);
        }
        if let Some(b) = body {
            req = req.body(b.to_string());
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("request to {current_url} failed: {e}"))?;

        if resp.status().is_redirection() {
            let location = resp
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|v| v.to_str().ok())
                .ok_or_else(|| {
                    format!("{current_url} returned a redirect with no Location header")
                })?;
            let next_url = resolve_redirect_target(&current_url, location)?;
            // A 303 always switches to GET regardless of the original
            // method (the one case the HTTP spec mandates a method
            // change on redirect); every other redirect status keeps the
            // original method.
            if resp.status().as_u16() == 303 {
                current_method = reqwest::Method::GET;
            }
            current_url = next_url;
            continue;
        }

        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        // Read only as far as the body is kept: a character is at most 4
        // bytes, so past that everything would be cut anyway (SME-51 B8).
        let limit = MAX_BODY_CHARS * 4;
        let mut bytes = Vec::new();
        let mut stopped_early = false;
        let mut resp = resp;
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| format!("failed to read response body: {e}"))?
        {
            bytes.extend_from_slice(&chunk);
            if bytes.len() >= limit {
                bytes.truncate(limit);
                stopped_early = true;
                break;
            }
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        let (body, cut) = fetch_guard::truncate(text, MAX_BODY_CHARS);
        let truncated = cut || stopped_early;
        return Ok(HttpResponseResult {
            url: current_url,
            status,
            content_type,
            body,
            truncated,
        });
    }
    Err(format!("too many redirects (> {MAX_REDIRECTS})"))
}

/// Resolves a `Location` header value against the URL it was a response
/// to — a redirect target is very often relative (`/c`, `c`, `//other.com/c`),
/// not a full URL, and the HTTP spec allows that.
fn resolve_redirect_target(base: &str, location: &str) -> Result<String, String> {
    let base_url = url::Url::parse(base).map_err(|e| format!("invalid base URL: {e}"))?;
    let resolved = base_url
        .join(location)
        .map_err(|e| format!("invalid redirect target {location:?}: {e}"))?;
    Ok(resolved.to_string())
}

/// Parses a model-supplied HTTP method string — `reqwest::Method`'s own
/// `FromStr` already handles case/validity, this just gives a clearer
/// error than its default.
fn parse_method(method: &str) -> Result<reqwest::Method, String> {
    // `reqwest::Method`'s `FromStr` is case-sensitive (HTTP method tokens
    // are, per RFC 7230) — uppercasing first is a lenient nicety for a
    // model that writes "post" rather than "POST", not a spec violation
    // (the actual request always goes out with the canonical uppercase form).
    method
        .to_uppercase()
        .parse()
        .map_err(|_| format!("unsupported HTTP method: {method:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every locally-reachable address in this dev container is either
    /// loopback or RFC1918-private, so there's no way to stand up a
    /// "legitimate" fixture server for these tests without loosening the
    /// guard *somewhere* — same reasoning, same seam,
    /// `webfetch::browser_tests::allow_loopback_too` already established.
    fn allow_loopback_too(addr: IpAddr) -> bool {
        fetch_guard::is_safe_fetch_addr(addr) || addr.is_loopback()
    }

    async fn start_test_server() -> (String, tokio::task::JoinHandle<()>) {
        let router = axum::Router::new()
            .route(
                "/echo",
                axum::routing::get(|| async {
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/plain")],
                        "hello from the test server",
                    )
                }),
            )
            .route(
                "/redirect-once",
                axum::routing::get(|| async {
                    axum::response::Redirect::temporary("/echo")
                }),
            )
            .route(
                "/endless",
                axum::routing::get(|| async {
                    let chunk = axum::body::Bytes::from(vec![b'a'; 64 * 1024]);
                    let stream = futures_util::stream::repeat_with(move || Ok::<_, std::io::Error>(chunk.clone()));
                    axum::body::Body::from_stream(stream)
                }),
            )
            .route(
                "/not-found",
                axum::routing::get(|| async { (axum::http::StatusCode::NOT_FOUND, "nope") }),
            )
            .route(
                "/redirect-to-private",
                axum::routing::get(|| async {
                    axum::response::Redirect::temporary("http://169.254.169.254/secret")
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a test-local port");
        let port = listener.local_addr().expect("local addr").port();
        let task = tokio::spawn(async move {
            axum::serve(listener, router).await.expect("test server error");
        });
        (format!("http://127.0.0.1:{port}"), task)
    }

    /// SME-51 B4: the request goes to the address the guard checked. A
    /// name the guard resolves (here, only through the test DNS) but the
    /// system can't stands in for a DNS-rebinding name, which answers the
    /// guard with a public address and the connection with a private one.
    #[tokio::test]
    async fn test_the_request_connects_to_the_address_that_was_checked() {
        let (base, _server) = start_test_server().await;
        let port = base.rsplit(':').next().unwrap();
        fetch_guard::test_dns::set("rebind.smelt-test.invalid", vec!["127.0.0.1".parse().unwrap()]);
        let result = request_with_guard(
            "GET",
            &format!("http://rebind.smelt-test.invalid:{port}/echo"),
            &[],
            None,
            allow_loopback_too,
        )
        .await
        .expect("the request should go to the checked address");
        assert_eq!(result.body, "hello from the test server");
    }

    /// SME-51 B8: the body was read whole before being cut to size, so an
    /// endless or huge response filled memory until the timeout.
    #[tokio::test]
    async fn test_an_endless_body_is_read_only_as_far_as_it_is_kept() {
        let (base, _server) = start_test_server().await;
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            request_with_guard("GET", &format!("{base}/endless"), &[], None, allow_loopback_too),
        )
        .await
        .expect("the request should stop reading once it has enough")
        .expect("request");
        assert!(result.truncated);
        assert!(result.body.chars().count() <= MAX_BODY_CHARS + 200);
    }

    #[tokio::test]
    async fn test_request_returns_a_successful_response() {
        let (base, _server) = start_test_server().await;
        let result = request_with_guard("GET", &format!("{base}/echo"), &[], None, allow_loopback_too)
            .await
            .expect("request should succeed");
        assert_eq!(result.status, 200);
        assert_eq!(result.body, "hello from the test server");
        assert_eq!(result.content_type.as_deref(), Some("text/plain"));
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn test_request_follows_a_redirect_and_reports_the_final_url() {
        let (base, _server) = start_test_server().await;
        let result = request_with_guard(
            "GET",
            &format!("{base}/redirect-once"),
            &[],
            None,
            allow_loopback_too,
        )
        .await
        .expect("request should succeed");
        assert_eq!(result.status, 200);
        assert_eq!(result.body, "hello from the test server");
        assert_eq!(result.url, format!("{base}/echo"));
    }

    #[tokio::test]
    async fn test_request_returns_a_non_2xx_status_as_data_not_an_error() {
        let (base, _server) = start_test_server().await;
        let result = request_with_guard(
            "GET",
            &format!("{base}/not-found"),
            &[],
            None,
            allow_loopback_too,
        )
        .await
        .expect("a 404 should still be a successful call");
        assert_eq!(result.status, 404);
        assert_eq!(result.body, "nope");
    }

    #[tokio::test]
    async fn test_request_refuses_to_follow_a_redirect_to_a_private_address() {
        // The redirect *target* is checked too, not just the URL the model
        // originally asked for — a plain HTTP client has no page-JS layer
        // to worry about (unlike webfetch), but a redirect chain is
        // exactly the same "attacker-influenced destination" shape.
        let (base, _server) = start_test_server().await;
        let result = request_with_guard(
            "GET",
            &format!("{base}/redirect-to-private"),
            &[],
            None,
            allow_loopback_too,
        )
        .await;
        assert!(
            result.is_err(),
            "expected a redirect to a private address to be refused, got: {result:?}"
        );
    }

    #[tokio::test]
    async fn test_request_refuses_a_direct_loopback_target_without_the_test_seam() {
        let (base, _server) = start_test_server().await;
        let result = request("GET", &format!("{base}/echo"), &[], None).await;
        assert!(
            result.is_err(),
            "the real (non-relaxed) guard should refuse a loopback address"
        );
    }

    /// A model reaching for its sandbox's dev server with http_request
    /// (seen on SME-42's hands-on check) should hear where that works.
    #[tokio::test]
    async fn test_a_refused_localhost_request_says_where_localhost_reaches_the_sandbox() {
        for url in ["http://127.0.0.1:5173/", "http://localhost:3000/api"] {
            let error = request("GET", url, &[], None).await.expect_err("loopback is refused here");
            assert!(error.contains("webfetch") && error.contains("curl"), "{url}: {error}");
        }
        let error = request("GET", "http://10.0.0.5/", &[], None).await.expect_err("private is refused");
        assert!(!error.contains("webfetch"), "only localhost-style hosts get the pointer: {error}");
    }

    #[test]
    fn test_resolve_redirect_target_resolves_a_relative_path_against_the_base() {
        assert_eq!(
            resolve_redirect_target("https://example.com/a/b", "/c").unwrap(),
            "https://example.com/c"
        );
    }

    #[test]
    fn test_resolve_redirect_target_resolves_a_path_relative_to_the_current_directory() {
        assert_eq!(
            resolve_redirect_target("https://example.com/a/b", "c").unwrap(),
            "https://example.com/a/c"
        );
    }

    #[test]
    fn test_resolve_redirect_target_passes_through_an_absolute_url_unchanged() {
        assert_eq!(
            resolve_redirect_target("https://example.com/a/b", "https://other.com/x").unwrap(),
            "https://other.com/x"
        );
    }

    #[test]
    fn test_resolve_redirect_target_rejects_an_invalid_base() {
        assert!(resolve_redirect_target("not a url", "/c").is_err());
    }

    #[test]
    fn test_parse_method_accepts_standard_methods_case_insensitively() {
        assert_eq!(parse_method("GET").unwrap(), reqwest::Method::GET);
        assert_eq!(parse_method("post").unwrap(), reqwest::Method::POST);
        assert_eq!(parse_method("Delete").unwrap(), reqwest::Method::DELETE);
    }

    #[test]
    fn test_parse_method_rejects_something_with_whitespace() {
        assert!(parse_method("GET /x").is_err());
    }
}
