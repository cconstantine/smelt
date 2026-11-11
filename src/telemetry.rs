//! OpenTelemetry tracing (SME-137).
//!
//! smelt's `tracing` spans go over OTLP/HTTP to Tempo when
//! `OTEL_EXPORTER_OTLP_ENDPOINT` is set (the compose stack's `smelt` service
//! sets it to `http://tempo:4318`), and are read in Grafana. With it unset or
//! empty, no exporter is built, no thread started and nothing sent: only the
//! console's `fmt` layer sees the spans.
//!
//! Two filters, one per layer, so console and export verbosity stay apart:
//! the console keeps `RUST_LOG` (`log_filter_directives`), and the export
//! always takes INFO and up, minus the exporter's own HTTP stack and
//! OpenTelemetry's internal logs, which would otherwise feed back into it.
//!
//! What a span never records: message text, tool input or output, headers,
//! query strings, credentials. An error's status description is its first
//! [`STATUS_DESCRIPTION_LIMIT`] characters.

use opentelemetry::KeyValue;
use opentelemetry::trace::TracerProvider as _;
use opentelemetry_sdk::trace::SdkTracerProvider;
use tracing::Span;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::{LevelFilter, Targets};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// How much of an error message a span's status keeps.
pub(crate) const STATUS_DESCRIPTION_LIMIT: usize = 500;

/// The OTLP endpoint to export to, when tracing is on: the value of
/// `OTEL_EXPORTER_OTLP_TRACES_ENDPOINT` or `OTEL_EXPORTER_OTLP_ENDPOINT`,
/// the first that is set and not empty, as the exporter reads them (a
/// blank one is taken, and the exporter then refuses it, logged). The exporter
/// reads the variable itself (adding `/v1/traces`); this only decides
/// whether to build one.
pub(crate) fn otel_endpoint(env: impl Fn(&str) -> Option<String>) -> Option<String> {
    // The exporter prefers the traces-only variable, as the spec says.
    ["OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "OTEL_EXPORTER_OTLP_ENDPOINT"]
        .into_iter()
        .find_map(|name| env(name).filter(|endpoint| !endpoint.is_empty()))
}

/// The database name in a Postgres URL, without its credentials, host or
/// options: what tells the dev server's traces from a check server's.
pub(crate) fn database_name(database_url: &str) -> Option<String> {
    let url = url::Url::parse(database_url).ok()?;
    let name = url.path().trim_start_matches('/');
    let name = percent_encoding::percent_decode_str(name).decode_utf8_lossy();
    (!name.is_empty()).then(|| name.into_owned())
}

/// The resource every exported span carries: `service.name` (`smelt`, or
/// `OTEL_SERVICE_NAME`), `service.version`, `smelt.database` (the name only,
/// from `DATABASE_URL`) and `smelt.port` (`PORT`, 8080 by default).
pub(crate) fn resource_attributes(env: impl Fn(&str) -> Option<String>) -> Vec<KeyValue> {
    let service = env("OTEL_SERVICE_NAME")
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| "smelt".to_string());
    let port = env("PORT").and_then(|port| port.parse::<u16>().ok()).unwrap_or(8080);
    let mut attributes = vec![
        KeyValue::new("service.name", service),
        KeyValue::new("service.version", env!("CARGO_PKG_VERSION")),
        KeyValue::new("smelt.port", i64::from(port)),
    ];
    if let Some(database) = env("DATABASE_URL").as_deref().and_then(database_name) {
        attributes.push(KeyValue::new("smelt.database", database));
    }
    attributes
}

/// `text` with every URL in it cut to its scheme, host and path: userinfo,
/// query and fragment can hold a key or a token. A cut query shows as `?…`,
/// and a key-shaped path segment (SME-392) as `<redacted>`.
pub(crate) fn scrub_urls(text: &str) -> String {
    static URL: std::sync::LazyLock<Option<regex::Regex>> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"[A-Za-z][A-Za-z0-9+.-]*://[^\s()<>"'`]+"#).ok()
    });
    // A pattern that can't compile can't scrub: say nothing rather than leak.
    let Some(url) = URL.as_ref() else { return "(error text withheld)".to_string() };
    url.replace_all(text, |found: &regex::Captures<'_>| {
        let whole = &found[0];
        // Sentence punctuation after a URL isn't part of it.
        let token = whole.trim_end_matches(['.', ',', ';', ':', '!']);
        let tail = &whole[token.len()..];
        let (scheme, rest) = token.split_once("://").unwrap_or(("", token));
        let cut_at = rest.find(['?', '#']).unwrap_or(rest.len());
        let (address, cut) = rest.split_at(cut_at);
        let address = match address.find('/') {
            Some(slash) => {
                let (authority, path) = address.split_at(slash);
                // The path keeps its words (`mcp`, `s`, a version) and loses
                // its key: `…/mcp/s/<key>/mcp` is a configured URL shape
                // (SME-386 M1), and reqwest's error text quotes the whole
                // path — rmcp prints that text into its own lines
                // (SME-392). `<redacted>` re-scrubs to itself.
                let path = path
                    .split('/')
                    .map(|segment| if looks_like_a_key(segment) { "<redacted>" } else { segment })
                    .collect::<Vec<&str>>()
                    .join("/");
                format!("{}{path}", authority.rsplit('@').next().unwrap_or(authority))
            }
            None => address.rsplit('@').next().unwrap_or(address).to_string(),
        };
        let query = if cut.starts_with('?') { "?…" } else { "" };
        format!("{scheme}://{address}{query}{tail}")
    })
    .into_owned()
}

/// Marks `span` as failed with `message` (its URLs scrubbed, cut to
/// [`STATUS_DESCRIPTION_LIMIT`] characters), without logging an event: a
/// failed tool call is the model's to handle, not a new console line. The
/// span must declare `otel.status_code` and `otel.status_description` (as
/// `tracing::field::Empty`), or the record is dropped.
pub(crate) fn mark_error(span: &Span, message: &str) {
    let description: String = scrub_urls(message).chars().take(STATUS_DESCRIPTION_LIMIT).collect();
    span.record("otel.status_code", "ERROR");
    // After the code: the description is what sets the status's text.
    span.record("otel.status_description", description.as_str());
}

/// Whether a URL query value or path segment, seen on its own, could be a
/// credential: 16 characters or more, with both letters and digits, as
/// API keys are and tool names, path words and flags aren't. SME-386 M1's
/// rule for a configured server's URL, moved here for SME-392 so
/// `scrub_urls` and `mcp::call::redact_secrets` share one rule.
pub(crate) fn looks_like_a_key(value: &str) -> bool {
    value.chars().count() >= 16 && value.chars().any(|c| c.is_ascii_digit()) && value.chars().any(|c| c.is_ascii_alphabetic())
}

/// Runs `future` in `span` and ends the span's export when the future
/// finishes or is dropped, whoever still holds the span. Use it instead of
/// `Instrument::instrument` for every exported span: hyper-util spawns each
/// new pooled connection `in_current_span`, and rmcp its service loop, so a
/// span that opened a connection would otherwise stay open (and unexported)
/// for as long as the connection lives, its parents with it.
///
/// A future of its own rather than an `async fn`, which would hold `future`
/// twice (its argument and the instrumented copy it awaits): sandbox
/// futures run on a 2 MB stack in debug builds (SME-115).
pub(crate) fn in_span<F: std::future::Future>(span: Span, future: F) -> InSpan<F> {
    use tracing::Instrument;
    InSpan { _end: EndOnDrop(span.clone()), inner: future.instrument(span) }
}

/// [`in_span`]'s future.
pub(crate) struct InSpan<F> {
    // Dropped first, so the span's work is gone before its export ends.
    inner: tracing::instrument::Instrumented<F>,
    _end: EndOnDrop,
}

impl<F: std::future::Future> std::future::Future for InSpan<F> {
    type Output = F::Output;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<F::Output> {
        // SAFETY: `inner` is pinned structurally: it's never moved out of
        // `self`, and `InSpan` has no `Drop` of its own and isn't `Unpin`
        // unless `inner` is.
        unsafe { self.map_unchecked_mut(|this| &mut this.inner) }.poll(cx)
    }
}

/// Runs `future` on a task of its own, outside every span, and waits for
/// it; dropping the wait stops the task. For a library that keeps
/// `Span::current()` for work outliving the call (rmcp's service loop), so
/// it holds none of the caller's spans. A panic in `future` is the
/// caller's.
pub(crate) async fn outside_spans<F>(future: F) -> F::Output
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    struct AbortOnDrop(tokio::task::AbortHandle);
    impl Drop for AbortOnDrop {
        fn drop(&mut self) {
            self.0.abort();
        }
    }
    // tokio doesn't carry the current span into a spawned task.
    let task = tokio::spawn(future);
    let _abort = AbortOnDrop(task.abort_handle());
    match task.await {
        Ok(output) => output,
        Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
        // Only `_abort` cancels it, and that runs after this returns.
        Err(_) => std::future::pending().await,
    }
}

struct EndOnDrop(Span);

impl Drop for EndOnDrop {
    fn drop(&mut self) {
        end(&self.0);
    }
}

/// Ends `span`'s export now: what it records afterwards is dropped. A no-op
/// when no export layer is installed.
pub(crate) fn end(span: &Span) {
    use opentelemetry::trace::TraceContextExt as _;
    use tracing_opentelemetry::OpenTelemetrySpanExt as _;
    // The SDK ends a span once: tracing's own close, later, is a no-op.
    span.context().span().end();
}

/// The span for one HTTP request: `{METHOD} {path}`, a server span with the
/// method and path only. Never the query string (the OAuth callback's
/// carries `code` and `state`) or a header.
pub(crate) fn make_http_span<B>(request: &http::Request<B>) -> Span {
    let method = request.method().as_str();
    let path = request.uri().path();
    tracing::info_span!(
        "request",
        otel.name = %format_args!("{method} {path}"),
        otel.kind = "server",
        http.request.method = %method,
        url.path = %path,
        http.response.status_code = tracing::field::Empty,
        otel.status_code = tracing::field::Empty,
        otel.status_description = tracing::field::Empty,
    )
}

/// Records a response's status on its request's span, a 5xx marking it
/// failed, and ends the span's export there: its time is the time to the
/// response's head, and a `ServerEvents` stream's span doesn't wait for the
/// tab to close.
pub(crate) fn on_http_response<B>(response: &http::Response<B>, _latency: std::time::Duration, span: &Span) {
    let status = response.status();
    span.record("http.response.status_code", i64::from(status.as_u16()));
    if status.is_server_error() {
        mark_error(span, &status.to_string());
    }
    // The request is answered: a turn it started, a connection it opened or
    // an event stream's body would otherwise keep its span from exporting.
    end(span);
}

/// Keeps the tracer provider for the process's life; dropping it flushes
/// what's queued. `main` holds it. smelt has no graceful shutdown, so in
/// practice up to one batch (about 5 s) of ended spans is lost at exit.
pub(crate) struct TelemetryGuard {
    provider: SdkTracerProvider,
}

impl Drop for TelemetryGuard {
    fn drop(&mut self) {
        let _ = self.provider.shutdown();
    }
}

/// Installs the process's subscriber: the console's `fmt` layer, filtered by
/// `RUST_LOG` as before, and, when [`otel_endpoint`] says so, the OTLP export
/// layer. A failure to build the exporter is logged and leaves tracing off.
pub(crate) fn init() -> Option<TelemetryGuard> {
    let env = |key: &str| std::env::var(key).ok();
    let console = tracing_subscriber::fmt::layer().with_filter(tracing_subscriber::EnvFilter::new(
        crate::log_filter_directives(&env("RUST_LOG").unwrap_or_default()),
    ));
    let registry = tracing_subscriber::registry().with(console);
    if otel_endpoint(env).is_none() {
        registry.init();
        return None;
    }
    // Reads OTEL_EXPORTER_OTLP_ENDPOINT itself, adding `/v1/traces`.
    let exporter = match opentelemetry_otlp::SpanExporter::builder().with_http().build() {
        Ok(exporter) => exporter,
        Err(e) => {
            registry.init();
            tracing::error!(error = %e, "couldn't set up OpenTelemetry export; tracing stays off");
            return None;
        }
    };
    let provider = SdkTracerProvider::builder()
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_attributes(resource_attributes(env))
                .build(),
        )
        .with_batch_exporter(exporter)
        .build();
    registry.with(export_layer(provider.tracer("smelt"))).init();
    Some(TelemetryGuard { provider })
}

/// The export layer: tracing-opentelemetry over `tracer`, with its own fixed
/// filter (INFO and up, minus what would feed back into the export).
fn export_layer<S>(tracer: opentelemetry_sdk::trace::SdkTracer) -> impl Layer<S>
where
    S: tracing::Subscriber + for<'span> tracing_subscriber::registry::LookupSpan<'span>,
{
    tracing_opentelemetry::layer()
        .with_tracer(tracer)
        .with_threads(false)
        .with_filter(export_filter())
}

/// What the export layer takes: smelt's own spans and events from INFO, and
/// everyone else's warnings and errors (as events on smelt's spans).
/// Dependencies' INFO spans stay out: dioxus-signals instruments every
/// signal it makes, which would be a root trace per signal. Targets match
/// by prefix, so `opentelemetry` covers `opentelemetry_sdk`,
/// `opentelemetry-otlp` and `opentelemetry-http` (their internal logs), and
/// `hyper` covers `hyper_util`: the exporter's own HTTP client would
/// otherwise trace its exports.
fn export_filter() -> Targets {
    Targets::new().with_default(LevelFilter::WARN).with_targets([
        ("smelt", LevelFilter::INFO),
        ("opentelemetry", LevelFilter::OFF),
        ("reqwest", LevelFilter::OFF),
        ("hyper", LevelFilter::OFF),
        ("h2", LevelFilter::OFF),
        ("chromiumoxide::conn", LevelFilter::OFF),
        ("chromiumoxide::handler", LevelFilter::OFF),
    ])
}

/// Runs `future` with a subscriber of its own that exports to memory, and
/// returns what it gave and every span that ended meanwhile, through the
/// real export layer. The subscriber is the thread's default
/// (`set_default`), so the test runs on a current-thread runtime, as
/// `#[tokio::test]` and `#[sqlx::test]` do by default; tasks it spawns run
/// on the same thread and are captured too.
#[cfg(test)]
pub(crate) async fn capture_spans<F: std::future::Future>(
    future: F,
) -> (F::Output, Vec<opentelemetry_sdk::trace::SpanData>) {
    capture(None, future).await
}

/// [`capture_spans`], with everything logged meanwhile, at any level,
/// written to `console` as the `fmt` layer would.
#[cfg(test)]
pub(crate) async fn capture_spans_with_console<F: std::future::Future>(
    console: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    future: F,
) -> (F::Output, Vec<opentelemetry_sdk::trace::SpanData>) {
    capture(Some(console), future).await
}

#[cfg(test)]
async fn capture<F: std::future::Future>(
    console: Option<std::sync::Arc<std::sync::Mutex<Vec<u8>>>>,
    future: F,
) -> (F::Output, Vec<opentelemetry_sdk::trace::SpanData>) {
    #[derive(Clone)]
    struct Console(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Console {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    // tracing-core asks the current thread's dispatcher, not the list, while
    // only one dispatcher is registered: with this capture's the only one,
    // a callsite another test's thread hit first was cached as "never" (see
    // `test_a_callsite_first_hit_on_another_thread_is_still_captured`). A
    // second dispatcher kept for the process keeps the list in use.
    static KEEP_THE_DISPATCHER_LIST: std::sync::OnceLock<tracing::Dispatch> = std::sync::OnceLock::new();
    KEEP_THE_DISPATCHER_LIST.get_or_init(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));
    let exporter = opentelemetry_sdk::trace::InMemorySpanExporter::default();
    let provider = SdkTracerProvider::builder().with_simple_exporter(exporter.clone()).build();
    let console = console.map(|buffer| {
        let writer = Console(buffer);
        tracing_subscriber::fmt::layer()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .with_filter(LevelFilter::TRACE)
    });
    let subscriber = tracing_subscriber::registry()
        .with(export_layer(provider.tracer("smelt-test")))
        .with(console);
    let output = {
        let _default = tracing::subscriber::set_default(subscriber);
        future.await
    };
    let spans = exporter.get_finished_spans().expect("the in-memory exporter's spans");
    (output, spans)
}

#[cfg(test)]
mod tests;
