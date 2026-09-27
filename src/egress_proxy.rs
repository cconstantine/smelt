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

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::fetch_guard;
use crate::sandbox::PodIo;
use sqlx::PgPool;

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
    tokio::spawn(accept_loop(listener, is_addr_allowed, None));
    Ok(addr)
}

/// Opens a connection to a port in one conversation's sandbox pod —
/// `sandbox::open_pod_port` for that conversation, or a stand-in in tests.
/// An `Err` is a message the proxy shows as the reason it couldn't connect.
pub type SandboxDial = Arc<dyn Fn(u16) -> DialFuture + Send + Sync>;

/// What a `SandboxDial` returns.
pub type DialFuture = Pin<Box<dyn Future<Output = Result<Box<dyn PodIo>, String>> + Send>>;

/// The real `SandboxDial` for `conversation_id`: `sandbox::open_pod_port`,
/// with its failures turned into sentences the model (or the user, in the
/// live panel) can act on.
pub fn sandbox_dial(pool: PgPool, conversation_id: i64) -> SandboxDial {
    let dial: SandboxDial = Arc::new(move |port| {
        let pool = pool.clone();
        Box::pin(async move {
            crate::sandbox::open_pod_port(&pool, conversation_id, port)
                .await
                .map_err(|e| match e {
                    crate::sandbox::TerminalError::NoPod => format!(
                        "This conversation has no running sandbox, so there's nothing at \
                         localhost:{port}. Start one with create_pod and run the server there."
                    ),
                    other => format!("Couldn't reach port {port} in the sandbox: {other}"),
                })
        })
    });
    with_timeout(dial, HANDSHAKE_TIMEOUT)
}

/// `dial`, bounded: a port-forward that hasn't opened within `limit` fails
/// with a message saying so. The one place opening a port-forward is
/// bounded, so every caller of `sandbox_dial` inherits it.
pub fn with_timeout(dial: SandboxDial, limit: Duration) -> SandboxDial {
    Arc::new(move |port| {
        let opening = dial(port);
        Box::pin(async move {
            tokio::time::timeout(limit, opening)
                .await
                .unwrap_or_else(|_| Err(format!("Timed out connecting to port {port} in the sandbox.")))
        })
    })
}

/// A proxy from `start_with_sandbox`. It stops accepting connections when
/// dropped; connections already open finish on their own.
pub struct RoutedProxy {
    pub addr: SocketAddr,
    accept_task: tokio::task::JoinHandle<()>,
}

impl Drop for RoutedProxy {
    fn drop(&mut self) {
        self.accept_task.abort();
    }
}

/// Like `start`, but for one conversation's browser context: requests for
/// `localhost`, `127.0.0.1` or `::1` (`fetch_guard::is_sandbox_host`) go to
/// that port in the conversation's sandbox through `dial`, instead of being
/// refused as loopback. Every other host goes through `is_addr_allowed`
/// exactly as with `start`. See SME-42.
pub async fn start_with_sandbox(
    is_addr_allowed: fn(IpAddr) -> bool,
    dial: SandboxDial,
) -> std::io::Result<RoutedProxy> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let accept_task = tokio::spawn(accept_loop(listener, is_addr_allowed, Some(dial)));
    Ok(RoutedProxy { addr, accept_task })
}

async fn accept_loop(
    listener: TcpListener,
    is_addr_allowed: fn(IpAddr) -> bool,
    sandbox: Option<SandboxDial>,
) {
    loop {
        match listener.accept().await {
            Ok((client, _)) => {
                let sandbox = sandbox.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(client, is_addr_allowed, sandbox).await {
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
}

async fn handle(
    mut client: TcpStream,
    is_addr_allowed: fn(IpAddr) -> bool,
    sandbox: Option<SandboxDial>,
) -> Result<(), String> {
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
    if let Some(dial) = sandbox
        && fetch_guard::is_sandbox_host(&request.host)
    {
        return handle_sandbox(client, request, rest, dial).await;
    }
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

/// The sandbox route: `request` goes to its port in the conversation's pod,
/// through `dial`. There's no address to check — `dial` can only reach that
/// one pod. A plain request's first reply bytes are read before anything
/// goes back, so a port nothing listens on (the stream ends without a
/// byte) becomes a readable 502 rather than an empty reply. The tunnel then
/// runs until either side closes; nothing here waits on the pod side's end
/// of stream, which a port-forward reports about a second late.
async fn handle_sandbox(
    mut client: TcpStream,
    request: ProxyRequest,
    rest: Vec<u8>,
    dial: SandboxDial,
) -> Result<(), String> {
    let io = |e: std::io::Error| e.to_string();
    // What goes to the pod first, and who's asking. A CONNECT tunnel's real
    // request (a WebSocket's handshake) is inside it: answer the CONNECT,
    // then read that request's head for its Origin.
    let (source, first_bytes) = match &request.forward_head {
        Some(head) => {
            let mut first = head.clone();
            first.extend_from_slice(&rest);
            (request.source.clone(), first)
        }
        None => {
            client
                .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
                .await
                .map_err(io)?;
            let mut peeked = rest;
            if peeked.is_empty() {
                let mut chunk = [0u8; 4096];
                let n = tokio::time::timeout(HANDSHAKE_TIMEOUT, client.read(&mut chunk))
                    .await
                    .map_err(|_| "timed out waiting for the tunnel's request".to_string())?
                    .map_err(io)?;
                peeked.extend_from_slice(&chunk[..n]);
            }
            // TLS hides who's asking, so it isn't let through. It wouldn't
            // load anyway: a dev server's own certificate isn't trusted.
            if peeked.first() == Some(&0x16) {
                return Err("refused a TLS tunnel to the sandbox: its requests can't be checked".to_string());
            }
            let (head, after) = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_head_from(&mut client, peeked))
                .await
                .map_err(|_| "timed out reading the tunnel's request".to_string())??;
            let source = request_source(&String::from_utf8_lossy(&head));
            let mut first = head;
            first.extend_from_slice(&after);
            (source, first)
        }
    };
    if !fetch_guard::allows_request_from(&source, |host, _| fetch_guard::is_sandbox_host(host)) {
        let from = source.origin.as_deref().map(|o| format!(" ({o})")).unwrap_or_default();
        let message = format!(
            "Refused: a page from another site{from} tried to reach port {} in this conversation's \
             sandbox. Only the sandbox's own pages, and pages opened directly, can reach it.",
            request.port
        );
        respond_with_body(&mut client, "403 Forbidden", &message).await;
        return Err(message);
    }
    // `sandbox_dial` bounds opening the port-forward (`with_timeout`).
    let upstream = match dial(request.port).await {
        Ok(upstream) => upstream,
        Err(message) => {
            respond_with_body(&mut client, "502 Bad Gateway", &message).await;
            return Err(message);
        }
    };
    let (mut from_page, mut to_page) = client.into_split();
    let (mut from_pod, mut to_pod) = tokio::io::split(upstream);
    to_pod.write_all(&first_bytes).await.map_err(io)?;
    // Everything else the page sends goes on to the pod from here — a
    // request body can follow its head in writes of its own (Chrome does
    // this for a larger POST), and the pod won't answer until it has it.
    let page_to_pod = tokio::spawn(async move {
        let _ = tokio::io::copy(&mut from_page, &mut to_pod).await;
        let _ = to_pod.shutdown().await;
    });
    if request.forward_head.is_some() {
        let mut first = vec![0u8; 16 * 1024];
        let n = from_pod.read(&mut first).await.map_err(io)?;
        if n == 0 {
            page_to_pod.abort();
            let message = format!(
                "Nothing is listening on port {} in this conversation's sandbox.",
                request.port
            );
            respond_with_body(&mut to_page, "502 Bad Gateway", &message).await;
            return Err(message);
        }
        to_page.write_all(&first[..n]).await.map_err(io)?;
    }
    let result = tokio::io::copy(&mut from_pod, &mut to_page).await;
    let _ = to_page.shutdown().await;
    // The pod's side is done; nothing it could still receive would be read.
    page_to_pod.abort();
    result.map(|_| ()).map_err(io)
}

/// Reads up to and including the blank line ending the request head;
/// returns the head and whatever bytes arrived after it.
async fn read_head(client: &mut TcpStream) -> Result<(Vec<u8>, Vec<u8>), String> {
    read_head_from(client, Vec::with_capacity(4096)).await
}

/// `read_head`, starting from bytes already read.
async fn read_head_from(client: &mut TcpStream, mut buf: Vec<u8>) -> Result<(Vec<u8>, Vec<u8>), String> {
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

/// Like `respond`, with a plain-text body saying what went wrong — shown
/// as the page to whoever loaded it, the model or the user.
async fn respond_with_body(client: &mut (impl tokio::io::AsyncWrite + Unpin), status: &str, body: &str) {
    let _ = client
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Type: text/plain; charset=utf-8\r\n\
                 Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .as_bytes(),
        )
        .await;
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
    /// Who sent it, for the sandbox route (`fetch_guard::allows_request_from`).
    /// A `CONNECT`'s real request is inside its tunnel, so this is read there.
    source: fetch_guard::RequestSource,
}

/// The method and the browser-set headers of a request head that say which
/// page sent it.
fn request_source(head: &str) -> fetch_guard::RequestSource {
    let mut lines = head.split("\r\n");
    let method = lines.next().unwrap_or_default().split(' ').next().unwrap_or_default();
    let mut source = fetch_guard::RequestSource {
        method: method.to_string(),
        ..Default::default()
    };
    for line in lines.take_while(|line| !line.is_empty()) {
        let Some((name, value)) = line.split_once(':') else { continue };
        let value = Some(value.trim().to_string());
        match name.trim().to_ascii_lowercase().as_str() {
            "origin" => source.origin = value,
            "sec-fetch-site" => source.sec_fetch_site = value,
            "sec-fetch-mode" => source.sec_fetch_mode = value,
            _ => {}
        }
    }
    source
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
            source: request_source(head),
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
        source: request_source(head),
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

/// A stand-in for `sandbox::open_pod_port` in tests: records each port
/// it's asked for and connects to `upstream` whatever the port.
#[cfg(test)]
pub(crate) fn dial_to(
    upstream: SocketAddr,
    dialed: Arc<std::sync::Mutex<Vec<u16>>>,
) -> SandboxDial {
    Arc::new(move |port| {
        dialed.lock().unwrap().push(port);
        Box::pin(async move {
            let stream = TcpStream::connect(upstream).await.map_err(|e| e.to_string())?;
            Ok(Box::new(stream) as Box<dyn PodIo>)
        })
    })
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
                forward_head: None,
                source: fetch_guard::RequestSource {
                    method: "CONNECT".to_string(),
                    ..Default::default()
                },
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

    #[tokio::test]
    async fn test_a_sandbox_route_sends_localhost_to_the_sandbox_despite_the_strict_guard() {
        let upstream = start_upstream().await;
        let dialed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let proxy = start_with_sandbox(fetch_guard::is_safe_fetch_addr, dial_to(upstream, dialed.clone()))
            .await
            .unwrap();
        for host in ["localhost", "127.0.0.1", "[::1]"] {
            let response = client_via(proxy.addr).get(format!("http://{host}:3000/")).send().await.unwrap();
            assert_eq!(response.status(), 200, "{host}");
            assert_eq!(response.text().await.unwrap(), "upstream says hi", "{host}");
        }
        assert_eq!(*dialed.lock().unwrap(), vec![3000, 3000, 3000]);
    }

    /// Sends `head` through the proxy as a plain HTTP request and returns the
    /// whole reply.
    async fn send_raw(proxy: SocketAddr, head: &str) -> String {
        let mut stream = TcpStream::connect(proxy).await.unwrap();
        stream.write_all(head.as_bytes()).await.unwrap();
        let mut reply = String::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut reply))
            .await
            .expect("a reply in time")
            .unwrap();
        reply
    }

    #[tokio::test]
    async fn test_a_sandbox_route_refuses_requests_other_sites_send() {
        let upstream = start_upstream().await;
        let dialed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let proxy = start_with_sandbox(fetch_guard::is_safe_fetch_addr, dial_to(upstream, dialed.clone()))
            .await
            .unwrap();
        for head in [
            // Another site's <img>: no Origin, cross-site.
            "GET http://localhost:3000/ HTTP/1.1\r\nHost: localhost:3000\r\n\
             Sec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: no-cors\r\n\r\n",
            // Another site's fetch() POST.
            "POST http://localhost:3000/ HTTP/1.1\r\nHost: localhost:3000\r\nOrigin: https://evil.example\r\n\
             Sec-Fetch-Site: cross-site\r\nSec-Fetch-Mode: no-cors\r\nContent-Length: 0\r\n\r\n",
        ] {
            let reply = send_raw(proxy.addr, head).await;
            assert!(reply.starts_with("HTTP/1.1 403"), "got {reply:?}");
            assert!(reply.contains("another site"), "the refusal should say why: {reply:?}");
        }
        assert!(dialed.lock().unwrap().is_empty(), "a refused request must not reach the sandbox");

        // The sandbox's own page, and the model's own navigation, still get through.
        for head in [
            "GET http://localhost:3000/ HTTP/1.1\r\nHost: localhost:3000\r\nOrigin: http://localhost:5173\r\n\
             Sec-Fetch-Site: same-site\r\nSec-Fetch-Mode: cors\r\n\r\n",
            "GET http://localhost:3000/ HTTP/1.1\r\nHost: localhost:3000\r\nSec-Fetch-Site: none\r\n\
             Sec-Fetch-Mode: navigate\r\n\r\n",
        ] {
            let reply = send_raw(proxy.addr, head).await;
            assert!(reply.starts_with("HTTP/1.1 200") && reply.ends_with("upstream says hi"), "got {reply:?}");
        }
    }

    /// A WebSocket's handshake carries no Sec-Fetch-* headers, only
    /// Origin, and it travels inside a CONNECT tunnel.
    #[tokio::test]
    async fn test_a_sandbox_route_checks_who_opened_a_tunnel() {
        let upstream = start_upstream().await;
        let dialed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let proxy = start_with_sandbox(fetch_guard::is_safe_fetch_addr, dial_to(upstream, dialed.clone()))
            .await
            .unwrap();
        let target: SocketAddr = "127.0.0.1:5173".parse().unwrap();

        let (mut tunnel, reply) = connect_through(proxy.addr, target).await;
        assert!(reply.starts_with("HTTP/1.1 200"), "got {reply:?}");
        tunnel
            .write_all(b"GET /ws HTTP/1.1\r\nHost: localhost:5173\r\nOrigin: https://evil.example\r\n\
                         Upgrade: websocket\r\nConnection: Upgrade\r\n\r\n")
            .await
            .unwrap();
        let mut refused = String::new();
        tokio::time::timeout(Duration::from_secs(5), tunnel.read_to_string(&mut refused))
            .await
            .expect("a refusal in time")
            .unwrap();
        assert!(refused.starts_with("HTTP/1.1 403"), "got {refused:?}");
        assert!(dialed.lock().unwrap().is_empty(), "another site's tunnel must not reach the sandbox");

        // TLS can't be checked, so it isn't let through either.
        let (mut tls, _) = connect_through(proxy.addr, target).await;
        tls.write_all(&[0x16, 0x03, 0x01, 0x00, 0x05, 1, 2, 3, 4, 5]).await.unwrap();
        let mut rest = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), tls.read_to_end(&mut rest))
            .await
            .expect("the tunnel should be closed");
        assert!(dialed.lock().unwrap().is_empty(), "a TLS tunnel must not reach the sandbox");

        // The sandbox's own page opening one still gets through.
        let (mut own, _) = connect_through(proxy.addr, target).await;
        own.write_all(b"GET / HTTP/1.1\r\nHost: localhost:5173\r\nOrigin: http://localhost:5173\r\n\
                        Connection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        own.read_to_string(&mut response).await.unwrap();
        assert!(response.ends_with("upstream says hi"), "got {response:?}");
        assert_eq!(*dialed.lock().unwrap(), vec![5173]);
    }

    /// Chrome sends a larger POST's body after its head, in a write of its
    /// own. The route must pass it on while waiting for the reply.
    #[tokio::test]
    async fn test_a_sandbox_route_passes_on_a_body_sent_after_the_head() {
        let router = axum::Router::new().route("/echo", axum::routing::post(|body: String| async move { body }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let dialed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let proxy = start_with_sandbox(fetch_guard::is_safe_fetch_addr, dial_to(upstream, dialed))
            .await
            .unwrap();
        let mut stream = TcpStream::connect(proxy.addr).await.unwrap();
        stream
            .write_all(
                b"POST http://localhost:3000/echo HTTP/1.1\r\nHost: localhost:3000\r\n\
                  Content-Type: text/plain\r\nContent-Length: 5\r\n\r\n",
            )
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        stream.write_all(b"hello").await.unwrap();
        let mut reply = String::new();
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_string(&mut reply))
            .await
            .expect("the POST must be answered, not left waiting for its body")
            .unwrap();
        assert!(reply.starts_with("HTTP/1.1 200") && reply.ends_with("hello"), "got {reply:?}");
    }

    #[tokio::test]
    async fn test_a_sandbox_route_tunnels_connect_to_localhost_through_the_sandbox() {
        let upstream = start_upstream().await;
        let dialed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let proxy = start_with_sandbox(fetch_guard::is_safe_fetch_addr, dial_to(upstream, dialed.clone()))
            .await
            .unwrap();
        let target: SocketAddr = "127.0.0.1:5173".parse().unwrap();
        let (mut tunnel, reply) = connect_through(proxy.addr, target).await;
        assert!(reply.starts_with("HTTP/1.1 200"), "got {reply:?}");
        tunnel
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost:5173\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        tunnel.read_to_string(&mut response).await.unwrap();
        assert!(response.ends_with("upstream says hi"), "got {response:?}");
        assert_eq!(*dialed.lock().unwrap(), vec![5173]);
    }

    #[tokio::test]
    async fn test_a_sandbox_route_still_guards_every_other_host() {
        let upstream = start_upstream().await;
        let dialed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let proxy = start_with_sandbox(fetch_guard::is_safe_fetch_addr, dial_to(upstream, dialed.clone()))
            .await
            .unwrap();
        // Loopback, but not one of the names that mean the sandbox.
        let response = client_via(proxy.addr)
            .get(format!("http://127.0.0.2:{}/", upstream.port()))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
        assert!(dialed.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_a_sandbox_route_that_cannot_connect_says_why() {
        let dial: SandboxDial = Arc::new(|_| {
            Box::pin(async { Err("This conversation has no running sandbox.".to_string()) })
        });
        let proxy = start_with_sandbox(fetch_guard::is_safe_fetch_addr, dial).await.unwrap();
        let response = client_via(proxy.addr).get("http://localhost:3000/").send().await.unwrap();
        assert_eq!(response.status(), 502);
        assert_eq!(response.text().await.unwrap(), "This conversation has no running sandbox.");
    }

    #[tokio::test]
    async fn test_a_sandbox_port_with_nothing_listening_says_so() {
        // Connects, then closes without a byte — what a port-forward to an
        // unused port does.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let silent = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                drop(socket);
            }
        });
        let dialed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let proxy = start_with_sandbox(fetch_guard::is_safe_fetch_addr, dial_to(silent, dialed))
            .await
            .unwrap();
        let response = client_via(proxy.addr).get("http://localhost:3000/").send().await.unwrap();
        assert_eq!(response.status(), 502);
        let body = response.text().await.unwrap();
        assert!(body.contains("Nothing is listening on port 3000"), "got {body:?}");
    }

    #[sqlx::test]
    async fn test_sandbox_dial_without_a_pod_says_how_to_get_one(pool: PgPool) {
        let conversation = crate::db::create_conversation(&pool).await.expect("create conversation");
        let dial = sandbox_dial(pool, conversation.id);
        let error = match dial(3000).await {
            Ok(_) => panic!("a conversation without a pod has nothing to connect to"),
            Err(e) => e,
        };
        assert!(error.contains("no running sandbox"), "got {error:?}");
        assert!(error.contains("create_pod"), "should say how to get one: {error:?}");
    }

    #[tokio::test]
    async fn test_a_dial_that_never_opens_times_out_with_a_reason() {
        let never: SandboxDial = Arc::new(|_| Box::pin(std::future::pending()) as DialFuture);
        let dial = with_timeout(never, Duration::from_millis(100));
        let result = tokio::time::timeout(Duration::from_secs(5), dial(3000))
            .await
            .expect("the bounded dial must give up on its own");
        let error = match result {
            Ok(_) => panic!("a dial that never opens can't succeed"),
            Err(e) => e,
        };
        assert!(error.contains("port 3000") && error.contains("Timed out"), "got {error:?}");
    }

    #[sqlx::test]
    async fn test_sandbox_dial_refuses_the_sandbox_agents_port(pool: PgPool) {
        let conversation = crate::db::create_conversation(&pool).await.expect("create conversation");
        let dial = sandbox_dial(pool, conversation.id);
        let error = match dial(8088).await {
            Ok(_) => panic!("the sandbox agent's port must never be reachable"),
            Err(e) => e,
        };
        assert!(error.contains("sandbox agent"), "got {error:?}");
    }

    #[tokio::test]
    async fn test_dropping_a_routed_proxy_stops_it_accepting() {
        let upstream = start_upstream().await;
        let dialed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let proxy = start_with_sandbox(fetch_guard::is_safe_fetch_addr, dial_to(upstream, dialed))
            .await
            .unwrap();
        let addr = proxy.addr;
        drop(proxy);
        tokio::task::yield_now().await;
        let result = tokio::time::timeout(Duration::from_secs(2), async {
            let mut stream = TcpStream::connect(addr).await?;
            stream.write_all(b"GET http://localhost:3000/ HTTP/1.1\r\n\r\n").await?;
            let mut buf = [0u8; 16];
            stream.read(&mut buf).await
        })
        .await;
        assert!(
            !matches!(result, Ok(Ok(n)) if n > 0),
            "a dropped proxy must not answer, got {result:?}"
        );
    }
}
