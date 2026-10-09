//! Refuses requests another website makes on the user's behalf (SME-51 B1).
//!
//! smelt has no login, and `dioxus-fullstack` decodes a server function's
//! body as JSON whatever its `Content-Type`. So a `text/plain` POST — one a
//! browser sends from any page without a CORS preflight — could start a turn,
//! import an SSH key or stop a pod. Two checks close that:
//!
//! - **Cross-site writes.** A request that isn't `GET`/`HEAD`/`OPTIONS` is
//!   refused when the browser says it came from another site
//!   (`Sec-Fetch-Site`), or, from a browser too old to send that, when its
//!   `Origin` names another host. `same-site` is refused too: a sandbox
//!   preview is served from a sibling origin (`*.preview.localhost`,
//!   `*-smelt.example.com`) and runs whatever the model wrote.
//! - **Unknown `Host` names** (DNS rebinding: a page whose name later resolves
//!   to smelt's address reads it as same-origin). Enforced when the names
//!   smelt is reached by are known, from `SMELT_BASE_URL` and
//!   `SMELT_ALLOWED_HOSTS` (plus `SMELT_BASE_URL` once that's set); an IP
//!   address or `localhost` is always allowed,
//!   since rebinding needs a name.
//!
//! A request with neither header (curl, a script) is let through: no browser
//! sent it, so no other site's page did.

use axum::extract::Request;
use axum::http::{HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// The host names smelt is reached by, beyond IP addresses and `localhost`:
/// `SMELT_BASE_URL`'s host plus the comma-separated `SMELT_ALLOWED_HOSTS`.
/// Empty means unknown, and the `Host` check is skipped.
pub fn allowed_hosts_from_env() -> Vec<String> {
    allowed_hosts(
        std::env::var("SMELT_BASE_URL").ok().as_deref(),
        std::env::var("SMELT_ALLOWED_HOSTS").ok().as_deref(),
    )
}

/// `allowed_hosts_from_env` on given values. Only `SMELT_ALLOWED_HOSTS`
/// turns the check on: `SMELT_BASE_URL` is often set because smelt is
/// behind a proxy, and a proxy usually sends its own name for smelt as
/// `Host`, which would then be refused (SME-51 code review 1). Once on,
/// `SMELT_BASE_URL`'s host is allowed too.
fn allowed_hosts(base_url: Option<&str>, list: Option<&str>) -> Vec<String> {
    let mut hosts: Vec<String> = list
        .unwrap_or_default()
        .split(',')
        .map(|h| h.trim().to_ascii_lowercase())
        .filter(|h| !h.is_empty())
        .collect();
    if hosts.is_empty() {
        return hosts;
    }
    if let Some(base) = base_url.filter(|v| !v.trim().is_empty())
        && let Ok(url) = url::Url::parse(base.trim())
        && let Some(host) = url.host_str()
    {
        hosts.push(host.to_ascii_lowercase());
    }
    hosts
}

/// Why `method`/`headers` should be refused, or `None` to let it through.
pub fn refusal(method: &Method, headers: &HeaderMap, allowed_hosts: &[String]) -> Option<String> {
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::trim);

    if !allowed_hosts.is_empty()
        && let Some(host) = header("host")
        && !host_allowed(host, allowed_hosts)
    {
        return Some(format!(
            "smelt isn't served at {host}. Add it to SMELT_ALLOWED_HOSTS if it should be."
        ));
    }

    if matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS) {
        return None;
    }
    match header("sec-fetch-site") {
        Some("same-origin") | Some("none") => None,
        Some(other) => Some(format!("refused a {other} request: smelt only takes changes from its own pages")),
        None => match header("origin") {
            None => None,
            Some(origin) => {
                let origin_host = url::Url::parse(origin).ok().and_then(|u| {
                    u.host_str().map(|h| match u.port() {
                        Some(port) => format!("{h}:{port}"),
                        None => h.to_string(),
                    })
                });
                let request_host = header("x-forwarded-host").or(header("host"));
                match (origin_host, request_host) {
                    (Some(o), Some(h)) if o.eq_ignore_ascii_case(h) => None,
                    _ => Some(format!(
                        "refused a request from {origin}: smelt only takes changes from its own pages"
                    )),
                }
            }
        },
    }
}

/// `host` (as a `Host` header: a name or address, maybe with a port) is an
/// IP address, `localhost` or a `.localhost` name, or one of `allowed`.
fn host_allowed(host: &str, allowed: &[String]) -> bool {
    let name = match host.strip_prefix('[') {
        // [::1]:8080
        Some(rest) => return rest.split(']').next().is_some_and(|ip| ip.parse::<std::net::IpAddr>().is_ok()),
        None => host.rsplit_once(':').map_or(host, |(name, port)| {
            if port.chars().all(|c| c.is_ascii_digit()) { name } else { host }
        }),
    }
    .to_ascii_lowercase();
    name.parse::<std::net::IpAddr>().is_ok()
        || name == "localhost"
        || name.ends_with(".localhost")
        || allowed.iter().any(|a| *a == name)
}

/// The middleware: `build_router` layers it over every route.
pub async fn guard(request: Request, next: Next) -> Response {
    static ALLOWED: std::sync::LazyLock<Vec<String>> = std::sync::LazyLock::new(allowed_hosts_from_env);
    match refusal(request.method(), request.headers(), &ALLOWED) {
        Some(reason) => {
            // The path only: a query can carry an OAuth code, and this line is
            // exported with the request's span (SME-137).
            tracing::warn!(method = %request.method(), path = %request.uri().path(), %reason, "refused a request");
            (StatusCode::FORBIDDEN, reason).into_response()
        }
        None => next.run(request).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                axum::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                value.parse().unwrap(),
            );
        }
        map
    }

    #[test]
    fn test_a_cross_site_write_is_refused_and_a_same_origin_one_is_not() {
        let post = Method::POST;
        assert!(refusal(&post, &headers(&[("sec-fetch-site", "cross-site")]), &[]).is_some());
        // A preview is a sibling origin running what the model wrote.
        assert!(refusal(&post, &headers(&[("sec-fetch-site", "same-site")]), &[]).is_some());
        assert_eq!(refusal(&post, &headers(&[("sec-fetch-site", "same-origin")]), &[]), None);
        assert_eq!(refusal(&post, &headers(&[("sec-fetch-site", "none")]), &[]), None);
        // Reads are left alone: another site can't see the answer anyway.
        assert_eq!(refusal(&Method::GET, &headers(&[("sec-fetch-site", "cross-site")]), &[]), None);
    }

    #[test]
    fn test_without_sec_fetch_site_the_origin_decides() {
        let post = Method::POST;
        let evil = headers(&[("origin", "https://evil.example"), ("host", "smelt.example.com")]);
        assert!(refusal(&post, &evil, &[]).is_some());
        let own = headers(&[("origin", "http://localhost:8080"), ("host", "localhost:8080")]);
        assert_eq!(refusal(&post, &own, &[]), None);
        let proxied = headers(&[
            ("origin", "https://smelt.example.com"),
            ("host", "127.0.0.1:8080"),
            ("x-forwarded-host", "smelt.example.com"),
        ]);
        assert_eq!(refusal(&post, &proxied, &[]), None);
        assert!(refusal(&post, &headers(&[("origin", "null")]), &[]).is_some());
        // No browser headers at all: curl or a script, not another site's page.
        assert_eq!(refusal(&post, &headers(&[("host", "localhost:8080")]), &[]), None);
    }

    /// SME-51 code review 1: `SMELT_BASE_URL` alone mustn't turn the host
    /// check on. It's set behind a proxy, whose own name for smelt (the
    /// `Host` it sends) would then be refused for every request.
    #[test]
    fn test_only_smelt_allowed_hosts_turns_the_host_check_on() {
        assert!(allowed_hosts(Some("https://smelt.example.com"), None).is_empty());
        assert!(allowed_hosts(Some("https://smelt.example.com"), Some(" ")).is_empty());
        assert_eq!(
            allowed_hosts(Some("https://smelt.example.com/"), Some("Smelt.Lan, other.example")),
            vec!["smelt.lan", "other.example", "smelt.example.com"]
        );
    }

    #[test]
    fn test_an_unknown_host_name_is_refused_once_the_names_are_known() {
        let get = Method::GET;
        let known = vec!["smelt.example.com".to_string()];
        assert!(refusal(&get, &headers(&[("host", "rebind.attacker.example")]), &known).is_some());
        for ok in ["smelt.example.com", "SMELT.example.com:443", "localhost:8080", "5173-4.preview.localhost:8181", "192.168.1.5:8080", "[::1]:8080"] {
            assert_eq!(refusal(&get, &headers(&[("host", ok)]), &known), None, "{ok}");
        }
        // Unknown names are only refused when smelt knows its own.
        assert_eq!(refusal(&get, &headers(&[("host", "rebind.attacker.example")]), &[]), None);
    }

    #[tokio::test]
    async fn test_the_middleware_refuses_before_the_handler_runs() {
        let router = axum::Router::new()
            .route("/api/thing", axum::routing::post(|| async { "done" }))
            .layer(axum::middleware::from_fn(guard));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = reqwest::Client::new();
        let url = format!("http://{addr}/api/thing");

        let refused = client
            .post(&url)
            .header("content-type", "text/plain")
            .header("origin", "https://evil.example")
            .header("sec-fetch-site", "cross-site")
            .body("{}")
            .send()
            .await
            .unwrap();
        assert_eq!(refused.status(), 403);

        let allowed = client.post(&url).header("sec-fetch-site", "same-origin").send().await.unwrap();
        assert_eq!(allowed.status(), 200);
        assert_eq!(allowed.text().await.unwrap(), "done");
    }
}
