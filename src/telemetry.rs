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
/// `OTEL_EXPORTER_OTLP_ENDPOINT`, unless it's unset or blank. The exporter
/// reads the variable itself (adding `/v1/traces`); this only decides
/// whether to build one.
pub(crate) fn otel_endpoint(env: impl Fn(&str) -> Option<String>) -> Option<String> {
    env("OTEL_EXPORTER_OTLP_ENDPOINT").filter(|endpoint| !endpoint.trim().is_empty())
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

/// Marks `span` as failed with `message` (cut to
/// [`STATUS_DESCRIPTION_LIMIT`] characters), without logging an event: a
/// failed tool call is the model's to handle, not a new console line. The
/// span must declare `otel.status_code` and `otel.status_description` (as
/// `tracing::field::Empty`), or the record is dropped.
pub(crate) fn mark_error(span: &Span, message: &str) {
    let description: String = message.chars().take(STATUS_DESCRIPTION_LIMIT).collect();
    span.record("otel.status_code", "ERROR");
    // After the code: the description is what sets the status's text.
    span.record("otel.status_description", description.as_str());
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

/// What the export layer takes. Targets match by prefix, so `opentelemetry`
/// covers `opentelemetry_sdk`, `opentelemetry-otlp` and `opentelemetry-http`
/// (their internal logs), and `hyper` covers `hyper_util`: the exporter's
/// HTTP client would otherwise trace its own exports.
fn export_filter() -> Targets {
    Targets::new().with_default(LevelFilter::INFO).with_targets([
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
mod tests {
    use super::*;
    use opentelemetry::trace::{SpanKind, Status};
    use std::collections::HashMap;

    fn env_of(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: HashMap<String, String> = vars.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |key| vars.get(key).cloned()
    }

    fn attribute<'a>(attributes: &'a [KeyValue], key: &str) -> Option<&'a opentelemetry::Value> {
        attributes.iter().find(|kv| kv.key.as_str() == key).map(|kv| &kv.value)
    }

    #[test]
    fn test_tracing_is_off_unless_the_endpoint_is_set() {
        assert_eq!(otel_endpoint(env_of(&[])), None);
        assert_eq!(otel_endpoint(env_of(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "")])), None);
        assert_eq!(otel_endpoint(env_of(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "  ")])), None);
        assert_eq!(
            otel_endpoint(env_of(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://tempo:4318")])),
            Some("http://tempo:4318".to_string())
        );
    }

    #[test]
    fn test_the_resource_names_the_service_database_and_port_without_credentials() {
        let attributes = resource_attributes(env_of(&[
            ("DATABASE_URL", "postgres://smelt:hunter2-secret@postgres:5432/smelt_check_42?sslmode=disable"),
            ("PORT", "8081"),
        ]));
        assert_eq!(attribute(&attributes, "service.name"), Some(&"smelt".into()));
        assert_eq!(attribute(&attributes, "smelt.database"), Some(&"smelt_check_42".into()));
        assert_eq!(attribute(&attributes, "smelt.port"), Some(&8081_i64.into()));
        assert!(attribute(&attributes, "service.version").is_some(), "{attributes:?}");
        let all = format!("{attributes:?}");
        assert!(!all.contains("hunter2"), "the password leaked: {all}");
        assert!(!all.contains("postgres:5432"), "the host leaked: {all}");
    }

    #[test]
    fn test_the_resource_takes_the_service_name_from_the_environment_and_defaults_the_port() {
        let attributes = resource_attributes(env_of(&[("OTEL_SERVICE_NAME", "smelt-dev")]));
        assert_eq!(attribute(&attributes, "service.name"), Some(&"smelt-dev".into()));
        assert_eq!(attribute(&attributes, "smelt.port"), Some(&8080_i64.into()));
        assert_eq!(attribute(&attributes, "smelt.database"), None);
    }

    #[test]
    fn test_database_name_is_only_the_path() {
        assert_eq!(database_name("postgres://u:p@h/smelt").as_deref(), Some("smelt"));
        assert_eq!(database_name("postgres://h:5432/").as_deref(), None);
        assert_eq!(database_name("not a url").as_deref(), None);
    }

    #[tokio::test]
    async fn test_an_http_span_has_the_path_but_never_the_query() {
        let ((), spans) = capture_spans(async {
            let request = http::Request::get("/oauth/mcp-callback/3?code=SECRET&state=X")
                .header("cookie", "session=SECRET")
                .body(())
                .expect("request");
            let span = make_http_span(&request);
            let response = http::Response::builder().status(404).body(()).expect("response");
            on_http_response(&response, std::time::Duration::from_millis(1), &span);
        })
        .await;
        let [span] = spans.as_slice() else { panic!("one span: {spans:?}") };
        assert_eq!(span.name, "GET /oauth/mcp-callback/3");
        assert_eq!(span.span_kind, SpanKind::Server);
        assert_eq!(attribute(&span.attributes, "url.path"), Some(&"/oauth/mcp-callback/3".into()));
        assert_eq!(attribute(&span.attributes, "http.request.method"), Some(&"GET".into()));
        assert_eq!(attribute(&span.attributes, "http.response.status_code"), Some(&404_i64.into()));
        assert_eq!(span.status, Status::Unset, "a 404 isn't the server failing");
        assert!(!format!("{span:?}").contains("SECRET"), "{span:?}");
    }

    /// A turn the request started (`start_turn`) and a connection it opened
    /// hold the request's span: it still exports at its response, and an
    /// event stream's span doesn't wait for the tab to close.
    #[tokio::test]
    async fn test_an_http_span_still_held_elsewhere_exports_at_its_response() {
        let mut held = None;
        let ((), spans) = capture_spans(async {
            let request = http::Request::get("/api/conversations/1/events").body(()).expect("request");
            let span = make_http_span(&request);
            held = Some(span.clone());
            let response = http::Response::builder().status(200).body(()).expect("response");
            on_http_response(&response, std::time::Duration::from_millis(1), &span);
        })
        .await;
        assert!(held.is_some());
        let names: Vec<_> = spans.iter().map(|span| span.name.to_string()).collect();
        assert_eq!(names, ["GET /api/conversations/1/events"]);
    }

    #[tokio::test]
    async fn test_a_5xx_response_marks_its_span_failed() {
        let ((), spans) = capture_spans(async {
            let request = http::Request::post("/api/x").body(()).expect("request");
            let span = make_http_span(&request);
            let response = http::Response::builder().status(500).body(()).expect("response");
            on_http_response(&response, std::time::Duration::from_millis(1), &span);
        })
        .await;
        let [span] = spans.as_slice() else { panic!("one span: {spans:?}") };
        assert!(matches!(span.status, Status::Error { .. }), "{:?}", span.status);
    }

    /// A library task that outlives the work (hyper-util spawns each pooled
    /// connection `in_current_span`, rmcp its service loop) holds the span
    /// open: it must still export when the work ends.
    #[tokio::test]
    async fn test_a_span_held_by_a_lingering_task_still_exports_when_its_work_ends() {
        let held = std::sync::Arc::new(std::sync::Mutex::new(None));
        let holder = held.clone();
        let ((), spans) = capture_spans(async move {
            in_span(tracing::info_span!("work"), async move {
                *holder.lock().expect("lock") = Some(Span::current());
            })
            .await;
        })
        .await;
        assert!(held.lock().expect("lock").is_some(), "the span is still held");
        let names: Vec<_> = spans.iter().map(|span| span.name.to_string()).collect();
        assert_eq!(names, ["work"]);
    }

    #[tokio::test]
    async fn test_a_span_whose_work_is_dropped_still_exports() {
        let ((), spans) = capture_spans(async {
            let held = std::sync::Arc::new(std::sync::Mutex::new(None));
            let holder = held.clone();
            let work = in_span(tracing::info_span!("work"), async move {
                *holder.lock().expect("lock") = Some(Span::current());
                std::future::pending::<()>().await
            });
            let _ = tokio::time::timeout(std::time::Duration::from_millis(10), work).await;
            std::mem::forget(held);
        })
        .await;
        let names: Vec<_> = spans.iter().map(|span| span.name.to_string()).collect();
        assert_eq!(names, ["work"]);
    }

    /// tracing-core keeps one dispatcher list for the whole process, and
    /// with only one registered it asks the *current thread's* dispatcher
    /// instead. With a capture's subscriber the only one registered, a
    /// callsite another test's thread hit first was cached as "never", and
    /// a capture missed its span (the chat-span test, in a full run). A
    /// span first made on another thread is still captured here.
    #[tokio::test]
    async fn test_a_callsite_first_hit_on_another_thread_is_still_captured() {
        fn fresh() -> Span {
            tracing::info_span!("first_hit_elsewhere")
        }
        let ((), spans) = capture_spans(async {
            std::thread::spawn(|| drop(fresh())).join().expect("the other thread");
            in_span(fresh(), async {}).await;
        })
        .await;
        let names: Vec<_> = spans.iter().map(|span| span.name.to_string()).collect();
        assert_eq!(names, ["first_hit_elsewhere"]);
    }

    /// What a library spawns from work run `outside_spans` (rmcp's service
    /// loop, which captures `Span::current()`) holds none of the caller's
    /// spans, and a span it makes is a root.
    #[tokio::test]
    async fn test_work_outside_spans_sees_no_current_span() {
        let ((current_is_none, ()), spans) = capture_spans(async {
            in_span(tracing::info_span!("caller"), async {
                let current_is_none = outside_spans(async { Span::current().is_none() }).await;
                outside_spans(async { tracing::info_span!("library").in_scope(|| {}) }).await;
                (current_is_none, ())
            })
            .await
        })
        .await;
        assert!(current_is_none, "the work saw the caller's span");
        let library = spans.iter().find(|span| span.name == "library").expect("the library's span");
        assert_eq!(library.parent_span_id, opentelemetry::trace::SpanId::INVALID, "a root");
    }

    #[tokio::test]
    async fn test_mark_error_sets_the_status_cut_to_the_limit_and_logs_nothing() {
        let long = "x".repeat(STATUS_DESCRIPTION_LIMIT + 100);
        let logged = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
        let ((), spans) = capture_spans_with_console(logged.clone(), async {
            let span = tracing::info_span!(
                "work",
                otel.status_code = tracing::field::Empty,
                otel.status_description = tracing::field::Empty,
            );
            mark_error(&span, &format!("{long}é"));
        })
        .await;
        let [span] = spans.as_slice() else { panic!("one span: {spans:?}") };
        let Status::Error { description } = &span.status else { panic!("{:?}", span.status) };
        assert_eq!(description.chars().count(), STATUS_DESCRIPTION_LIMIT, "{description}");
        assert!(span.events.is_empty(), "no event: {:?}", span.events);
        let logged = String::from_utf8(logged.lock().expect("lock").clone()).expect("utf-8");
        assert!(!logged.contains("ERROR") && !logged.contains("xxx"), "nothing logged: {logged}");
    }
}
