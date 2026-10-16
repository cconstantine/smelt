//! A small HTTP proxy that a conversation's browser contexts route through,
//! the only way a browser running in smelt's server process can name a
//! pod's `localhost` or a container's address there (SME-42, SME-33):
//! requests for those hosts go to that conversation's sandbox through a
//! port-forward instead of to the open internet.
//!
//! Handles the two forms a browser sends to an HTTP proxy: `CONNECT
//! host:port` (HTTPS, WebSockets) becomes a raw tunnel, and absolute-form
//! `GET http://host/path` is forwarded as origin-form with `Connection:
//! close`. Chrome pools a proxy connection per destination, so it reuses
//! one only for the same host and port; on the sandbox route, where each
//! request is checked when its connection opens, the pod's reply is marked
//! `Connection: close` too, so each connection carries one request
//! (SME-165, `reply`). Every other host is resolved and connected to directly —
//! addresses are no longer filtered (SME-155), but resolving here rather
//! than letting the connection resolve itself keeps the checked address and
//! the connected one identical.

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

use crate::fetch_guard;
use reply::Relayed;
use crate::sandbox::{PodHost, PodIo};
use sqlx::PgPool;

/// Largest request head accepted — far above anything a browser sends.
const MAX_HEAD_BYTES: usize = 64 * 1024;
/// Largest reply head the sandbox route passes on: Chrome's own limit
/// (`HttpStreamParser::kMaxHeaderBufSize`), so every head it would accept
/// gets through (SME-165 review 1).
const MAX_REPLY_HEAD_BYTES: usize = 256 * 1024;
/// Bounds reading the request head and connecting upstream; the tunnel
/// itself is unbounded, since a WebSocket can legitimately stay open.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Opens a connection to a port in one conversation's sandbox pod, on its
/// own `localhost` or at a Docker container's address there (SME-33) —
/// `sandbox::open_pod_target` for that conversation, or a stand-in in
/// tests. An `Err` is a message the proxy shows as the reason it couldn't
/// connect.
pub type SandboxDial = Arc<dyn Fn(PodHost, u16) -> DialFuture + Send + Sync>;

/// What a `SandboxDial` returns.
pub type DialFuture = Pin<Box<dyn Future<Output = Result<Box<dyn PodIo>, DialError>> + Send>>;

/// Why a `SandboxDial` couldn't connect: a message for whoever asked (the
/// model, through the egress proxy), and whether it's because the
/// conversation has no running sandbox, which a preview link explains to
/// the user its own way (SME-46).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DialError {
    pub message: String,
    pub no_sandbox: bool,
}

impl From<String> for DialError {
    fn from(message: String) -> Self {
        Self { message, no_sandbox: false }
    }
}

impl std::fmt::Display for DialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

/// The real `SandboxDial` for `conversation_id`: `sandbox::open_pod_target`,
/// with its failures turned into sentences the model (or the user, in the
/// live panel) can act on.
pub fn sandbox_dial(pool: PgPool, conversation_id: i64) -> SandboxDial {
    let dial: SandboxDial = Arc::new(move |host, port| {
        let pool = pool.clone();
        Box::pin(async move {
            crate::sandbox::open_pod_target(&pool, conversation_id, host, port)
                .await
                .map_err(|e| match e {
                    crate::sandbox::TerminalError::NoPod => DialError {
                        message: format!(
                            "This conversation has no running sandbox, so there's nothing at \
                             {}. Start one with create_pod and run the server there.",
                            describe(host, port)
                        ),
                        no_sandbox: true,
                    },
                    other => DialError::from(format!("Couldn't reach {} in the sandbox: {other}", describe(host, port))),
                })
        })
    });
    with_timeout(dial, HANDSHAKE_TIMEOUT)
}

/// `dial`, bounded: a port-forward that hasn't opened within `limit` fails
/// with a message saying so. The one place opening a port-forward is
/// bounded, so every caller of `sandbox_dial` inherits it.
pub fn with_timeout(dial: SandboxDial, limit: Duration) -> SandboxDial {
    Arc::new(move |host, port| {
        let opening = dial(host, port);
        Box::pin(async move {
            tokio::time::timeout(limit, opening).await.unwrap_or_else(|_| {
                Err(DialError::from(format!("Timed out connecting to {} in the sandbox.", describe(host, port))))
            })
        })
    })
}

/// `host:port` as the model or user wrote it: `localhost:3000`, or a
/// container's `172.21.0.2:3000`.
fn describe(host: PodHost, port: u16) -> String {
    match host {
        PodHost::Localhost => format!("localhost:{port}"),
        PodHost::Container(ip) => format!("{ip}:{port}"),
    }
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

/// Starts the proxy for one conversation's browser context: requests for
/// `localhost`, `127.0.0.1` or `::1`, or a Docker container's address in
/// the pod (`fetch_guard::sandbox_host`), go to that port in the
/// conversation's sandbox through `dial` — the only way a browser in
/// smelt's server process can reach them. Every other host is resolved and
/// forwarded as-is. See SME-42 and SME-33.
pub async fn start_with_sandbox(dial: SandboxDial) -> std::io::Result<RoutedProxy> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let accept_task = tokio::spawn(accept_loop(listener, Some(dial)));
    Ok(RoutedProxy { addr, accept_task })
}

async fn accept_loop(listener: TcpListener, sandbox: Option<SandboxDial>) {
    loop {
        match listener.accept().await {
            Ok((client, _)) => {
                let sandbox = sandbox.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle(client, sandbox).await {
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

async fn handle(mut client: TcpStream, sandbox: Option<SandboxDial>) -> Result<(), String> {
    let (head, rest) = tokio::time::timeout(HANDSHAKE_TIMEOUT, read_head(&mut client))
        .await
        .map_err(|_| "timed out reading the request".to_string())??;
    let request = match parse_request_head(&head) {
        Ok(request) => request,
        Err(e) => {
            respond(&mut client, "400 Bad Request").await;
            return Err(e);
        }
    };
    if let Some(dial) = sandbox
        && let Some(host) = fetch_guard::sandbox_host(&request.host)
    {
        return handle_sandbox(client, request, host, rest, dial).await;
    }
    let addrs = match fetch_guard::resolve_target(&request.host, request.port).await {
        Ok(addrs) => addrs,
        Err(e) => {
            respond_with_body(&mut client, "502 Bad Gateway", &e).await;
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
/// one pod. A plain request's reply heads are read before anything goes
/// back, so a port nothing listens on (the stream ends without a byte)
/// becomes a readable 502 rather than an empty reply, and the final head is
/// marked `Connection: close`: the request was checked when its connection
/// opened, so the connection carries no other (SME-165). The tunnel then
/// runs until either side closes; nothing here waits on the pod side's end
/// of stream, which a port-forward reports about a second late.
async fn handle_sandbox(
    mut client: TcpStream,
    request: ProxyRequest,
    host: PodHost,
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
    // Only `localhost` on another port is same-site with `localhost`, and
    // that's the sandbox too (SME-173 review 1).
    let destination = fetch_guard::Destination {
        sends_sec_fetch: fetch_guard::browser_sends_sec_fetch(&request.host),
        same_site_is_home: true,
    };
    if !fetch_guard::allows_request_from(&source, destination, |host, _| fetch_guard::sandbox_host(host).is_some()) {
        let from = source.origin.as_deref().map(|o| format!(" ({o})")).unwrap_or_default();
        let message = format!(
            "Refused: a page from another site{from} tried to reach {} in this conversation's \
             sandbox. Only the sandbox's own pages, and pages opened directly, can reach it.",
            describe(host, request.port)
        );
        respond_with_body(&mut client, "403 Forbidden", &message).await;
        return Err(message);
    }
    // `sandbox_dial` bounds opening the port-forward (`with_timeout`).
    let upstream = match dial(host, request.port).await {
        Ok(upstream) => upstream,
        Err(e) => {
            respond_with_body(&mut client, "502 Bad Gateway", &e.message).await;
            return Err(e.message);
        }
    };
    let (mut from_page, mut to_page) = client.into_split();
    let (mut from_pod, mut to_pod) = tokio::io::split(upstream);
    to_pod.write_all(&first_bytes).await.map_err(io)?;
    // Everything else the page sends goes on to the pod from here — a
    // request body can follow its head in writes of its own (Chrome does
    // this for a larger POST), and the pod won't answer until it has it.
    // Once the page's side ends (it closed, as Chrome does after every
    // sandbox reply, or broke), `gone` says so: nothing more can reach the
    // page, so the pod's connection is let go then, even if the server
    // there keeps its side open (SME-165 review 2). A failed write to the
    // pod doesn't: it may still be answering.
    let (page_gone, gone) = tokio::sync::watch::channel(false);
    let page_to_pod = tokio::spawn(async move {
        let mut chunk = vec![0u8; 16 * 1024];
        loop {
            match from_page.read(&mut chunk).await {
                Ok(0) | Err(_) => {
                    let _ = page_gone.send(true);
                    break;
                }
                Ok(n) => {
                    if to_pod.write_all(&chunk[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = to_pod.shutdown().await;
    });
    if request.forward_head.is_some() {
        // The final reply head goes on marked `Connection: close`, so the
        // browser never sends a second request, which nothing would
        // check, down this connection (SME-165).
        let relayed = tokio::select! {
            relayed = reply::relay_reply_heads(&mut from_pod, &mut to_page, MAX_REPLY_HEAD_BYTES) => relayed,
            () = page_left(gone.clone()) => {
                page_to_pod.abort();
                return Ok(());
            }
        };
        let relayed = match relayed {
            Ok(relayed) => relayed,
            Err(e) => {
                // The pod's connection broke: let it go now, not when the
                // page next sends something (SME-165 review 2).
                page_to_pod.abort();
                return Err(io(e));
            }
        };
        let refusal = match relayed {
            Relayed::CopyRest => None,
            Relayed::Ended => {
                page_to_pod.abort();
                let _ = to_page.shutdown().await;
                return Ok(());
            }
            Relayed::Nothing => Some(format!(
                "Nothing is listening on {} in this conversation's sandbox.",
                describe(host, request.port)
            )),
            Relayed::TooLarge => Some(format!(
                "The server at {} in this conversation's sandbox sent a reply head too large to check.",
                describe(host, request.port)
            )),
        };
        if let Some(message) = refusal {
            page_to_pod.abort();
            respond_with_body(&mut to_page, "502 Bad Gateway", &message).await;
            let _ = to_page.shutdown().await;
            return Err(message);
        }
    }
    let result = tokio::select! {
        result = tokio::io::copy(&mut from_pod, &mut to_page) => result.map(|_| ()),
        () = page_left(gone) => Ok(()),
    };
    let _ = to_page.shutdown().await;
    // The pod's side is done; nothing it could still receive would be read.
    page_to_pod.abort();
    result.map_err(io)
}

/// Resolves once `gone` says the page's side of a sandbox connection has
/// ended; never, if its sender goes without saying so.
async fn page_left(mut gone: tokio::sync::watch::Receiver<bool>) {
    if gone.wait_for(|&gone| gone).await.is_err() {
        std::future::pending::<()>().await;
    }
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
            "referer" => source.referer = value,
            "upgrade-insecure-requests" => source.upgrade_insecure_requests = value.as_deref() == Some("1"),
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

/// A stand-in for `sandbox::open_pod_target` in tests: records each host
/// and port it's asked for and connects to `upstream` whatever they are.
#[cfg(test)]
pub(crate) fn dial_to(
    upstream: SocketAddr,
    dialed: Arc<std::sync::Mutex<Vec<(PodHost, u16)>>>,
) -> SandboxDial {
    Arc::new(move |host, port| {
        dialed.lock().unwrap().push((host, port));
        Box::pin(async move {
            let stream = TcpStream::connect(upstream).await.map_err(|e| e.to_string())?;
            Ok(Box::new(stream) as Box<dyn PodIo>)
        })
    })
}

mod reply;

#[cfg(test)]
mod tests;
