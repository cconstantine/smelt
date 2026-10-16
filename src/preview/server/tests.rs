use std::net::SocketAddr;
use std::sync::Mutex;

use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpStream;

use super::*;
use crate::egress_proxy::{DialError, DialFuture};
use crate::sandbox::PodIo;

/// A dev server stand-in: echoes the `Host` it was sent, redirects
/// to its own `localhost` address, and echoes WebSocket messages.
async fn start_upstream() -> SocketAddr {
    use axum::extract::ws::{Message, WebSocketUpgrade};
    let router = axum::Router::new()
        .route(
            "/",
            axum::routing::get(|headers: axum::http::HeaderMap| async move {
                format!("host={}", headers.get("host").and_then(|h| h.to_str().ok()).unwrap_or(""))
            }),
        )
        .route(
            "/redirect",
            axum::routing::get(|| async {
                (StatusCode::FOUND, [(hyper::header::LOCATION, "http://localhost:3000/landed")])
            }),
        )
        .route(
            "/redirect-container",
            axum::routing::get(|| async {
                (StatusCode::FOUND, [(hyper::header::LOCATION, "http://172.21.0.2:3000/landed")])
            }),
        )
        .route(
            "/ws",
            axum::routing::get(|ws: WebSocketUpgrade| async move {
                ws.on_upgrade(|mut socket| async move {
                    while let Some(Ok(Message::Text(text))) = socket.recv().await {
                        let _ = socket.send(Message::Text(format!("echo: {text}").into())).await;
                    }
                })
            }),
        );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    addr
}

/// Every dial made: (conversation, port).
type Dials = Arc<Mutex<Vec<(i64, u16)>>>;

/// Dials go to `upstream`, whatever the conversation and port.
fn dial_for_upstream(upstream: SocketAddr, dials: Dials) -> DialFor {
    Arc::new(move |conversation| {
        let dials = dials.clone();
        Arc::new(move |_host, port| {
            dials.lock().unwrap().push((conversation, port));
            Box::pin(async move {
                let stream = TcpStream::connect(upstream).await.map_err(|e| e.to_string())?;
                Ok(Box::new(stream) as Box<dyn PodIo>)
            }) as DialFuture
        }) as SandboxDial
    })
}

/// Every dial made, with the host in the pod: (conversation, host, port).
type HostDials = Arc<Mutex<Vec<(i64, PodHost, u16)>>>;

fn dial_hosts_to_upstream(upstream: SocketAddr, dials: HostDials) -> DialFor {
    Arc::new(move |conversation| {
        let dials = dials.clone();
        Arc::new(move |host, port| {
            dials.lock().unwrap().push((conversation, host, port));
            Box::pin(async move {
                let stream = TcpStream::connect(upstream).await.map_err(|e| e.to_string())?;
                Ok(Box::new(stream) as Box<dyn PodIo>)
            }) as DialFuture
        }) as SandboxDial
    })
}

/// Starts the preview server; returns its address and the template
/// it answers to (`http://{port}-{conversation}.preview.localhost:<its port>`).
async fn start_preview(dial_for: DialFor) -> (SocketAddr, PreviewTemplate) {
    start_preview_with("http", "preview.localhost", dial_for, None).await
}

/// `start_preview`, with smelt's own address for links back to it.
async fn start_preview_linking_to(dial_for: DialFor, smelt_url: Option<&str>) -> (SocketAddr, PreviewTemplate) {
    start_preview_with("http", "preview.localhost", dial_for, smelt_url).await
}

/// A preview server whose addresses are
/// `{scheme}://{port}-{conversation}.{domain}:…` (served as plain
/// http here: TLS would be a proxy's job in front of it).
async fn start_preview_at(scheme: &str, domain: &str, dial_for: DialFor) -> (SocketAddr, PreviewTemplate) {
    start_preview_with(scheme, domain, dial_for, None).await
}

/// `start_preview_at`, linking back to `smelt_url`.
async fn start_preview_with(
    scheme: &str,
    domain: &str,
    dial_for: DialFor,
    smelt_url: Option<&str>,
) -> (SocketAddr, PreviewTemplate) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let template = PreviewTemplate::parse(&format!(
        "{scheme}://{{port}}-{{conversation}}.{domain}:{}",
        addr.port()
    ))
    .unwrap();
    tokio::spawn(serve(listener, template.clone(), dial_for, smelt_url.map(str::to_string)));
    (addr, template)
}

/// A client that sends every `*.preview.localhost` name to `preview`.
fn client_for(preview: SocketAddr, hosts: &[&str]) -> reqwest::Client {
    let mut builder = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
    for host in hosts {
        builder = builder.resolve(host, preview);
    }
    builder.build().unwrap()
}

#[tokio::test]
async fn test_a_preview_request_reaches_its_conversations_port_as_localhost() {
    let upstream = start_upstream().await;
    let dials = Dials::default();
    let (preview, template) = start_preview(dial_for_upstream(upstream, dials.clone())).await;
    let client = client_for(preview, &["3000-42.preview.localhost"]);
    let response = client.get(format!("{}/", template.url_for(42, PodHost::Localhost, 3000))).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "host=localhost:3000");
    assert_eq!(*dials.lock().unwrap(), vec![(42, 3000)]);
}

#[tokio::test]
async fn test_each_preview_address_dials_its_own_conversation() {
    let upstream = start_upstream().await;
    let dials = Dials::default();
    let (preview, template) = start_preview(dial_for_upstream(upstream, dials.clone())).await;
    let client = client_for(preview, &["3000-7.preview.localhost", "5173-8.preview.localhost"]);
    for (conversation, port) in [(7, 3000), (8, 5173)] {
        let response = client
            .get(format!("{}/", template.url_for(conversation, PodHost::Localhost, port)))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }
    assert_eq!(*dials.lock().unwrap(), vec![(7, 3000), (8, 5173)]);
}

#[tokio::test]
async fn test_a_host_that_is_not_a_preview_is_not_found() {
    let upstream = start_upstream().await;
    let dials = Dials::default();
    let (preview, _) = start_preview(dial_for_upstream(upstream, dials.clone())).await;
    let client = client_for(preview, &["smelt.preview.localhost"]);
    let response = client
        .get(format!("http://smelt.preview.localhost:{}/", preview.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    assert!(dials.lock().unwrap().is_empty());
}

/// Another site open in the user's browser mustn't reach a preview;
/// a link from smelt, and the same conversation's other previews, can.
#[tokio::test]
async fn test_a_preview_only_answers_its_own_pages_and_direct_visits() {
    let upstream = start_upstream().await;
    let dials = Dials::default();
    let (preview, template) = start_preview(dial_for_upstream(upstream, dials.clone())).await;
    let client = client_for(preview, &["3000-42.preview.localhost"]);
    let url = format!("{}/", template.url_for(42, PodHost::Localhost, 3000));
    let other_port = template.url_for(42, PodHost::Localhost, 5173);
    let other_conversation = template.url_for(7, PodHost::Localhost, 3000);
    let refused: [&[(&str, &str)]; 3] = [
        &[("origin", "https://evil.example"), ("sec-fetch-site", "cross-site"), ("sec-fetch-mode", "cors")],
        &[("sec-fetch-site", "cross-site"), ("sec-fetch-mode", "no-cors")],
        &[("origin", &other_conversation), ("sec-fetch-site", "same-site"), ("sec-fetch-mode", "cors")],
    ];
    for headers in refused {
        let mut request = client.post(&url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 403, "{headers:?}");
        let body = response.text().await.unwrap();
        assert!(body.contains("another site"), "the refusal should say why: {body:?}");
    }
    // Every preview is same-site with every other, so a GET that isn't a
    // page load needs a Referer from this conversation (SME-173).
    let other_conversation_page = format!("{other_conversation}/");
    let refused_gets: [&[(&str, &str)]; 2] = [
        // Conversation 7's <img> of this preview.
        &[("sec-fetch-site", "same-site"), ("sec-fetch-mode", "no-cors"), ("referer", &other_conversation_page)],
        // The same from a no-referrer page, or smelt's markdown image.
        &[("sec-fetch-site", "same-site"), ("sec-fetch-mode", "no-cors")],
    ];
    for headers in refused_gets {
        let mut request = client.get(&url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 403, "{headers:?}");
    }
    assert!(dials.lock().unwrap().is_empty(), "a refused request must not reach the sandbox");

    let other_port_page = format!("{other_port}/");
    let allowed: [&[(&str, &str)]; 4] = [
        // Opening the link from smelt's sandbox panel.
        &[("sec-fetch-site", "cross-site"), ("sec-fetch-mode", "navigate")],
        // The conversation's frontend calling its API on another port.
        &[("origin", &other_port), ("sec-fetch-site", "same-site"), ("sec-fetch-mode", "cors")],
        // The same link where smelt shares the previews' site (noreferrer).
        &[("sec-fetch-site", "same-site"), ("sec-fetch-mode", "navigate"), ("upgrade-insecure-requests", "1")],
        // The conversation's frontend loading a script from another port.
        &[("sec-fetch-site", "same-site"), ("sec-fetch-mode", "no-cors"), ("referer", &other_port_page)],
    ];
    for headers in allowed {
        let mut request = client.get(&url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 200, "{headers:?}");
    }
}

/// Previews at a plain-http address that isn't `localhost` get no
/// Sec-Fetch-* headers from a browser, like a container's address
/// (SME-90): another site's image must still be refused.
#[tokio::test]
async fn test_a_preview_without_sec_fetch_still_refuses_other_sites() {
    let upstream = start_upstream().await;
    let dials = Dials::default();
    let (preview, template) =
        start_preview_at("http", "preview.example.test", dial_for_upstream(upstream, dials.clone())).await;
    let client = client_for(preview, &["3000-42.preview.example.test"]);
    let url = format!("{}/", template.url_for(42, PodHost::Localhost, 3000));
    let other_port = format!("{}/app.js", template.url_for(42, PodHost::Localhost, 5173));
    let other_conversation = format!("{}/", template.url_for(7, PodHost::Localhost, 3000));
    let refused: [&[(&str, &str)]; 3] = [
        &[("referer", "http://evil.test:8703/")],
        &[("referer", &other_conversation)],
        &[],
    ];
    for headers in refused {
        let mut request = client.get(&url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 403, "{headers:?}");
    }
    assert!(dials.lock().unwrap().is_empty(), "a refused request must not reach the sandbox");

    let allowed: [&[(&str, &str)]; 3] = [
        // Opening the link.
        &[("upgrade-insecure-requests", "1")],
        // The preview's own image.
        &[("referer", &url)],
        // The conversation's frontend on another port.
        &[("referer", &other_port)],
    ];
    for headers in allowed {
        let mut request = client.get(&url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), 200, "{headers:?}");
    }
}

/// Over https a browser always sends Sec-Fetch-*, so a request
/// without it there isn't from a browser page: a webhook, or `curl`
/// (SME-90's code review).
#[tokio::test]
async fn test_an_https_preview_lets_requests_from_outside_a_browser_through() {
    let upstream = start_upstream().await;
    let dials = Dials::default();
    let (preview, template) =
        start_preview_at("https", "preview.example.test", dial_for_upstream(upstream, dials.clone())).await;
    let client = client_for(preview, &["3000-42.preview.example.test"]);
    let response = client
        .get(format!("http://3000-42.preview.example.test:{}/", preview.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(*dials.lock().unwrap(), vec![(42, 3000)]);
    assert!(template.url_for(42, PodHost::Localhost, 3000).starts_with("https://"));
}

/// Every dial fails with `error`.
fn dial_failing_with(error: DialError) -> DialFor {
    Arc::new(move |_| {
        let error = error.clone();
        Arc::new(move |_, _| {
            let error = error.clone();
            Box::pin(async move { Err(error) }) as DialFuture
        }) as SandboxDial
    })
}

/// What `egress_proxy::sandbox_dial` fails with when the conversation
/// has no sandbox: a message meant for the model.
fn no_sandbox() -> DialError {
    DialError {
        message: "This conversation has no running sandbox, so there's nothing at port 3000. Start one \
                          with create_pod and run the server there."
            .to_string(),
        no_sandbox: true,
    }
}

#[tokio::test]
async fn test_a_conversation_without_a_sandbox_gets_a_page_linking_back_to_it() {
    let (preview, template) =
        start_preview_linking_to(dial_failing_with(no_sandbox()), Some("http://smelt.test:8180")).await;
    let client = client_for(preview, &["3000-42.preview.localhost"]);
    let response = client.get(format!("{}/", template.url_for(42, PodHost::Localhost, 3000))).send().await.unwrap();
    assert_eq!(response.status(), 503);
    let content_type = response.headers().get("content-type").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    assert!(content_type.starts_with("text/html"), "got {content_type:?}");
    let body = response.text().await.unwrap();
    assert!(body.contains("sandbox isn't running"), "says what's wrong: {body}");
    assert!(body.contains("port 3000"), "names what the link was for: {body}");
    assert!(body.contains(r#"href="http://smelt.test:8180/conversation/42""#), "links to the conversation: {body}");
    assert!(!body.contains("create_pod"), "a tool name is for the model, not the user: {body}");
}

#[tokio::test]
async fn test_without_smelts_address_the_page_names_the_conversation_instead_of_linking() {
    let (preview, template) = start_preview(dial_failing_with(no_sandbox())).await;
    let client = client_for(preview, &["3000-42.preview.localhost"]);
    let response = client.get(format!("{}/", template.url_for(42, PodHost::Localhost, 3000))).send().await.unwrap();
    assert_eq!(response.status(), 503);
    let body = response.text().await.unwrap();
    assert!(body.contains("sandbox isn't running"), "says what's wrong: {body}");
    assert!(body.contains("conversation 42"), "names the conversation: {body}");
    assert!(!body.contains("href="), "no address to link to: {body}");
}

#[tokio::test]
async fn test_the_pages_link_escapes_smelts_address() {
    let (preview, template) =
        start_preview_linking_to(dial_failing_with(no_sandbox()), Some(r#"http://smelt.test/"><script>"#)).await;
    let client = client_for(preview, &["3000-42.preview.localhost"]);
    let response = client.get(format!("{}/", template.url_for(42, PodHost::Localhost, 3000))).send().await.unwrap();
    let body = response.text().await.unwrap();
    assert!(!body.contains("<script>"), "got {body}");
    assert!(body.contains("&quot;&gt;&lt;script&gt;/conversation/42"), "got {body}");
}

#[tokio::test]
async fn test_another_dial_failure_still_says_what_went_wrong_as_text() {
    let failure = DialError::from("Timed out connecting to port 3000 in the sandbox.".to_string());
    let (preview, template) = start_preview_linking_to(dial_failing_with(failure), Some("http://smelt.test")).await;
    let client = client_for(preview, &["3000-42.preview.localhost"]);
    let response = client.get(format!("{}/", template.url_for(42, PodHost::Localhost, 3000))).send().await.unwrap();
    assert_eq!(response.status(), 502);
    assert_eq!(response.text().await.unwrap(), "Timed out connecting to port 3000 in the sandbox.");
}

#[tokio::test]
async fn test_a_port_with_nothing_listening_says_so() {
    // Accepts, then closes without a byte — what a port-forward to
    // an unused port does.
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let silent_addr = silent.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((socket, _)) = silent.accept().await {
            drop(socket);
        }
    });
    let (preview, template) = start_preview(dial_for_upstream(silent_addr, Dials::default())).await;
    let client = client_for(preview, &["3000-42.preview.localhost"]);
    let response = client.get(format!("{}/", template.url_for(42, PodHost::Localhost, 3000))).send().await.unwrap();
    assert_eq!(response.status(), 502);
    let body = response.text().await.unwrap();
    assert!(body.contains("Nothing is listening on port 3000"), "got {body:?}");
}

#[tokio::test]
async fn test_a_redirect_to_the_dev_servers_own_address_stays_on_the_preview() {
    let upstream = start_upstream().await;
    let (preview, template) = start_preview(dial_for_upstream(upstream, Dials::default())).await;
    let client = client_for(preview, &["3000-42.preview.localhost"]);
    let response = client
        .get(format!("{}/redirect", template.url_for(42, PodHost::Localhost, 3000)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 302);
    assert_eq!(
        response.headers().get("location").unwrap(),
        &format!("{}/landed", template.url_for(42, PodHost::Localhost, 3000))
    );
}

/// A container's preview dials that container, which sees itself
/// addressed as it would from a Linux host, and a redirect to its
/// own address stays on the preview (SME-33).
#[tokio::test]
async fn test_a_container_preview_reaches_that_container() {
    let upstream = start_upstream().await;
    let dials = HostDials::default();
    let (preview, template) = start_preview(dial_hosts_to_upstream(upstream, dials.clone())).await;
    let container = PodHost::Container(std::net::Ipv4Addr::new(172, 21, 0, 2));
    let client = client_for(preview, &["172-21-0-2-3000-42.preview.localhost"]);
    let base = template.url_for(42, container, 3000);
    let response = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "host=172.21.0.2:3000");
    assert_eq!(*dials.lock().unwrap(), vec![(42, container, 3000)]);

    let response = client.get(format!("{base}/redirect-container")).send().await.unwrap();
    assert_eq!(response.status(), 302);
    assert_eq!(response.headers().get("location").unwrap(), &format!("{base}/landed"));
}

#[tokio::test]
async fn test_a_kept_alive_connection_reuses_one_port_forward() {
    let upstream = start_upstream().await;
    let dials = Dials::default();
    let (preview, template) = start_preview(dial_for_upstream(upstream, dials.clone())).await;
    let client = client_for(preview, &["3000-42.preview.localhost"]);
    for _ in 0..3 {
        let response = client.get(format!("{}/", template.url_for(42, PodHost::Localhost, 3000))).send().await.unwrap();
        assert_eq!(response.text().await.unwrap(), "host=localhost:3000");
    }
    assert_eq!(dials.lock().unwrap().len(), 1, "one connection, one port-forward");
}

#[tokio::test]
async fn test_a_dev_server_that_dies_mid_response_ends_it_and_the_next_request_redials() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // First connection: promises 100 bytes, sends 7, dies. Later
    // ones answer properly.
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    tokio::spawn(async move {
        let mut first = true;
        while let Ok((mut socket, _)) = upstream.accept().await {
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            if std::mem::take(&mut first) {
                let _ = socket
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial")
                    .await;
            } else {
                let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
            }
        }
    });
    let dials = Dials::default();
    let (preview, template) = start_preview(dial_for_upstream(upstream_addr, dials.clone())).await;
    let client = client_for(preview, &["3000-42.preview.localhost"]);
    let url = format!("{}/", template.url_for(42, PodHost::Localhost, 3000));

    let cut_short = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        client.get(&url).send().await?.text().await
    })
    .await
    .expect("a response cut short must end, not hang");
    assert!(cut_short.is_err(), "a truncated body must be an error, got {cut_short:?}");

    let next = client.get(&url).send().await.expect("the next request").text().await.expect("its body");
    assert_eq!(next, "ok");
}

#[tokio::test]
async fn test_an_upstream_closed_between_requests_is_redialled_on_the_same_connection() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // Answers one request per connection, then closes it — like a
    // dev server dropping an idle keep-alive connection.
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = upstream.accept().await {
            let mut buf = [0u8; 4096];
            let _ = socket.read(&mut buf).await;
            let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
        }
    });
    let dials = Dials::default();
    let (preview, _) = start_preview(dial_for_upstream(upstream_addr, dials.clone())).await;
    let host = format!("3000-42.preview.localhost:{}", preview.port());
    // One browser connection for both requests.
    let mut stream = TcpStream::connect(preview).await.unwrap();
    let mut replies = Vec::new();
    for _ in 0..2 {
        stream
            .write_all(format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("a reply in time")
            .unwrap();
        replies.push(String::from_utf8_lossy(&buf[..n]).to_string());
        // Let the proxy see the upstream close before the next one.
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
    for reply in &replies {
        assert!(reply.starts_with("HTTP/1.1 200") && reply.ends_with("ok"), "got {replies:?}");
    }
    assert_eq!(dials.lock().unwrap().len(), 2, "the closed upstream was replaced");
}

/// Like `dial_for_upstream`, but the upstream's end of stream reaches
/// the proxy a second late — as it does through a real port-forward
/// (see `sandbox::open_pod_target`).
fn dial_for_upstream_with_late_eof(upstream: SocketAddr, dials: Dials) -> DialFor {
    Arc::new(move |conversation| {
        let dials = dials.clone();
        Arc::new(move |_host, port| {
            dials.lock().unwrap().push((conversation, port));
            Box::pin(async move {
                let tcp = TcpStream::connect(upstream).await.map_err(|e| e.to_string())?;
                let (proxy_side, pump_side) = tokio::io::duplex(64 * 1024);
                let (mut from_server, mut to_server) = tcp.into_split();
                let (mut from_proxy, mut to_proxy) = tokio::io::split(pump_side);
                tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut from_server, &mut to_proxy).await;
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    let _ = tokio::io::AsyncWriteExt::shutdown(&mut to_proxy).await;
                });
                tokio::spawn(async move {
                    let _ = tokio::io::copy(&mut from_proxy, &mut to_server).await;
                });
                Ok(Box::new(proxy_side) as Box<dyn PodIo>)
            }) as DialFuture
        }) as SandboxDial
    })
}

#[tokio::test]
async fn test_an_idle_upstream_dropped_just_before_a_request_is_redialled() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    // HTTP/1.1 and kept alive, until the connection has been idle
    // for 100ms — like Node's keep-alive timeout, only shorter.
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = upstream.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut socket, _)) = upstream.accept().await {
            tokio::spawn(async move {
                let mut buf = [0u8; 4096];
                while let Ok(Ok(n)) =
                    tokio::time::timeout(std::time::Duration::from_millis(100), socket.read(&mut buf)).await
                {
                    if n == 0 {
                        return;
                    }
                    let _ = socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok").await;
                }
            });
        }
    });
    let dials = Dials::default();
    let (preview, _) = start_preview(dial_for_upstream_with_late_eof(upstream_addr, dials.clone())).await;
    let host = format!("3000-42.preview.localhost:{}", preview.port());
    let mut stream = TcpStream::connect(preview).await.unwrap();
    let mut replies = Vec::new();
    for pause in [300, 0] {
        stream
            .write_all(format!("GET / HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes())
            .await
            .unwrap();
        let mut buf = vec![0u8; 4096];
        let n = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut buf))
            .await
            .expect("a reply in time")
            .unwrap();
        replies.push(String::from_utf8_lossy(&buf[..n]).to_string());
        // The upstream drops the idle connection at 100ms; the proxy
        // won't hear of it for another second.
        tokio::time::sleep(std::time::Duration::from_millis(pause)).await;
    }
    for reply in &replies {
        assert!(reply.starts_with("HTTP/1.1 200") && reply.ends_with("ok"), "got {replies:?}");
    }
    assert_eq!(dials.lock().unwrap().len(), 2, "the dropped upstream was replaced");
}

#[tokio::test]
async fn test_a_request_without_a_host_is_not_found() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let dials = Dials::default();
    let (preview, _) = start_preview(dial_for_upstream(start_upstream().await, dials.clone())).await;
    let mut stream = TcpStream::connect(preview).await.unwrap();
    stream.write_all(b"GET / HTTP/1.1\r\nConnection: close\r\n\r\n").await.unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).await.unwrap();
    assert!(reply.starts_with("HTTP/1.1 404"), "got {reply:?}");
    assert!(dials.lock().unwrap().is_empty());
}

#[tokio::test]
async fn test_a_websocket_passes_through_to_the_dev_server() {
    let upstream = start_upstream().await;
    let (preview, template) = start_preview(dial_for_upstream(upstream, Dials::default())).await;
    let url = format!("{}/ws", template.url_for(42, PodHost::Localhost, 5173)).replace("http://", "ws://");
    let stream = TcpStream::connect(preview).await.unwrap();
    let (mut socket, _) = tokio_tungstenite::client_async(url, stream).await.expect("upgrade");
    socket
        .send(tokio_tungstenite::tungstenite::Message::Text("hi".into()))
        .await
        .unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
        .await
        .expect("a reply in time")
        .expect("the socket stays open")
        .expect("a message");
    assert_eq!(reply.into_text().unwrap(), "echo: hi");
}

/// A preview server answering to an `https` template that spells out
/// `:443`, as behind a TLS proxy; requests carry the `Host` a browser
/// sends there, without the default port (SME-183).
async fn start_preview_at_default_port(dial_for: DialFor) -> (SocketAddr, PreviewTemplate) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let template = PreviewTemplate::parse("https://{port}-{conversation}.preview.example.test:443").unwrap();
    tokio::spawn(serve(listener, template.clone(), dial_for, None));
    (addr, template)
}

#[tokio::test]
async fn test_a_template_with_its_default_port_serves_the_host_a_browser_sends() {
    let upstream = start_upstream().await;
    let dials = Dials::default();
    let (preview, _) = start_preview_at_default_port(dial_for_upstream(upstream, dials.clone())).await;
    let response = reqwest::Client::new()
        .get(format!("http://{preview}/"))
        .header("host", "3000-42.preview.example.test")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(*dials.lock().unwrap(), vec![(42, 3000)]);
}

#[tokio::test]
async fn test_a_template_with_its_default_port_knows_its_own_pages() {
    let upstream = start_upstream().await;
    let dials = Dials::default();
    let (preview, _) = start_preview_at_default_port(dial_for_upstream(upstream, dials.clone())).await;
    let client = reqwest::Client::new();
    let url = format!("http://{preview}/");
    let host = "3000-42.preview.example.test";
    let response = client
        .get(&url)
        .header("host", host)
        .header("origin", "https://5173-42.preview.example.test")
        .header("sec-fetch-site", "same-site")
        .header("sec-fetch-mode", "cors")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "a fetch from the conversation's own preview");
    let response = client
        .get(&url)
        .header("host", host)
        .header("referer", "https://5173-42.preview.example.test/page")
        .header("sec-fetch-site", "same-site")
        .header("sec-fetch-mode", "no-cors")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "a script loaded by the conversation's own preview");
    let response = client
        .post(&url)
        .header("host", host)
        .header("origin", "https://5173-7.preview.example.test")
        .header("sec-fetch-site", "same-site")
        .header("sec-fetch-mode", "cors")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403, "a POST from another conversation's preview");
    let dials = dials.lock().unwrap();
    assert!(!dials.is_empty() && dials.iter().all(|&d| d == (42, 3000)), "{dials:?}");
}

/// A TLS proxy that writes `Host` as `$host:$server_port` (SME-183's review).
#[tokio::test]
async fn test_a_template_with_its_default_port_serves_a_proxy_host_spelling_it_out() {
    let upstream = start_upstream().await;
    let dials = Dials::default();
    let (preview, _) = start_preview_at_default_port(dial_for_upstream(upstream, dials.clone())).await;
    let response = reqwest::Client::new()
        .get(format!("http://{preview}/"))
        .header("host", "3000-42.preview.example.test:443")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(*dials.lock().unwrap(), vec![(42, 3000)]);
}
