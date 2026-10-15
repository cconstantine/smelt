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

/// Why `share` didn't record a preview.
#[derive(Debug)]
pub enum ShareError {
    /// The conversation has no running sandbox.
    NoSandbox,
    /// `SMELT_PREVIEW_URL` is unusable, so previews are off.
    PreviewsOff(String),
    Other(String),
}

/// A preview `share` recorded.
pub struct SharedPreview {
    pub url: String,
    /// Whether anything listens on the port yet; the link is recorded
    /// either way.
    pub listening: bool,
}

/// Records a preview of `port` on `host` in `conversation_id`'s running
/// sandbox, so the sandbox panel shows it: saves it on the pod and
/// publishes the pod's whole preview list (SME-42).
pub async fn share(
    pool: &sqlx::PgPool,
    conversation_id: i64,
    host: PodHost,
    port: u16,
) -> Result<SharedPreview, ShareError> {
    let pod_id = crate::sandbox::live_pod_id(pool, conversation_id)
        .await
        .map_err(|e| match e {
            crate::sandbox::TerminalError::NoPod => ShareError::NoSandbox,
            other => ShareError::Other(other.to_string()),
        })?;
    let template = configured_template().map_err(ShareError::PreviewsOff)?;
    let listening = crate::sandbox::pod_port_is_listening(pool, conversation_id, host, port)
        .await
        .map_err(|e| ShareError::Other(e.to_string()))?;
    let stored_host = match host {
        PodHost::Localhost => String::new(),
        PodHost::Container(ip) => ip.to_string(),
    };
    let ports = crate::db::add_pod_preview(pool, pod_id, &stored_host, port)
        .await
        .map_err(|e| ShareError::Other(e.to_string()))?;
    crate::events::publish(
        conversation_id,
        crate::events::ConversationEvent::SandboxPreviewUpdate {
            pod_id,
            previews: preview_links(&template, conversation_id, &stored_previews(&ports)),
        },
    );
    Ok(SharedPreview { url: template.url_for(conversation_id, host, port), listening })
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
        #[expect(clippy::expect_used, reason = "both placeholders were counted exactly once above")]
        let port_at = authority.find(PORT).expect("counted above");
        #[expect(clippy::expect_used, reason = "both placeholders were counted exactly once above")]
        let conversation_at = authority.find(CONVERSATION).expect("counted above");
        let port_first = port_at < conversation_at;
        let (first, second) = if port_first { (PORT, CONVERSATION) } else { (CONVERSATION, PORT) };
        #[expect(clippy::expect_used, reason = "both placeholders were counted exactly once above")]
        let (before, rest) = authority.split_once(first).expect("found above");
        #[expect(clippy::expect_used, reason = "both placeholders were counted exactly once above")]
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
        tokio::spawn(serve(listener, template, dial_for, crate::mcp_oauth::smelt_base_url()));
    }

    /// Serves previews on `listener` for as long as it's open: each
    /// request's `Host` picks the conversation and port (`template`), and
    /// the request goes to that port in that conversation's sandbox.
    pub async fn serve(listener: TcpListener, template: PreviewTemplate, dial_for: DialFor, smelt_url: Option<String>) {
        let template = Arc::new(template);
        let smelt_url = Arc::new(smelt_url);
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(accepted) => accepted,
                Err(e) => {
                    tracing::warn!("preview: accept failed: {e}");
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    continue;
                }
            };
            let (template, dial_for, smelt_url) = (template.clone(), dial_for.clone(), smelt_url.clone());
            tokio::spawn(async move {
                // One upstream connection per browser connection, kept for
                // the requests that follow on it.
                let upstream: CachedUpstream = Arc::default();
                let service = hyper::service::service_fn(move |request| {
                    let (template, dial_for, upstream) = (template.clone(), dial_for.clone(), upstream.clone());
                    let smelt_url = smelt_url.clone();
                    async move {
                        let preview = Preview { template: &template, dial_for: &dial_for, smelt_url: smelt_url.as_deref() };
                        Ok::<_, std::convert::Infallible>(handle(request, &preview, &upstream).await)
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

    /// What every preview request is served with.
    struct Preview<'a> {
        template: &'a PreviewTemplate,
        dial_for: &'a DialFor,
        /// smelt's own address (`SMELT_BASE_URL`), for links back to it.
        smelt_url: Option<&'a str>,
    }

    async fn handle(mut request: Request<Incoming>, preview: &Preview<'_>, cached: &CachedUpstream) -> Response<Body> {
        let template = preview.template;
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
            referer: header("referer"),
            upgrade_insecure_requests: header("upgrade-insecure-requests").as_deref() == Some("1"),
        };
        // Whether the browser sent Sec-Fetch-* to the address it used: it
        // always does over https (a proxy in front of this listener ends
        // TLS), but not to a plain-http one that isn't `localhost` (SME-90).
        let sends_sec_fetch = template.scheme == "https"
            || url::Url::parse(&format!("http://{host}"))
                .ok()
                .and_then(|url| url.host_str().map(crate::fetch_guard::browser_sends_sec_fetch))
                .unwrap_or(false);
        let is_home = |host: &str, port: Option<u16>| {
            let authority = port.map_or_else(|| host.to_string(), |port| format!("{host}:{port}"));
            template.match_host(&authority).is_some_and(|(origin_conversation, _, _)| origin_conversation == conversation)
        };
        if !crate::fetch_guard::allows_request_from(&source, sends_sec_fetch, is_home) {
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
        #[expect(clippy::expect_used, reason = "local_authority is localhost or an IP, then a port: always a valid header value")]
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
            None => match connect(preview, conversation, pod_host, port).await {
                Ok(sender) => (sender, false),
                Err(response) => return response,
            },
        };
        if reused && sender.ready().await.is_err() {
            sender = match connect(preview, conversation, pod_host, port).await {
                Ok(sender) => sender,
                Err(response) => return response,
            };
            reused = false;
        }
        let first_try = sender.send_request(request).await;
        let first_try = match (first_try, retry) {
            (Err(e), Some(retry)) if reused => {
                tracing::debug!(conversation, port, "preview: reused connection gone, retrying: {e}");
                sender = match connect(preview, conversation, pod_host, port).await {
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
        preview: &Preview<'_>,
        conversation: i64,
        host: PodHost,
        port: u16,
    ) -> Result<SendRequest<Body>, Response<Body>> {
        let stream = (preview.dial_for)(conversation)(host, port).await.map_err(|e| {
            if e.no_sandbox {
                no_sandbox_page(conversation, host, port, preview.smelt_url)
            } else {
                text_response(StatusCode::BAD_GATEWAY, &e.message)
            }
        })?;
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

    /// The page a preview link shows when its conversation has no running
    /// sandbox: what's wrong and how to get the preview back, in the user's
    /// terms rather than the model's message (SME-46). Links back to the
    /// conversation when smelt's own address is known.
    fn no_sandbox_page(conversation: i64, host: PodHost, port: u16, smelt_url: Option<&str>) -> Response<Body> {
        let open = match smelt_url {
            Some(base) => format!(
                r#"<a href="{}/conversation/{conversation}">Open the conversation</a>"#,
                escape_html(base.trim_end_matches('/'))
            ),
            None => format!("Open conversation {conversation} in smelt"),
        };
        let page = format!(
            r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Sandbox not running</title>
<style>
body {{ font-family: system-ui, sans-serif; max-width: 36rem; margin: 4rem auto; padding: 0 1rem; line-height: 1.5; color: #1f2328; background: #fff; }}
@media (prefers-color-scheme: dark) {{ body {{ color: #e6edf3; background: #0d1117; }} a {{ color: #4493f8; }} }}
</style>
</head>
<body>
<h1>This preview's sandbox isn't running</h1>
<p>This link shows {what} in the sandbox of conversation {conversation}, and that sandbox isn't running right now, so there's nothing to show.</p>
<p>{open} and ask for the sandbox and the dev server to be started again, then reload this page.</p>
</body>
</html>
"#,
            what = what(host, port),
        );
        response(StatusCode::SERVICE_UNAVAILABLE, "text/html; charset=utf-8", page)
    }

    fn escape_html(text: &str) -> String {
        text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }

    fn text_response(status: StatusCode, text: &str) -> Response<Body> {
        response(status, "text/plain; charset=utf-8", text.to_string())
    }

    fn response(status: StatusCode, content_type: &'static str, body: String) -> Response<Body> {
        let mut response = Response::new(Body::from(body));
        *response.status_mut() = status;
        response.headers_mut().insert(hyper::header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        response
    }

    #[cfg(test)]
    mod tests;
}

#[cfg(test)]
mod tests;
