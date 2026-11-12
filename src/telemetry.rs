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

/// Wraps a `fmt` layer's `MakeWriter` so every line it writes passes
/// through [`scrub_urls`] (SME-392): rmcp's own WARN/ERROR lines carry
/// reqwest's error text, which quotes the full request URL — query and
/// key-shaped path segment included — and a dependency's lines are not
/// scrubbed at their source as smelt's are. The `fmt` layer escapes
/// newlines inside field values, so one line is one event; the EnvFilter
/// still runs first, this only rewrites the text.
#[derive(Clone)]
pub(crate) struct Scrubbed<M>(M);

impl<M> Scrubbed<M> {
    pub(crate) fn new(inner: M) -> Self {
        Scrubbed(inner)
    }
}

impl<'a, M> tracing_subscriber::fmt::MakeWriter<'a> for Scrubbed<M>
where
    M: tracing_subscriber::fmt::MakeWriter<'a>,
{
    type Writer = ScrubbingWriter<M::Writer>;

    fn make_writer(&'a self) -> Self::Writer {
        ScrubbingWriter { inner: self.0.make_writer(), line: Vec::new() }
    }
}

/// The line-buffering writer behind [`Scrubbed`].
pub(crate) struct ScrubbingWriter<W: std::io::Write> {
    inner: W,
    line: Vec<u8>,
}

impl<W: std::io::Write> std::io::Write for ScrubbingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        for byte in buf {
            if *byte == b'\n' {
                self.write_line(b"\n")?;
            } else {
                self.line.push(*byte);
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // Keep any line in progress: every event ends at a newline, so a
        // partial line here is fmt mid-write, not a line to release. Drop
        // writes what is left.
        self.inner.flush()
    }
}

impl<W: std::io::Write> Drop for ScrubbingWriter<W> {
    fn drop(&mut self) {
        // A tail that never saw its newline is still scrubbed, and stays
        // newline-free: what arrived is what goes, with URLs cut.
        if !self.line.is_empty() {
            let line = std::mem::take(&mut self.line);
            let _ = self.write_scrubbed(&line, b"");
        }
    }
}

impl<W: std::io::Write> ScrubbingWriter<W> {
    fn write_line(&mut self, terminator: &[u8]) -> std::io::Result<()> {
        let line = std::mem::take(&mut self.line);
        self.write_scrubbed(&line, terminator)
    }

    fn write_scrubbed(&mut self, line: &[u8], terminator: &[u8]) -> std::io::Result<()> {
        // fmt writes UTF-8; lossy keeps a line a dependency garbled whole.
        let text = String::from_utf8_lossy(line);
        self.inner.write_all(scrub_urls(&text).as_bytes())?;
        self.inner.write_all(terminator)
    }
}

/// The export sink's URL scrubber (SME-392): wraps the OTLP exporter and
/// rewrites every string that leaves — each span event's name and string
/// attribute values (an event's message rides its name, a dependency's
/// error text its attributes), the span's own string attribute values,
/// its links' string attribute values (SME-483), and a failed status's
/// description — through [`scrub_urls`] before
/// delegating. Span *names* stay: they're smelt's own; a dependency's
/// text rides events. smelt's own lines were scrubbed or redacted at the
/// source long before this — `scrub_urls` is idempotent, so they come out
/// unchanged — and whatever a dependency quotes (reqwest's `for url (…)`
/// with a key in the query or path is what filed this) loses the secret
/// and keeps the sentence.
#[derive(Debug, Clone)]
pub(crate) struct UrlScrubbed<E> {
    inner: E,
}

impl<E> UrlScrubbed<E> {
    pub(crate) fn new(inner: E) -> Self {
        Self { inner }
    }
}

impl<E> opentelemetry_sdk::trace::SpanExporter for UrlScrubbed<E>
where
    E: opentelemetry_sdk::trace::SpanExporter,
{
    fn export(
        &self,
        batch: Vec<opentelemetry_sdk::trace::SpanData>,
    ) -> impl std::future::Future<Output = opentelemetry_sdk::error::OTelSdkResult> + Send {
        self.inner.export(batch.into_iter().map(scrub_span_data).collect())
    }

    fn shutdown_with_timeout(&self, timeout: std::time::Duration) -> opentelemetry_sdk::error::OTelSdkResult {
        self.inner.shutdown_with_timeout(timeout)
    }

    fn force_flush(&self) -> opentelemetry_sdk::error::OTelSdkResult {
        self.inner.force_flush()
    }

    fn set_resource(&mut self, resource: &opentelemetry_sdk::Resource) {
        self.inner.set_resource(resource)
    }
}

/// `span` with every string an OTLP reader would read scrubbed. Built by
/// rewriting in place: the strings are `Cow`s and rewriting a borrowed one
/// allocates, which is the point.
fn scrub_span_data(mut span: opentelemetry_sdk::trace::SpanData) -> opentelemetry_sdk::trace::SpanData {
    span.attributes = span.attributes.into_iter().map(scrub_key_value).collect();
    for event in span.events.events.iter_mut() {
        event.name = std::borrow::Cow::Owned(scrub_urls(&event.name));
        event.attributes = std::mem::take(&mut event.attributes).into_iter().map(scrub_key_value).collect();
    }
    // SME-483: a link's attribute values can carry the same text an
    // event's, so they scrub the same way. Nothing in smelt calls
    // `follows_from`/`add_link` today — the tracing-opentelemetry bridge
    // supports both, and its `add_link_with_attributes` path would carry
    // caller attributes to the exporter, which is why this branch matters.
    for link in span.links.links.iter_mut() {
        link.attributes = std::mem::take(&mut link.attributes).into_iter().map(scrub_key_value).collect();
    }
    span.status = match span.status {
        opentelemetry::trace::Status::Error { description } => {
            opentelemetry::trace::Status::error(scrub_urls(&description))
        }
        status => status,
    };
    span
}

fn scrub_key_value(pair: opentelemetry::KeyValue) -> opentelemetry::KeyValue {
    let value = match pair.value {
        opentelemetry::Value::String(text) => opentelemetry::Value::String(scrub_urls(text.as_ref()).into()),
        opentelemetry::Value::Array(opentelemetry::Array::String(texts)) => {
            opentelemetry::Value::Array(opentelemetry::Array::String(
                texts.into_iter().map(|text| scrub_urls(text.as_ref()).into()).collect(),
            ))
        }
        value => value,
    };
    opentelemetry::KeyValue::new(pair.key, value)
}

/// The server process's panic hook (SME-483). Panics never enter the
/// tracing pipeline, so neither SME-392 sink can scrub them: without this
/// the default hook writes a panic message quoting a configured MCP URL
/// (an `expect` on an rmcp call, say) to stderr with its key intact.
///
/// The hook keeps the default hook's message shape and stderr destination
/// — [`write_scrubbed_panic_report`] is that shape — and only scrubs the
/// text. It must never panic itself: while a hook runs a panic cannot
/// unwind (std's panic-count guard aborts with "panic in a panic hook"
/// before unwinding, so not even `catch_unwind` here could catch one), so
/// the hook runs no user code and takes no lock beyond stderr's own. The
/// backtrace it prints is always the full one when `RUST_BACKTRACE` asks
/// for one; the default hook's short/full distinction is std-internal and
/// not reproducible on stable. Two more departures from std, neither of
/// which matters here (no deployment sets either variable):
/// `Backtrace::capture` also honours `RUST_LIB_BACKTRACE`, which std's
/// hook ignores; and the hook prints its note or backtrace where std
/// prints none (a `force_no_backtrace` panic, such as one in a destructor
/// during cleanup), and none on a double panic, where std forces a full
/// backtrace.
pub(crate) fn install_scrubbed_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        let name = std::thread::current();
        let location = info
            .location()
            .map(|location| format!("{}:{}:{}", location.file(), location.line(), location.column()));
        // The default hook's tail: the backtrace it captured, or — once
        // per process, as std's own `FIRST_PANIC` — the note that points
        // one out. `Backtrace::capture` never panics.
        let backtrace = std::backtrace::Backtrace::capture();
        let tail = match backtrace.status() {
            std::backtrace::BacktraceStatus::Captured => backtrace.to_string(),
            _ if FIRST_PANIC.swap(false, std::sync::atomic::Ordering::Relaxed) => {
                "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n".to_string()
            }
            _ => String::new(),
        };
        let mut stderr = std::io::stderr().lock();
        let _ = write_scrubbed_panic_report(
            &mut stderr,
            name.name().unwrap_or("<unnamed>"),
            location.as_deref(),
            info.payload(),
            &tail,
        );
    }));
}

/// The first-panic marker behind the note line, mirroring the `FIRST_PANIC`
/// in std's default hook.
static FIRST_PANIC: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// `out` gets the default hook's report of a panic in `name` at `location`
/// with `payload`: a lead newline (the default hook's, so the report
/// starts a line), `thread '<name>' panicked at <file>:<line>:<col>:`,
/// the message, then `tail` — a captured backtrace's text, the note that
/// points one out, or empty — and the whole of it passed through
/// [`scrub_urls`], so a panic quoting a configured URL loses its key
/// (SME-483). The payload is read the way the default hook's
/// `payload_as_str` reads it — a string payload's text, anything else
/// `Box<dyn Any>` — so no user code runs on this path: a panic inside the
/// hook aborts the process.
fn write_scrubbed_panic_report<W: std::io::Write>(
    out: &mut W,
    name: &str,
    location: Option<&str>,
    payload: &dyn std::any::Any,
    tail: &str,
) -> std::io::Result<()> {
    let message = if let Some(text) = payload.downcast_ref::<&str>() {
        *text
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.as_str()
    } else {
        "Box<dyn Any>"
    };
    let location = location.unwrap_or("unknown location");
    let text = format!("\nthread '{name}' panicked at {location}:\n{message}\n{tail}");
    out.write_all(scrub_urls(&text).as_bytes())
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
    // SME-392: every console line passes `scrub_urls` on its way out, so a
    // dependency's own log line (rmcp's transport failures quote the whole
    // request URL) shows its diagnosis without its key.
    let console = tracing_subscriber::fmt::layer()
        .with_writer(Scrubbed::new(std::io::stdout))
        .with_filter(tracing_subscriber::EnvFilter::new(
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
        // SME-392: what leaves for Tempo/Loki passes the same URL scrub as the
    // console — a dependency's WARN event under an smelt span exports its
    // raw text, and rmcp's transport failures quote the whole request URL.
    .with_batch_exporter(UrlScrubbed::new(exporter))
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
    // The same scrubbing decorator as production (SME-392): what the
    // capture's spans hold is what Tempo would get.
    let provider = SdkTracerProvider::builder()
        .with_simple_exporter(UrlScrubbed::new(exporter.clone()))
        .build();
    let console = console.map(|buffer| {
        let writer = Console(buffer);
        // The same scrubbing writer as production (SME-392): what the
        // console tests see is what the console gets.
        tracing_subscriber::fmt::layer()
            .with_writer(Scrubbed::new(move || writer.clone()))
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
