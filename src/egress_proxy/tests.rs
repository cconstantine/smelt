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

async fn start_upstream() -> SocketAddr {
    let router = axum::Router::new().route("/", axum::routing::get(|| async { "upstream says hi" }));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr
}

/// A name that means the upstream test server but is none of the hosts
/// that mean the sandbox, so requests for it go down the plain forward
/// path of a sandbox-routed proxy.
async fn upstream_named() -> (String, SocketAddr) {
    let upstream = start_upstream().await;
    let name = "upstream.smelt-test.invalid";
    fetch_guard::test_dns::set(name, vec![upstream.ip()]);
    (name.to_string(), upstream)
}

fn client_via(proxy: SocketAddr) -> reqwest::Client {
    reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(format!("http://{proxy}")).unwrap())
        .build()
        .unwrap()
}

#[tokio::test]
async fn test_a_sandbox_route_forwards_every_other_host() {
    let (name, upstream) = upstream_named().await;
    let dialed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let proxy = start_with_sandbox(dial_to("127.0.0.1:1".parse().unwrap(), dialed.clone()))
        .await
        .unwrap();
    let response = client_via(proxy.addr)
        .get(format!("http://{name}:{}/", upstream.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "upstream says hi");
    assert!(dialed.lock().unwrap().is_empty(), "a plain host must not be dialed into the sandbox");
}

#[tokio::test]
async fn test_a_sandbox_route_reports_a_host_it_cannot_resolve() {
    let dialed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let proxy = start_with_sandbox(dial_to("127.0.0.1:1".parse().unwrap(), dialed.clone()))
        .await
        .unwrap();
    let response = client_via(proxy.addr).get("http://this-host-does-not-exist.invalid/").send().await.unwrap();
    assert_eq!(response.status(), 502);
    assert!(response.text().await.unwrap().contains("this-host-does-not-exist"), "the reply should say why");
}

async fn connect_through(proxy: SocketAddr, target: &str) -> (TcpStream, String) {
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
async fn test_a_sandbox_route_tunnels_connect_to_every_other_host() {
    let (name, upstream) = upstream_named().await;
    let dialed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let proxy = start_with_sandbox(dial_to("127.0.0.1:1".parse().unwrap(), dialed.clone()))
        .await
        .unwrap();
    let target = format!("{name}:{}", upstream.port());
    let (mut tunnel, reply) = connect_through(proxy.addr, &target).await;
    assert!(reply.starts_with("HTTP/1.1 200"), "got {reply:?}");
    tunnel
        .write_all(format!("GET / HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n").as_bytes())
        .await
        .unwrap();
    let mut response = String::new();
    tunnel.read_to_string(&mut response).await.unwrap();
    assert!(response.ends_with("upstream says hi"), "got {response:?}");
    assert!(dialed.lock().unwrap().is_empty(), "a plain host must not be dialed into the sandbox");
}

#[tokio::test]
async fn test_a_sandbox_route_sends_localhost_to_the_sandbox() {
    let upstream = start_upstream().await;
    let dialed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let proxy = start_with_sandbox(dial_to(upstream, dialed.clone()))
        .await
        .unwrap();
    for host in ["localhost", "127.0.0.1", "[::1]"] {
        let response = client_via(proxy.addr).get(format!("http://{host}:3000/")).send().await.unwrap();
        assert_eq!(response.status(), 200, "{host}");
        assert_eq!(response.text().await.unwrap(), "upstream says hi", "{host}");
    }
    assert_eq!(*dialed.lock().unwrap(), vec![(PodHost::Localhost, 3000); 3]);
}

#[tokio::test]
async fn test_a_sandbox_route_sends_a_container_address_to_that_container() {
    let upstream = start_upstream().await;
    let dialed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let proxy = start_with_sandbox(dial_to(upstream, dialed.clone()))
        .await
        .unwrap();
    // As the model's browser opens it: a page load (SME-90).
    let response = client_via(proxy.addr)
        .get("http://172.21.0.2:3000/")
        .header("upgrade-insecure-requests", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        *dialed.lock().unwrap(),
        vec![(PodHost::Container(std::net::Ipv4Addr::new(172, 21, 0, 2)), 3000)]
    );
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
    let proxy = start_with_sandbox(dial_to(upstream, dialed.clone()))
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
        // A sandbox page on another port that sends `Referrer-Policy:
        // same-origin` (Django's default) or `no-referrer`, loading an
        // image: only `localhost` is same-site with `localhost`, so it's
        // the sandbox's own (SME-173 review 1, measured in Chromium).
        "GET http://localhost:3000/ HTTP/1.1\r\nHost: localhost:3000\r\nSec-Fetch-Site: same-site\r\n\
             Sec-Fetch-Mode: no-cors\r\nSec-Fetch-Dest: image\r\n\r\n",
    ] {
        let reply = send_raw(proxy.addr, head).await;
        assert!(reply.starts_with("HTTP/1.1 200") && reply.ends_with("upstream says hi"), "got {reply:?}");
    }
}

/// A container's address is plain http on a private IP, so Chrome sends
/// it no Sec-Fetch-* headers: these are the heads the real
/// chrome-headless-shell sent in SME-90's spike.
#[tokio::test]
async fn test_a_sandbox_route_refuses_other_sites_requests_to_a_container() {
    let upstream = start_upstream().await;
    let dialed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let proxy = start_with_sandbox(dial_to(upstream, dialed.clone()))
        .await
        .unwrap();
    for head in [
        // Another site's <img>.
        "GET http://172.21.0.2:3000/ HTTP/1.1\r\nHost: 172.21.0.2:3000\r\n\
             Accept: image/avif,image/webp,*/*\r\nReferer: http://evil.test:8703/\r\n\r\n",
        // The same with referrerpolicy=no-referrer: nothing at all.
        "GET http://172.21.0.2:3000/ HTTP/1.1\r\nHost: 172.21.0.2:3000\r\nAccept: */*\r\n\r\n",
        // A no-cors HEAD.
        "HEAD http://172.21.0.2:3000/ HTTP/1.1\r\nHost: 172.21.0.2:3000\r\nAccept: */*\r\n\r\n",
    ] {
        let reply = send_raw(proxy.addr, head).await;
        assert!(reply.starts_with("HTTP/1.1 403"), "got {reply:?}");
        assert!(reply.contains("another site"), "the refusal should say why: {reply:?}");
    }
    assert!(dialed.lock().unwrap().is_empty(), "a refused request must not reach the sandbox");

    // The model opening the page, and the page's own image.
    for head in [
        "GET http://172.21.0.2:3000/ HTTP/1.1\r\nHost: 172.21.0.2:3000\r\n\
             Upgrade-Insecure-Requests: 1\r\nAccept: text/html\r\n\r\n",
        "GET http://172.21.0.2:3000/ HTTP/1.1\r\nHost: 172.21.0.2:3000\r\n\
             Referer: http://172.21.0.2:3000/page\r\n\r\n",
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
    let proxy = start_with_sandbox(dial_to(upstream, dialed.clone()))
        .await
        .unwrap();
    let target: SocketAddr = "127.0.0.1:5173".parse().unwrap();

    let (mut tunnel, reply) = connect_through(proxy.addr, &target.to_string()).await;
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
    let (mut tls, _) = connect_through(proxy.addr, &target.to_string()).await;
    tls.write_all(&[0x16, 0x03, 0x01, 0x00, 0x05, 1, 2, 3, 4, 5]).await.unwrap();
    let mut rest = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), tls.read_to_end(&mut rest))
        .await
        .expect("the tunnel should be closed");
    assert!(dialed.lock().unwrap().is_empty(), "a TLS tunnel must not reach the sandbox");

    // The sandbox's own page opening one still gets through.
    let (mut own, _) = connect_through(proxy.addr, &target.to_string()).await;
    own.write_all(b"GET / HTTP/1.1\r\nHost: localhost:5173\r\nOrigin: http://localhost:5173\r\n\
                        Connection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    own.read_to_string(&mut response).await.unwrap();
    assert!(response.ends_with("upstream says hi"), "got {response:?}");
    assert_eq!(*dialed.lock().unwrap(), vec![(PodHost::Localhost, 5173)]);
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
    let proxy = start_with_sandbox(dial_to(upstream, dialed))
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
    let proxy = start_with_sandbox(dial_to(upstream, dialed.clone()))
        .await
        .unwrap();
    let target: SocketAddr = "127.0.0.1:5173".parse().unwrap();
    let (mut tunnel, reply) = connect_through(proxy.addr, &target.to_string()).await;
    assert!(reply.starts_with("HTTP/1.1 200"), "got {reply:?}");
    tunnel
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost:5173\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let mut response = String::new();
    tunnel.read_to_string(&mut response).await.unwrap();
    assert!(response.ends_with("upstream says hi"), "got {response:?}");
    assert_eq!(*dialed.lock().unwrap(), vec![(PodHost::Localhost, 5173)]);
}

#[tokio::test]
async fn test_a_sandbox_route_that_cannot_connect_says_why() {
    let dial: SandboxDial = Arc::new(|_, _| {
        Box::pin(async { Err(DialError::from("This conversation has no running sandbox.".to_string())) })
    });
    let proxy = start_with_sandbox(dial).await.unwrap();
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
    let proxy = start_with_sandbox(dial_to(silent, dialed))
        .await
        .unwrap();
    let response = client_via(proxy.addr).get("http://localhost:3000/").send().await.unwrap();
    assert_eq!(response.status(), 502);
    let body = response.text().await.unwrap();
    assert!(body.contains("Nothing is listening on localhost:3000"), "got {body:?}");
}

#[sqlx::test]
async fn test_sandbox_dial_without_a_pod_says_how_to_get_one(pool: PgPool) {
    let conversation = crate::db::create_conversation(&pool).await.expect("create conversation");
    let dial = sandbox_dial(pool, conversation.id);
    let error = match dial(PodHost::Localhost, 3000).await {
        Ok(_) => panic!("a conversation without a pod has nothing to connect to"),
        Err(e) => e,
    };
    assert!(error.message.contains("no running sandbox"), "got {error:?}");
    assert!(error.message.contains("create_pod"), "should say how to get one: {error:?}");
    assert!(error.no_sandbox, "a preview tells this case apart from other failures: {error:?}");
}

#[tokio::test]
async fn test_a_dial_that_never_opens_times_out_with_a_reason() {
    let never: SandboxDial = Arc::new(|_, _| Box::pin(std::future::pending()) as DialFuture);
    let dial = with_timeout(never, Duration::from_millis(100));
    let result = tokio::time::timeout(Duration::from_secs(5), dial(PodHost::Localhost, 3000))
        .await
        .expect("the bounded dial must give up on its own");
    let error = match result {
        Ok(_) => panic!("a dial that never opens can't succeed"),
        Err(e) => e,
    };
    assert!(error.message.contains("localhost:3000") && error.message.contains("Timed out"), "got {error:?}");
}

#[sqlx::test]
async fn test_sandbox_dial_refuses_the_sandbox_agents_port(pool: PgPool) {
    let conversation = crate::db::create_conversation(&pool).await.expect("create conversation");
    let dial = sandbox_dial(pool, conversation.id);
    let error = match dial(PodHost::Localhost, 8088).await {
        Ok(_) => panic!("the sandbox agent's port must never be reachable"),
        Err(e) => e,
    };
    assert!(error.message.contains("sandbox agent"), "got {error:?}");
}

#[tokio::test]
async fn test_dropping_a_routed_proxy_stops_it_accepting() {
    let upstream = start_upstream().await;
    let dialed = Arc::new(std::sync::Mutex::new(Vec::new()));
    let proxy = start_with_sandbox(dial_to(upstream, dialed))
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

/// A stand-in for a server in the pod that ignores the proxy's
/// `Connection: close` (SME-165): on each connection it reads one request
/// head and records it, writes `reply` one chunk at a time, 100 ms apart,
/// then echoes whatever else arrives, never closing first unless
/// `close_after`.
async fn start_raw_upstream(reply: Vec<Vec<u8>>, close_after: bool) -> (SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let heads = Arc::new(std::sync::Mutex::new(Vec::new()));
    let seen = heads.clone();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let reply = reply.clone();
            let seen = seen.clone();
            tokio::spawn(async move {
                let Ok((head, _)) = read_head_from(&mut stream, Vec::new()).await else { return };
                seen.lock().unwrap().push(String::from_utf8_lossy(&head).to_string());
                for (i, chunk) in reply.iter().enumerate() {
                    if i > 0 {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    if stream.write_all(chunk).await.is_err() {
                        return;
                    }
                }
                if close_after {
                    return;
                }
                let mut buf = [0u8; 4096];
                while let Ok(n) = stream.read(&mut buf).await {
                    if n == 0 || stream.write_all(&buf[..n]).await.is_err() {
                        return;
                    }
                }
            });
        }
    });
    (addr, heads)
}

/// The model's own navigation to the sandbox, which the SME-90 check lets through.
const NAVIGATION: &str = "GET http://localhost:3000/ HTTP/1.1\r\nHost: localhost:3000\r\n\
                          Sec-Fetch-Site: none\r\nSec-Fetch-Mode: navigate\r\n\r\n";

/// Sends `head` through a sandbox-routed proxy to an upstream answering
/// `reply`, and reads back `expected.len()` bytes (fewer if the stream
/// ends first, or nothing more comes for 5 s). Returns the stream too, still
/// open.
async fn exchange(reply: Vec<&[u8]>, head: &str, expected: &str) -> (String, TcpStream) {
    let (upstream, _) = start_raw_upstream(reply.into_iter().map(<[u8]>::to_vec).collect(), false).await;
    let proxy = start_with_sandbox(dial_to(upstream, Arc::new(std::sync::Mutex::new(Vec::new()))))
        .await
        .unwrap();
    let mut stream = TcpStream::connect(proxy.addr).await.unwrap();
    stream.write_all(head.as_bytes()).await.unwrap();
    let got = read_up_to(&mut stream, expected.len()).await;
    // Dropping the proxy stops its accepting; this connection lives on.
    drop(proxy);
    (got, stream)
}

async fn read_up_to(stream: &mut TcpStream, len: usize) -> String {
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while got.len() < len {
        match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buf)).await {
            Ok(Ok(n)) if n > 0 => got.extend_from_slice(&buf[..n]),
            _ => break,
        }
    }
    String::from_utf8_lossy(&got).to_string()
}

/// SME-165: a pod server answering keep-alive has its reply marked
/// `Connection: close`, so Chrome never sends a second, unchecked request
/// down the connection. The head arrives split across writes, one break
/// inside a header name.
#[tokio::test]
async fn test_a_sandbox_reply_is_marked_connection_close() {
    let expected = "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: text/plain\r\nX-Kept: 1\r\n\
                    Content-Length: 5\r\n\r\nhello";
    let (got, _) = exchange(
        vec![
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nconn",
            b"ection: Keep-Alive\r\nKeep-Alive: timeout=60\r\nProxy-Connection: keep-alive\r\nX-Kept: 1\r\n\
              Content-Length: 5\r\n\r\nhel",
            b"lo",
        ],
        NAVIGATION,
        expected,
    )
    .await;
    assert_eq!(got, expected);
}

/// Chrome finds a status line after up to four bytes of anything, in any
/// case, and a head's end at a bare `\n\n` (`HttpUtil::LocateStartOfStatusLine`,
/// `LocateEndOfHeaders`): such a reply is closed too.
#[tokio::test]
async fn test_a_sandbox_reply_chrome_would_still_read_is_closed() {
    for (reply, expected) in [
        (
            &b"\r\n  http/1.1 200 OK\r\nConnection: keep-alive\r\nContent-Length: 2\r\n\r\nhi"[..],
            "http/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nhi",
        ),
        (
            &b"HTTP/1.1 200 OK\nConnection: keep-alive\nContent-Length: 2\n\nhi"[..],
            "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nhi",
        ),
    ] {
        let (got, _) = exchange(vec![reply], NAVIGATION, expected).await;
        assert_eq!(got, expected);
    }
}

/// An interim `103` goes through unchanged and the final reply after it is
/// the one closed: Chrome decides on the final head (SME-165).
#[tokio::test]
async fn test_a_sandbox_reply_after_early_hints_is_closed() {
    let hints = "HTTP/1.1 103 Early Hints\r\nLink: </a.css>; rel=preload\r\nConnection: keep-alive\r\n\r\n";
    let expected = format!("{hints}HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nhi");
    let (got, _) = exchange(
        vec![hints.as_bytes(), b"HTTP/1.1 200 OK\r\nConnection: keep-alive\r\nContent-Length: 2\r\n\r\nhi"],
        NAVIGATION,
        &expected,
    )
    .await;
    assert_eq!(got, expected);
}

/// A `101` to a request that didn't ask to upgrade is skipped by Chrome
/// like any 1xx, so the reply after it, in the same write here, is closed.
#[tokio::test]
async fn test_a_sandbox_reply_after_an_unasked_101_is_closed() {
    let switching = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\r\n";
    let expected = format!("{switching}HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nhi");
    let reply = format!("{switching}HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi");
    let (got, _) = exchange(vec![reply.as_bytes()], NAVIGATION, &expected).await;
    assert_eq!(got, expected);
}

/// Chrome skips every 1xx, a `101` included, unless the request is its
/// own WebSocket handshake (`HttpNetworkTransaction::DoReadHeadersComplete`),
/// which it sends inside a `CONNECT` tunnel, never as a plain request. So a
/// `101` to a plain request asking to upgrade is interim too, and the
/// reply after it is closed (SME-165 review 1).
#[tokio::test]
async fn test_a_sandbox_reply_after_a_101_to_an_upgrade_request_is_closed() {
    let switching = "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n";
    let expected = format!("{switching}HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nhi");
    let reply = format!("{switching}HTTP/1.1 200 OK\r\nConnection: keep-alive\r\nContent-Length: 2\r\n\r\nhi");
    let (got, _) = exchange(
        vec![reply.as_bytes()],
        "GET http://localhost:3000/ws HTTP/1.1\r\nHost: localhost:3000\r\nConnection: Upgrade\r\n\
         Upgrade: websocket\r\nSec-Fetch-Site: none\r\nSec-Fetch-Mode: navigate\r\n\r\n",
        &expected,
    )
    .await;
    assert_eq!(got, expected);
}

/// A first header line starting with whitespace would be read by Chrome
/// as a continuation of the proxy's `Connection: close`
/// (`AssembleRawHeaders`), turning its value into `close X-Foo: bar`,
/// which `IsKeepAlive` reads as neither close nor keep-alive, so an
/// HTTP/1.1 connection is kept (SME-165 review 1).
#[tokio::test]
async fn test_a_sandbox_reply_whose_first_header_is_a_continuation_is_still_closed() {
    let expected = "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nhi";
    let (got, _) = exchange(
        vec![b"HTTP/1.1 200 OK\r\n X-Foo: bar\r\nContent-Length: 2\r\n\r\nhi"],
        NAVIGATION,
        expected,
    )
    .await;
    assert_eq!(got, expected);
}

/// Chrome accepts a reply head up to 256 KiB (`kMaxHeaderBufSize`), so the
/// proxy passes one that size on rather than refusing it (SME-165 review 1).
#[tokio::test]
async fn test_a_sandbox_reply_head_chrome_accepts_is_passed_on() {
    let filler = "a".repeat(200 * 1024);
    let reply = format!("HTTP/1.1 200 OK\r\nX-Filler: {filler}\r\nContent-Length: 2\r\n\r\nhi");
    let expected = format!("HTTP/1.1 200 OK\r\nConnection: close\r\nX-Filler: {filler}\r\nContent-Length: 2\r\n\r\nhi");
    let (got, _) = exchange(vec![reply.as_bytes()], NAVIGATION, &expected).await;
    assert!(got == expected, "got {:?}", &got[..got.len().min(200)]);
}

/// A reply head too large to check is refused, saying why.
#[tokio::test]
async fn test_a_sandbox_reply_head_too_large_is_refused() {
    let reply = format!("HTTP/1.1 200 OK\r\nX-Filler: {}\r\n\r\n", "a".repeat(260 * 1024));
    let (got, _) = exchange(vec![reply.as_bytes()], NAVIGATION, &"x".repeat(1024)).await;
    assert!(got.starts_with("HTTP/1.1 502"), "got {:?}", &got[..got.len().min(200)]);
    assert!(got.contains("too large"), "the reply should say why: {:?}", &got[..got.len().min(200)]);
}

/// Bytes that aren't an HTTP reply (Chrome reads them as HTTP/0.9, which
/// it never reuses) pass through as they come, without waiting for a head.
#[tokio::test]
async fn test_a_sandbox_reply_that_isnt_http_passes_through() {
    let banner = "SSH-2.0-OpenSSH_9.6\r\n";
    let (got, _) = exchange(vec![banner.as_bytes()], NAVIGATION, banner).await;
    assert_eq!(got, banner);
}

/// A reply that ends partway through its head reaches the page as far as
/// it got, and the connection ends.
#[tokio::test]
async fn test_a_sandbox_reply_cut_off_in_its_head_is_passed_on() {
    let (upstream, _) = start_raw_upstream(vec![b"HTTP/1.1 200 OK\r\nContent-Le".to_vec()], true).await;
    let proxy = start_with_sandbox(dial_to(upstream, Arc::new(std::sync::Mutex::new(Vec::new()))))
        .await
        .unwrap();
    let reply = send_raw(proxy.addr, NAVIGATION).await;
    assert_eq!(reply, "HTTP/1.1 200 OK\r\nContent-Le");
}

/// A pod connection that records when the proxy drops it, and whose reads
/// can be made to fail.
struct TrackedPod {
    inner: TcpStream,
    dropped: Arc<std::sync::atomic::AtomicBool>,
    fail_reads: bool,
}

impl Drop for TrackedPod {
    fn drop(&mut self) {
        self.dropped.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

impl tokio::io::AsyncRead for TrackedPod {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if self.fail_reads {
            return std::task::Poll::Ready(Err(std::io::Error::other("the port-forward broke")));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for TrackedPod {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// A `SandboxDial` connecting to `upstream` through a `TrackedPod`.
fn tracked_dial(upstream: SocketAddr, dropped: Arc<std::sync::atomic::AtomicBool>, fail_reads: bool) -> SandboxDial {
    Arc::new(move |_, _| {
        let dropped = dropped.clone();
        Box::pin(async move {
            let inner = TcpStream::connect(upstream).await.map_err(|e| e.to_string())?;
            Ok(Box::new(TrackedPod { inner, dropped, fail_reads }) as Box<dyn PodIo>)
        })
    })
}

/// Waits up to 5 s for the proxy to drop its pod connection.
async fn dropped_in_time(dropped: &std::sync::atomic::AtomicBool) -> bool {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !dropped.load(std::sync::atomic::Ordering::SeqCst) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    dropped.load(std::sync::atomic::Ordering::SeqCst)
}

/// Once the page has closed its connection (as Chrome does after every
/// sandbox reply now), the proxy lets the pod's go too, even when the
/// server there ignores the end of the request and keeps its side open:
/// each request has a port-forward of its own, and one held per request
/// would pile up (SME-165 review 2).
#[tokio::test]
async fn test_a_sandbox_connection_ends_when_the_page_closes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    // Answers, then holds the connection open, reading nothing more.
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((mut stream, _)) = listener.accept().await {
            let _ = read_head_from(&mut stream, Vec::new()).await;
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi").await;
            held.push(stream);
        }
    });
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let proxy = start_with_sandbox(tracked_dial(upstream, dropped.clone(), false)).await.unwrap();
    let mut page = TcpStream::connect(proxy.addr).await.unwrap();
    page.write_all(NAVIGATION.as_bytes()).await.unwrap();
    let expected = "HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Length: 2\r\n\r\nhi";
    assert_eq!(read_up_to(&mut page, expected.len()).await, expected);
    drop(page);
    assert!(dropped_in_time(&dropped).await, "the pod's connection outlived the page's");
}

/// The same while the pod hasn't answered yet: a page that has gone
/// doesn't keep a port-forward open waiting for a reply nobody will read
/// (SME-165 review 2).
#[tokio::test]
async fn test_a_sandbox_connection_waiting_for_a_reply_ends_when_the_page_closes() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    // Never answers, and holds the connection open.
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let proxy = start_with_sandbox(tracked_dial(upstream, dropped.clone(), false)).await.unwrap();
    let mut page = TcpStream::connect(proxy.addr).await.unwrap();
    page.write_all(NAVIGATION.as_bytes()).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    drop(page);
    assert!(dropped_in_time(&dropped).await, "the pod's connection outlived the page's");
}

/// A pod connection that fails while the reply heads are read is let go
/// at once, not held until the page closes its side (SME-165 review 2).
#[tokio::test]
async fn test_a_sandbox_connection_that_fails_is_let_go() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((stream, _)) = listener.accept().await {
            held.push(stream);
        }
    });
    let dropped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let proxy = start_with_sandbox(tracked_dial(upstream, dropped.clone(), true)).await.unwrap();
    // The page keeps its connection open throughout.
    let mut page = TcpStream::connect(proxy.addr).await.unwrap();
    page.write_all(NAVIGATION.as_bytes()).await.unwrap();
    assert!(dropped_in_time(&dropped).await, "a failed pod connection was held while the page stayed open");
    drop(page);
}
