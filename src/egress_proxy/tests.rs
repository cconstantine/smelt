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
