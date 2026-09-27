//! Sandbox previews for the user's browser (SME-42): each conversation's
//! dev server, on each port, gets an origin of its own — e.g.
//! `http://3000-42.preview.localhost:8181` for port 3000 in conversation
//! 42's pod — served by a reverse proxy on a listener of its own
//! (`SMELT_PREVIEW_ADDR`) that reaches the pod through
//! `sandbox::open_pod_target`.
//!
//! Its own listener rather than a route on smelt's app, because `dx
//! serve`'s dev proxy replaces `Host` before a request reaches the app, and
//! the preview's address is all in `Host`. Its own origin rather than a
//! path under smelt's, because dev servers emit root-absolute paths
//! (`/assets/…`, `/@vite/client`), and so the previewed page can't read
//! smelt's.

/// The default `SMELT_PREVIEW_URL`: `*.localhost` resolves to this machine
/// in browsers with no DNS set up, and `8181` is the port compose
/// publishes.
use crate::sandbox::PodHost;

pub const DEFAULT_PREVIEW_URL: &str = "http://{port}-{conversation}.preview.localhost:8181";

/// Where the preview listener binds unless `SMELT_PREVIEW_ADDR` says
/// otherwise; compose publishes this port.
pub const DEFAULT_PREVIEW_ADDR: &str = "0.0.0.0:8181";

/// A setting's value, with set-but-empty treated as unset (as everywhere
/// else in smelt).
fn setting(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The template every preview address is built from: `SMELT_PREVIEW_URL`,
/// or `DEFAULT_PREVIEW_URL` when it's unset — or why it can't be used.
pub fn configured_template() -> Result<PreviewTemplate, String> {
    let raw = setting("SMELT_PREVIEW_URL").unwrap_or_else(|| DEFAULT_PREVIEW_URL.to_string());
    PreviewTemplate::parse(&raw).map_err(|e| format!("SMELT_PREVIEW_URL is invalid: {e}"))
}

/// The links for `ports` in `conversation_id`'s sandbox, as the sandbox
/// panel shows them.
pub fn preview_links(
    template: &PreviewTemplate,
    conversation_id: i64,
    previews: &[(PodHost, u16)],
) -> Vec<crate::events::SandboxPreview> {
    previews
        .iter()
        .map(|&(host, port)| crate::events::SandboxPreview {
            port,
            host: match host {
                PodHost::Localhost => None,
                PodHost::Container(ip) => Some(ip.to_string()),
            },
            url: template.url_for(conversation_id, host, port),
        })
        .collect()
}

/// Previews as `db::list_pod_previews` stores them — `""` for the pod's
/// localhost, or a container's address — as hosts in the pod.
pub fn stored_previews(rows: &[(String, u16)]) -> Vec<(PodHost, u16)> {
    rows.iter()
        .filter_map(|(host, port)| {
            let host = match host.as_str() {
                "" => PodHost::Localhost,
                address => PodHost::Container(crate::docker_net::container_address(address)?),
            };
            Some((host, *port))
        })
        .collect()
}

/// `SMELT_PREVIEW_URL`, parsed: a scheme and an authority holding one
/// `{port}` and one `{conversation}`, e.g.
/// `https://{port}-{conversation}-smelt.example.com`.
#[derive(Debug, Clone, PartialEq)]
pub struct PreviewTemplate {
    scheme: String,
    /// The authority, lowercased, split around its two placeholders.
    before: String,
    between: String,
    after: String,
    port_first: bool,
}

const PORT: &str = "{port}";
const CONVERSATION: &str = "{conversation}";

impl PreviewTemplate {
    pub fn parse(template: &str) -> Result<Self, String> {
        let (scheme, authority) = template
            .split_once("://")
            .ok_or_else(|| format!("{template:?} needs a scheme, like https://"))?;
        if scheme != "http" && scheme != "https" {
            return Err(format!("{template:?} must be an http:// or https:// address"));
        }
        if authority.contains(['/', '?', '#']) {
            return Err(format!("{template:?} must be just a scheme and host, with no path"));
        }
        let authority = authority.to_ascii_lowercase();
        if authority.matches(PORT).count() != 1 || authority.matches(CONVERSATION).count() != 1 {
            return Err(format!("{template:?} must hold {PORT} and {CONVERSATION} once each"));
        }
        let port_at = authority.find(PORT).expect("counted above");
        let conversation_at = authority.find(CONVERSATION).expect("counted above");
        let port_first = port_at < conversation_at;
        let (first, second) = if port_first { (PORT, CONVERSATION) } else { (CONVERSATION, PORT) };
        let (before, rest) = authority.split_once(first).expect("found above");
        let (between, after) = rest.split_once(second).expect("found above");
        // Something that isn't a digit must separate the two numbers, or
        // "3000" + "42" can't be told from "300" + "042".
        if between.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!(
                "{template:?} needs something other than digits between {PORT} and {CONVERSATION}"
            ));
        }
        Ok(Self {
            scheme: scheme.to_string(),
            before: before.to_string(),
            between: between.to_string(),
            after: after.to_string(),
            port_first,
        })
    }

    /// The preview's base URL for `host:port` in `conversation_id`'s
    /// sandbox: the pod's own localhost, or a Docker container there.
    pub fn url_for(&self, conversation_id: i64, host: PodHost, port: u16) -> String {
        let slot = port_slot(host, port);
        let (first, second) = if self.port_first {
            (slot, conversation_id.to_string())
        } else {
            (conversation_id.to_string(), slot)
        };
        format!("{}://{}{first}{}{second}{}", self.scheme, self.before, self.between, self.after)
    }

    /// The conversation, host in its pod, and port a request's `Host`
    /// header names, if it's a preview address at all.
    pub fn match_host(&self, host: &str) -> Option<(i64, PodHost, u16)> {
        let host = host.to_ascii_lowercase();
        let middle = host.strip_prefix(&self.before)?.strip_suffix(&self.after)?;
        // The conversation is digits only, so it's split off from its own
        // side; the port slot may hold a dashed container address.
        let (slot, conversation) = if self.port_first {
            middle.rsplit_once(&self.between)?
        } else {
            let (conversation, slot) = middle.split_once(&self.between)?;
            (slot, conversation)
        };
        if !is_number(conversation) {
            return None;
        }
        let conversation: i64 = conversation.parse().ok().filter(|&c| c > 0)?;
        let (pod_host, port) = parse_port_slot(slot)?;
        Some((conversation, pod_host, port))
    }
}

/// What fills a preview address's `{port}` slot: the port for the pod's
/// own localhost, or a container's address and port with dashes for dots,
/// e.g. `172-21-0-2-3000` (SME-33).
fn port_slot(host: PodHost, port: u16) -> String {
    match host {
        PodHost::Localhost => port.to_string(),
        PodHost::Container(ip) => {
            let [a, b, c, d] = ip.octets();
            format!("{a}-{b}-{c}-{d}-{port}")
        }
    }
}

/// `port_slot` read back: a port alone, or a container address in the
/// pod's Docker range followed by a port.
fn parse_port_slot(slot: &str) -> Option<(PodHost, u16)> {
    let parts: Vec<&str> = slot.split('-').collect();
    if !parts.iter().all(|p| is_number(p)) {
        return None;
    }
    let (host, port) = match parts[..] {
        [port] => (PodHost::Localhost, port),
        [a, b, c, d, port] => {
            let address = format!("{a}.{b}.{c}.{d}");
            let ip = crate::docker_net::container_address(&address)?;
            // "172.021.0.2" would parse the same; only one spelling counts.
            if ip.to_string() != address {
                return None;
            }
            (PodHost::Container(ip), port)
        }
        _ => return None,
    };
    let port: u16 = port.parse().ok().filter(|&p| p != 0)?;
    Some((host, port))
}

fn is_number(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

pub use server::*;

mod server {
    use std::sync::Arc;

    use axum::body::Body;
    use hyper::body::{Body as _, Incoming};
    use hyper::client::conn::http1::SendRequest;
    use hyper::header::{HOST, HeaderValue, LOCATION, UPGRADE};
    use hyper::{Request, Response, StatusCode};
    use hyper_util::rt::TokioIo;
    use tokio::net::TcpListener;

    use super::PreviewTemplate;
    use crate::sandbox::PodHost;
    use crate::egress_proxy::SandboxDial;

    /// How to reach a conversation's sandbox: `egress_proxy::sandbox_dial`
    /// in the app, a stand-in in tests.
    pub type DialFor = Arc<dyn Fn(i64) -> SandboxDial + Send + Sync>;

    /// Starts the preview listener, from `main`. A bad `SMELT_PREVIEW_URL`
    /// or a port that can't be bound is logged as an error and leaves
    /// previews off; the rest of smelt runs as usual.
    pub async fn start(pool: sqlx::PgPool) {
        let template = match super::configured_template() {
            Ok(template) => template,
            Err(e) => {
                tracing::error!("sandbox previews are off: {e}");
                return;
            }
        };
        let addr = super::setting("SMELT_PREVIEW_ADDR").unwrap_or_else(|| super::DEFAULT_PREVIEW_ADDR.to_string());
        let listener = match TcpListener::bind(&addr).await {
            Ok(listener) => listener,
            Err(e) => {
                tracing::error!("sandbox previews are off: couldn't listen on {addr}: {e}");
                return;
            }
        };
        tracing::info!("sandbox previews on {addr}");
        let dial_for: DialFor = Arc::new(move |conversation| crate::egress_proxy::sandbox_dial(pool.clone(), conversation));
        tokio::spawn(serve(listener, template, dial_for));
    }

    /// Serves previews on `listener` for as long as it's open: each
    /// request's `Host` picks the conversation and port (`template`), and
    /// the request goes to that port in that conversation's sandbox.
    pub async fn serve(listener: TcpListener, template: PreviewTemplate, dial_for: DialFor) {
        let template = Arc::new(template);
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(e) => {
                    tracing::warn!("preview: accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            };
            let (template, dial_for) = (template.clone(), dial_for.clone());
            tokio::spawn(async move {
                // One upstream connection per browser connection, kept for
                // the requests that follow on it.
                let upstream: CachedUpstream = Arc::default();
                let service = hyper::service::service_fn(move |request| {
                    let (template, dial_for, upstream) = (template.clone(), dial_for.clone(), upstream.clone());
                    async move {
                        Ok::<_, std::convert::Infallible>(handle(request, &template, &dial_for, &upstream).await)
                    }
                });
                if let Err(e) = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .with_upgrades()
                    .await
                {
                    tracing::debug!("preview: connection ended: {e}");
                }
            });
        }
    }

    /// A connection to a dev server in a sandbox, and which conversation
    /// and port it's to.
    struct Upstream {
        conversation: i64,
        host: PodHost,
        port: u16,
        sender: SendRequest<Body>,
    }

    type CachedUpstream = Arc<tokio::sync::Mutex<Option<Upstream>>>;

    async fn handle(
        mut request: Request<Incoming>,
        template: &PreviewTemplate,
        dial_for: &DialFor,
        cached: &CachedUpstream,
    ) -> Response<Body> {
        let host = request.headers().get(HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
        let Some((conversation, pod_host, port)) = template.match_host(host) else {
            return text_response(StatusCode::NOT_FOUND, "This isn't a sandbox preview address.");
        };
        // Only this conversation's own previews, and someone opening the link
        // directly: any other site open in the same browser mustn't be able
        // to send requests to its sandbox (SME-42's code review).
        let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        let source = crate::fetch_guard::RequestSource {
            method: request.method().to_string(),
            origin: header("origin"),
            sec_fetch_site: header("sec-fetch-site"),
            sec_fetch_mode: header("sec-fetch-mode"),
        };
        let is_home = |host: &str, port: Option<u16>| {
            let authority = port.map_or_else(|| host.to_string(), |port| format!("{host}:{port}"));
            template.match_host(&authority).is_some_and(|(origin_conversation, _, _)| origin_conversation == conversation)
        };
        if !crate::fetch_guard::allows_request_from(&source, is_home) {
            return text_response(
                StatusCode::FORBIDDEN,
                "Refused: a page from another site tried to reach this preview. Open the preview's \
                 link from smelt instead.",
            );
        }
        // The dev server sees itself addressed as it would locally, so a
        // host check (Vite's, for one) accepts the request: `localhost`, or
        // the container's own address, as from a Linux host (SME-33).
        let local = local_authority(pod_host, port);
        request.headers_mut().insert(HOST, HeaderValue::from_str(&local).expect("a valid Host"));
        let downstream_upgrade = request
            .headers()
            .contains_key(UPGRADE)
            .then(|| hyper::upgrade::on(&mut request));

        // A request with no body can go again on a fresh connection when a
        // reused one turns out to be gone. `is_closed` can't be trusted for
        // that: a port-forward reports a dev server dropping an idle
        // connection about a second late (`sandbox::open_pod_target`).
        let retry = request.body().is_end_stream().then(|| {
            let mut again = Request::new(Body::empty());
            *again.method_mut() = request.method().clone();
            *again.uri_mut() = request.uri().clone();
            *again.version_mut() = request.version();
            *again.headers_mut() = request.headers().clone();
            again
        });
        let request = request.map(Body::new);

        // Held until the response head is back: HTTP/1 is one request at a
        // time per connection anyway.
        let mut cached = cached.lock().await;
        let reusable = cached
            .take()
            .filter(|up| {
                up.conversation == conversation && up.host == pod_host && up.port == port && !up.sender.is_closed()
            });
        let (mut sender, mut reused) = match reusable {
            Some(up) => (up.sender, true),
            None => match connect(dial_for, conversation, pod_host, port).await {
                Ok(sender) => (sender, false),
                Err(response) => return response,
            },
        };
        if reused && sender.ready().await.is_err() {
            sender = match connect(dial_for, conversation, pod_host, port).await {
                Ok(sender) => sender,
                Err(response) => return response,
            };
            reused = false;
        }
        let first_try = sender.send_request(request).await;
        let first_try = match (first_try, retry) {
            (Err(e), Some(retry)) if reused => {
                tracing::debug!(conversation, port, "preview: reused connection gone, retrying: {e}");
                sender = match connect(dial_for, conversation, pod_host, port).await {
                    Ok(sender) => sender,
                    Err(response) => return response,
                };
                reused = false;
                sender.send_request(retry).await
            }
            (result, _) => result,
        };
        let mut response = match first_try {
            Ok(response) => response,
            Err(e) => {
                tracing::debug!(conversation, port, reused, "preview: no response: {e}");
                let message = if reused {
                    format!(
                        "The connection to {} in this conversation's sandbox closed. \
                         Reload to try again.",
                        what(pod_host, port)
                    )
                } else {
                    format!("Nothing is listening on {} in this conversation's sandbox.", what(pod_host, port))
                };
                return text_response(StatusCode::BAD_GATEWAY, &message);
            }
        };
        match downstream_upgrade {
            // An upgraded connection belongs to that one exchange.
            Some(downstream) if response.status() == StatusCode::SWITCHING_PROTOCOLS => {
                let upstream = hyper::upgrade::on(&mut response);
                tokio::spawn(async move {
                    match tokio::try_join!(downstream, upstream) {
                        Ok((downstream, upstream)) => {
                            let _ = tokio::io::copy_bidirectional(
                                &mut TokioIo::new(downstream),
                                &mut TokioIo::new(upstream),
                            )
                            .await;
                        }
                        Err(e) => tracing::debug!("preview: upgrade failed: {e}"),
                    }
                });
            }
            _ => *cached = Some(Upstream { conversation, host: pod_host, port, sender }),
        }
        drop(cached);

        rewrite_location(&mut response, pod_host, port, &template.url_for(conversation, pod_host, port));
        response.map(Body::new)
    }

    /// A fresh connection to `port` in `conversation`'s sandbox, or the
    /// response saying why there isn't one.
    async fn connect(
        dial_for: &DialFor,
        conversation: i64,
        host: PodHost,
        port: u16,
    ) -> Result<SendRequest<Body>, Response<Body>> {
        let stream = dial_for(conversation)(host, port)
            .await
            .map_err(|message| text_response(StatusCode::BAD_GATEWAY, &message))?;
        let (sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .map_err(|e| {
                text_response(
                    StatusCode::BAD_GATEWAY,
                    &format!("Couldn't talk to {} in the sandbox: {e}", what(host, port)),
                )
            })?;
        tokio::spawn(async move {
            if let Err(e) = connection.with_upgrades().await {
                tracing::debug!(conversation, port, "preview: upstream connection ended: {e}");
            }
        });
        Ok(sender)
    }

    /// Points a redirect to the dev server's own local address back at the
    /// preview, so following it stays on the preview.
    /// What a preview reaches, for a message: `port 3000`, or a container's
    /// `172.21.0.2:3000`.
    fn what(host: PodHost, port: u16) -> String {
        match host {
            PodHost::Localhost => format!("port {port}"),
            PodHost::Container(ip) => format!("{ip}:{port}"),
        }
    }

    /// `host:port` as the dev server would be addressed from the pod, or
    /// from a Linux host for a container.
    fn local_authority(host: PodHost, port: u16) -> String {
        match host {
            PodHost::Localhost => format!("localhost:{port}"),
            PodHost::Container(ip) => format!("{ip}:{port}"),
        }
    }

    fn rewrite_location(response: &mut Response<Incoming>, host: PodHost, port: u16, preview_base: &str) {
        let Some(location) = response.headers().get(LOCATION).and_then(|l| l.to_str().ok()) else {
            return;
        };
        let names: Vec<String> = match host {
            PodHost::Localhost => ["localhost", "127.0.0.1", "[::1]"].map(String::from).to_vec(),
            PodHost::Container(ip) => vec![ip.to_string()],
        };
        let rewritten = names.iter().find_map(|host| {
            let local = format!("http://{host}:{port}");
            let rest = location.strip_prefix(&local)?;
            (rest.is_empty() || rest.starts_with(['/', '?', '#'])).then(|| format!("{preview_base}{rest}"))
        });
        if let Some(rewritten) = rewritten.and_then(|l| HeaderValue::from_str(&l).ok()) {
            response.headers_mut().insert(LOCATION, rewritten);
        }
    }

    fn text_response(status: StatusCode, text: &str) -> Response<Body> {
        Response::builder()
            .status(status)
            .header(hyper::header::CONTENT_TYPE, "text/plain; charset=utf-8")
            .body(Body::from(text.to_string()))
            .expect("a valid response")
    }

    #[cfg(test)]
    mod tests {
        use std::net::SocketAddr;
        use std::sync::Mutex;

        use futures_util::{SinkExt, StreamExt};
        use tokio::net::TcpStream;

        use super::*;
        use crate::egress_proxy::DialFuture;
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
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let template = PreviewTemplate::parse(&format!(
                "http://{{port}}-{{conversation}}.preview.localhost:{}",
                addr.port()
            ))
            .unwrap();
            tokio::spawn(serve(listener, template.clone(), dial_for));
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
            assert!(dials.lock().unwrap().is_empty(), "a refused request must not reach the sandbox");

            let allowed: [&[(&str, &str)]; 2] = [
                // Opening the link from smelt's sandbox panel.
                &[("sec-fetch-site", "cross-site"), ("sec-fetch-mode", "navigate")],
                // The conversation's frontend calling its API on another port.
                &[("origin", &other_port), ("sec-fetch-site", "same-site"), ("sec-fetch-mode", "cors")],
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

        #[tokio::test]
        async fn test_a_conversation_without_a_sandbox_says_so() {
            let dial_for: DialFor = Arc::new(|_| {
                Arc::new(|_, _| {
                    Box::pin(async { Err("This conversation has no running sandbox.".to_string()) }) as DialFuture
                }) as SandboxDial
            });
            let (preview, template) = start_preview(dial_for).await;
            let client = client_for(preview, &["3000-42.preview.localhost"]);
            let response = client.get(format!("{}/", template.url_for(42, PodHost::Localhost, 3000))).send().await.unwrap();
            assert_eq!(response.status(), 502);
            assert_eq!(response.text().await.unwrap(), "This conversation has no running sandbox.");
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(s: &str) -> PreviewTemplate {
        PreviewTemplate::parse(s).expect("a valid template")
    }

    #[test]
    fn test_the_default_template_builds_and_matches_dev_addresses() {
        let t = template(DEFAULT_PREVIEW_URL);
        assert_eq!(t.url_for(42, PodHost::Localhost, 3000), "http://3000-42.preview.localhost:8181");
        assert_eq!(t.match_host("3000-42.preview.localhost:8181"), Some((42, PodHost::Localhost, 3000)));
        assert_eq!(t.match_host("3000-42.Preview.LOCALHOST:8181"), Some((42, PodHost::Localhost, 3000)));
    }

    #[test]
    fn test_a_production_template_under_a_wildcard_domain() {
        let t = template("https://{port}-{conversation}-smelt.constantlee.us");
        assert_eq!(t.url_for(7, PodHost::Localhost, 5173), "https://5173-7-smelt.constantlee.us");
        assert_eq!(t.match_host("5173-7-smelt.constantlee.us"), Some((7, PodHost::Localhost, 5173)));
    }

    #[test]
    fn test_the_placeholders_can_come_in_either_order() {
        let t = template("https://c{conversation}-p{port}.example.com");
        assert_eq!(t.url_for(12, PodHost::Localhost, 8080), "https://c12-p8080.example.com");
        assert_eq!(t.match_host("c12-p8080.example.com"), Some((12, PodHost::Localhost, 8080)));
    }

    #[test]
    fn test_a_container_preview_carries_its_address_in_the_port_slot() {
        let container = PodHost::Container(std::net::Ipv4Addr::new(172, 21, 0, 2));
        let t = template(DEFAULT_PREVIEW_URL);
        assert_eq!(t.url_for(42, container, 3000), "http://172-21-0-2-3000-42.preview.localhost:8181");
        assert_eq!(
            t.match_host("172-21-0-2-3000-42.preview.localhost:8181"),
            Some((42, container, 3000))
        );
        let t = template("https://c{conversation}-p{port}.example.com");
        assert_eq!(t.url_for(12, container, 8080), "https://c12-p172-21-0-2-8080.example.com");
        assert_eq!(t.match_host("c12-p172-21-0-2-8080.example.com"), Some((12, container, 8080)));
    }

    #[test]
    fn test_container_preview_hosts_outside_the_pod_or_malformed_do_not_match() {
        let t = template(DEFAULT_PREVIEW_URL);
        for host in [
            // Outside the pod's Docker range.
            "172-24-0-1-3000-42.preview.localhost:8181",
            "10-43-0-1-443-42.preview.localhost:8181",
            // Not an address and a port.
            "21-0-2-3000-42.preview.localhost:8181",
            "172-21-0-2-42.preview.localhost:8181",
            "172-21-0-2-0-42.preview.localhost:8181",
            "172-21-0-256-3000-42.preview.localhost:8181",
            "172-21-0-2-3000--42.preview.localhost:8181",
        ] {
            assert_eq!(t.match_host(host), None, "{host:?} is not a preview");
        }
    }

    #[test]
    fn test_hosts_that_are_not_previews_do_not_match() {
        let t = template("https://{port}-{conversation}-smelt.constantlee.us");
        for host in [
            "smelt.constantlee.us",
            "constantlee.us",
            "abc-42-smelt.constantlee.us",
            "3000-abc-smelt.constantlee.us",
            "3000--smelt.constantlee.us",
            "-42-smelt.constantlee.us",
            "3000-42-smelt.constantlee.us.evil.example",
            "evil.example.3000-42-smelt.constantlee.us",
            "3000-42-smelt.constantlee.us:8443",
            "70000-1-smelt.constantlee.us",
            "0-1-smelt.constantlee.us",
            "+3000-42-smelt.constantlee.us",
            "3000-99999999999999999999-smelt.constantlee.us",
            "",
        ] {
            assert_eq!(t.match_host(host), None, "{host:?} is not a preview");
        }
    }

    #[test]
    fn test_preview_links_pair_each_port_with_its_address() {
        let t = template(DEFAULT_PREVIEW_URL);
        let container = PodHost::Container(std::net::Ipv4Addr::new(172, 21, 0, 2));
        let links = preview_links(&t, 9, &[(PodHost::Localhost, 3000), (PodHost::Localhost, 5173), (container, 3000)]);
        let link = |port, host: Option<&str>, url: &str| crate::events::SandboxPreview {
            port,
            host: host.map(str::to_string),
            url: url.to_string(),
        };
        assert_eq!(
            links,
            vec![
                link(3000, None, "http://3000-9.preview.localhost:8181"),
                link(5173, None, "http://5173-9.preview.localhost:8181"),
                link(3000, Some("172.21.0.2"), "http://172-21-0-2-3000-9.preview.localhost:8181"),
            ]
        );
    }

    #[test]
    fn test_templates_missing_a_placeholder_scheme_or_with_a_path_are_refused() {
        for bad in [
            "https://{port}-smelt.example.com",
            "https://{conversation}-smelt.example.com",
            "{port}-{conversation}.example.com",
            "ftp://{port}-{conversation}.example.com",
            "https://{port}-{conversation}.example.com/preview",
            "https://{port}{conversation}.example.com",
            "https://{port}-{port}-{conversation}.example.com",
        ] {
            assert!(PreviewTemplate::parse(bad).is_err(), "{bad:?} should be refused");
        }
    }
}
