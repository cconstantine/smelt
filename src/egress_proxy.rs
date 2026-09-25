//! A small HTTP proxy that all of the shared headless browser's traffic is
//! forced through, so the SSRF guard applies to *everything* a page does —
//! not just requests on the one page CDP interception is enabled on. Popups,
//! WebSockets, service workers and other workers each run outside that page
//! and slipped past per-page interception entirely; at the network level
//! there's no way around the proxy.
//!
//! Handles the two forms a browser sends to an HTTP proxy: `CONNECT
//! host:port` (HTTPS, WebSockets) becomes a raw tunnel, and absolute-form
//! `GET http://host/path` is forwarded as origin-form with `Connection:
//! close`, so the browser never reuses one upstream connection for a
//! different host. Every target is resolved here, every resolved address is
//! checked, and the connection goes to exactly the address that was checked.

use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::fetch_guard;

/// Largest request head accepted — far above anything a browser sends.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Bounds reading the request head and connecting upstream; the tunnel
/// itself is unbounded, since a WebSocket can legitimately stay open.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(test)]
static REQUESTS_HANDLED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many requests any proxy in this process has accepted — for tests
/// that need to prove traffic actually went through one.
#[cfg(all(test, feature = "browser-test"))]
pub fn requests_handled() -> u64 {
    REQUESTS_HANDLED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Starts the proxy on a loopback port and returns its address. Runs for
/// the rest of the process's life.
pub async fn start(is_addr_allowed: fn(IpAddr) -> bool) -> std::io::Result<SocketAddr> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((client, _)) => {
                    tokio::spawn(async move {
                        if let Err(e) = handle(client, is_addr_allowed).await {
                            tracing::debug!("egress proxy: {e}");
                        }
                    });
                }
                Err(e) => {
                    tracing::warn!("egress proxy: accept failed: {e}");
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        }
    });
    Ok(addr)
}

async fn handle(mut client: TcpStream, is_addr_allowed: fn(IpAddr) -> bool) -> Result<(), String> {
    let (head, rest) = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_head(&mut client))
        .await
        .map_err(|_| "timed out reading the request".to_string())??;
    #[cfg(test)]
    REQUESTS_HANDLED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let request = match parse_request_head(&head) {
        Ok(request) => request,
        Err(e) => {
            respond(&mut client, "400 Bad Request").await;
            return Err(e);
        }
    };
    let addrs = match fetch_guard::resolve_allowed(&request.host, request.port, is_addr_allowed).await
    {
        Ok(addrs) => addrs,
        Err(e) => {
            respond(&mut client, "403 Forbidden").await;
            return Err(e);
        }
    };
    let mut upstream = match tokio::time::timeout(HANDSHAKE_TIMEOUT, TcpStream::connect(&addrs[..])).await
    {
        Ok(Ok(upstream)) => upstream,
        _ => {
            respond(&mut client, "502 Bad Gateway").await;
            return Err(format!("could not connect to {}:{}", request.host, request.port));
        }
    };
    let io = |e: std::io::Error| e.to_string();
    match &request.forward_head {
        None => client
            .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
            .await
            .map_err(io)?,
        Some(forward_head) => upstream.write_all(forward_head).await.map_err(io)?,
    }
    upstream.write_all(&rest).await.map_err(io)?;
    tokio::io::copy_bidirectional(&mut client, &mut upstream)
        .await
        .map_err(io)?;
    Ok(())
}

/// Reads up to and including the blank line ending the request head;
/// returns the head and whatever bytes arrived after it.
async fn read_head(client: &mut TcpStream) -> Result<(Vec<u8>, Vec<u8>), String> {
    let mut buf = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = buf.split_off(end + 4);
            return Ok((buf, rest));
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err("request head too large".to_string());
        }
        let n = client.read(&mut chunk).await.map_err(|e| e.to_string())?;
        if n == 0 {
            return Err("connection closed before the request head ended".to_string());
        }
        buf.extend_from_slice(&chunk[..n]);
    }
}

async fn respond(client: &mut TcpStream, status: &str) {
    let _ = client
        .write_all(format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").as_bytes())
        .await;
}

#[derive(Debug, PartialEq)]
struct ProxyRequest {
    host: String,
    port: u16,
    /// The request head to send upstream, rewritten to origin-form; `None`
    /// for `CONNECT`, which tunnels raw bytes instead.
    forward_head: Option<Vec<u8>>,
}

/// Headers that are about the hop to this proxy, not the request itself.
const HOP_HEADERS: [&str; 3] = ["proxy-connection", "proxy-authorization", "connection"];

fn parse_request_head(head: &[u8]) -> Result<ProxyRequest, String> {
    let head = std::str::from_utf8(head).map_err(|_| "request head is not UTF-8".to_string())?;
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(format!("malformed request line: {request_line:?}"));
    };

    if method.eq_ignore_ascii_case("CONNECT") {
        let (host, port) = host_and_port(&format!("http://{target}"))?;
        return Ok(ProxyRequest {
            host,
            port,
            forward_head: None,
        });
    }

    let url = url::Url::parse(target)
        .map_err(|_| format!("expected an absolute http:// target, got {target:?}"))?;
    if url.scheme() != "http" {
        return Err(format!("unsupported scheme through the proxy: {}", url.scheme()));
    }
    let (host, port) = host_and_port(target)?;
    let path = match url.query() {
        Some(query) => format!("{}?{query}", url.path()),
        None => url.path().to_string(),
    };
    let headers: Vec<&str> = lines.take_while(|line| !line.is_empty()).collect();
    let upgrading = headers
        .iter()
        .any(|h| h.split(':').next().is_some_and(|n| n.trim().eq_ignore_ascii_case("upgrade")));
    let mut forward = format!("{method} {path} {version}\r\n");
    for header in &headers {
        let name = header.split(':').next().unwrap_or_default().trim().to_ascii_lowercase();
        let keep_connection = upgrading && name == "connection";
        if !HOP_HEADERS.contains(&name.as_str()) || keep_connection {
            forward.push_str(header);
            forward.push_str("\r\n");
        }
    }
    if !upgrading {
        forward.push_str("Connection: close\r\n");
    }
    forward.push_str("\r\n");
    Ok(ProxyRequest {
        host,
        port,
        forward_head: Some(forward.into_bytes()),
    })
}

fn host_and_port(url: &str) -> Result<(String, u16), String> {
    let parsed = url::Url::parse(url).map_err(|e| format!("invalid target {url:?}: {e}"))?;
    let host = match parsed.host() {
        Some(url::Host::Domain(domain)) => domain.to_string(),
        Some(url::Host::Ipv4(ip)) => ip.to_string(),
        Some(url::Host::Ipv6(ip)) => ip.to_string(),
        None => return Err(format!("no host in {url:?}")),
    };
    let port = parsed
        .port_or_known_default()
        .ok_or_else(|| format!("no port in {url:?}"))?;
    Ok((host, port))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn forward_head(request: &ProxyRequest) -> &str {
        std::str::from_utf8(request.forward_head.as_deref().expect("a forwarded request")).unwrap()
    }

    #[test]
    fn test_parse_connect_gives_a_tunnel_target() {
        let request = parse_request_head(b"CONNECT example.com:443 HTTP/1.1\r\nHost: example.com:443\r\n\r\n")
            .unwrap();
        assert_eq!(
            request,
            ProxyRequest {
                host: "example.com".to_string(),
                port: 443,
                forward_head: None
            }
        );
    }

    #[test]
    fn test_parse_connect_handles_ipv6_literals() {
        let request = parse_request_head(b"CONNECT [::1]:8080 HTTP/1.1\r\n\r\n").unwrap();
        assert_eq!((request.host.as_str(), request.port), ("::1", 8080));
    }

    #[test]
    fn test_parse_absolute_get_rewrites_to_origin_form_and_closes() {
        let request = parse_request_head(
            b"GET http://example.com:8080/a/b?c=1 HTTP/1.1\r\nHost: example.com:8080\r\n\
              Proxy-Connection: keep-alive\r\nAccept: */*\r\n\r\n",
        )
        .unwrap();
        assert_eq!((request.host.as_str(), request.port), ("example.com", 8080));
        assert_eq!(
            forward_head(&request),
            "GET /a/b?c=1 HTTP/1.1\r\nHost: example.com:8080\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn test_parse_absolute_get_keeps_an_upgrade() {
        let request = parse_request_head(
            b"GET http://example.com/ws HTTP/1.1\r\nHost: example.com\r\nConnection: Upgrade\r\n\
              Upgrade: websocket\r\n\r\n",
        )
        .unwrap();
        assert_eq!(
            forward_head(&request),
            "GET /ws HTTP/1.1\r\nHost: example.com\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n\r\n"
        );
    }

    #[test]
    fn test_parse_rejects_origin_form_and_other_schemes() {
        assert!(parse_request_head(b"GET /path HTTP/1.1\r\n\r\n").is_err());
        assert!(parse_request_head(b"GET ftp://example.com/ HTTP/1.1\r\n\r\n").is_err());
        assert!(parse_request_head(b"garbage\r\n\r\n").is_err());
    }

    fn allow_loopback(addr: IpAddr) -> bool {
        addr.is_loopback()
    }

    async fn start_upstream() -> SocketAddr {
        let router = axum::Router::new().route("/", axum::routing::get(|| async { "upstream says hi" }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        addr
    }

    fn client_via(proxy: SocketAddr) -> reqwest::Client {
        reqwest::Client::builder()
            .proxy(reqwest::Proxy::all(format!("http://{proxy}")).unwrap())
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn test_forwards_a_plain_http_request_to_an_allowed_address() {
        let upstream = start_upstream().await;
        let proxy = start(allow_loopback).await.unwrap();
        let response = client_via(proxy).get(format!("http://{upstream}/")).send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.text().await.unwrap(), "upstream says hi");
    }

    #[tokio::test]
    async fn test_refuses_a_plain_http_request_to_a_refused_address() {
        let upstream = start_upstream().await;
        let proxy = start(fetch_guard::is_safe_fetch_addr).await.unwrap();
        let response = client_via(proxy).get(format!("http://{upstream}/")).send().await.unwrap();
        assert_eq!(response.status(), 403);
    }

    #[tokio::test]
    async fn test_refuses_a_hostname_that_resolves_to_a_refused_address() {
        let upstream = start_upstream().await;
        let proxy = start(fetch_guard::is_safe_fetch_addr).await.unwrap();
        let response = client_via(proxy)
            .get(format!("http://localhost:{}/", upstream.port()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
    }

    async fn connect_through(proxy: SocketAddr, target: SocketAddr) -> (TcpStream, String) {
        let mut stream = TcpStream::connect(proxy).await.unwrap();
        stream
            .write_all(format!("CONNECT {target} HTTP/1.1\r\nHost: {target}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut reply = vec![0u8; 256];
        let n = stream.read(&mut reply).await.unwrap();
        (stream, String::from_utf8_lossy(&reply[..n]).to_string())
    }

    #[tokio::test]
    async fn test_tunnels_connect_to_an_allowed_address() {
        let upstream = start_upstream().await;
        let proxy = start(allow_loopback).await.unwrap();
        let (mut tunnel, reply) = connect_through(proxy, upstream).await;
        assert!(reply.starts_with("HTTP/1.1 200"), "got {reply:?}");
        tunnel
            .write_all(format!("GET / HTTP/1.1\r\nHost: {upstream}\r\nConnection: close\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut response = String::new();
        tunnel.read_to_string(&mut response).await.unwrap();
        assert!(response.ends_with("upstream says hi"), "got {response:?}");
    }

    #[tokio::test]
    async fn test_refuses_connect_to_a_refused_address() {
        let upstream = start_upstream().await;
        let proxy = start(fetch_guard::is_safe_fetch_addr).await.unwrap();
        let (_, reply) = connect_through(proxy, upstream).await;
        assert!(reply.starts_with("HTTP/1.1 403"), "got {reply:?}");
    }
}
