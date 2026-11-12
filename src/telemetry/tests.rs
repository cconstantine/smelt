// telemetry's tests (moved out of telemetry.rs by SME-392; the module
// path, and so every test's `--exact` name, is unchanged).

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
    // Review 2: as the exporter reads them, only an empty value is unset,
    // so a blank traces variable doesn't hide the general one from smelt
    // while the exporter still takes the blank one.
    assert_eq!(
        otel_endpoint(env_of(&[
            ("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "  "),
            ("OTEL_EXPORTER_OTLP_ENDPOINT", "http://tempo:4318"),
        ])),
        Some("  ".to_string())
    );
    assert_eq!(
        otel_endpoint(env_of(&[("OTEL_EXPORTER_OTLP_ENDPOINT", "http://tempo:4318")])),
        Some("http://tempo:4318".to_string())
    );
    // The exporter takes the traces-only variable too (review 1).
    assert_eq!(
        otel_endpoint(env_of(&[("OTEL_EXPORTER_OTLP_TRACES_ENDPOINT", "http://tempo:4318/v1/traces")])),
        Some("http://tempo:4318/v1/traces".to_string())
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

/// A dependency's own INFO spans (dioxus-signals instruments every
/// signal it makes) aren't exported, but its warnings inside smelt's
/// spans are.
#[tokio::test]
async fn test_only_smelts_info_spans_and_everyones_warnings_are_exported() {
    let ((), spans) = capture_spans(async {
        tracing::info_span!(target: "dioxus_signals::signal", "new_maybe_sync").in_scope(|| {});
        in_span(tracing::info_span!("work"), async {
            tracing::info!(target: "kube_client", "a dependency's chatter");
            tracing::warn!(target: "kube_client", "a dependency's warning");
        })
        .await;
    })
    .await;
    let names: Vec<_> = spans.iter().map(|span| span.name.to_string()).collect();
    assert_eq!(names, ["work"]);
    let events: Vec<_> = spans[0].events.iter().map(|event| event.name.to_string()).collect();
    assert_eq!(events, ["a dependency's warning"]);
}

/// Review 1: an error's text quotes URLs (reqwest's `for url (…)`, smelt's
/// own `failed to load {url}`), and a query or userinfo can hold a key.
#[test]
fn test_urls_in_text_lose_their_userinfo_query_and_fragment() {
    let scrubbed = scrub_urls(
        "error sending request for url (http://me:pw@127.0.0.1:1/mcp?apiKey=SECRET-KEY#frag); \
         then https://example.com/a/b?token=SECRET2 and wss://h/x?y=SECRET3.",
    );
    assert!(!scrubbed.contains("SECRET"), "{scrubbed}");
    assert!(!scrubbed.contains("me:pw"), "{scrubbed}");
    assert!(scrubbed.contains("(http://127.0.0.1:1/mcp?…)"), "{scrubbed}");
    assert!(scrubbed.contains("https://example.com/a/b?…"), "{scrubbed}");
    assert!(scrubbed.ends_with("wss://h/x?…."), "{scrubbed}");
    assert_eq!(scrub_urls("no url here: 404 at /a?b"), "no url here: 404 at /a?b");
}

#[tokio::test]
async fn test_mark_error_scrubs_urls() {
    let ((), spans) = capture_spans(async {
        let span = tracing::info_span!(
            "work",
            otel.status_code = tracing::field::Empty,
            otel.status_description = tracing::field::Empty,
        );
        mark_error(&span, "failed for url (http://h/mcp?apiKey=SECRET-KEY)");
    })
    .await;
    let [span] = spans.as_slice() else { panic!("one span: {spans:?}") };
    assert!(!format!("{:?}", span.status).contains("SECRET"), "{:?}", span.status);
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

/// SME-392: rmcp prints reqwest's error text into its own WARN/ERROR
/// lines, and that text quotes the URL with its path, so a key sitting in a
/// path segment (`…/mcp/s/<key>/mcp`) must be scrubbed along with the query.
/// Key-shaped means what `mcp::call::redact_secrets` has treated as key-
/// shaped since SME-386 M1 — 16 characters or more with letters and digits —
/// and that heuristic now lives here, one rule for both sinks and the
/// config URL.
#[test]
fn test_scrub_urls_cuts_key_shaped_path_segments_too() {
    // The one URL of this ticket's tests, both key positions in it.
    let scrubbed = scrub_urls(
        "error sending request for url \
         (http://127.0.0.1:9/mcp/s/TEST-KEY-392-PATH-0001/mcp?apiKey=TEST-KEY-392)",
    );
    assert!(!scrubbed.contains("TEST-KEY-392"), "{scrubbed}");
    assert!(scrubbed.contains("http://127.0.0.1:9/mcp/s/<redacted>/mcp?…"), "{scrubbed}");
    // Ordinary path words stay (the query goes whole, as today), and a
    // digits-only segment stays: the heuristic needs letters too.
    assert_eq!(scrub_urls("https://mcp.exa.ai/mcp?tools=web_search_exa"), "https://mcp.exa.ai/mcp?…");
    assert_eq!(
        scrub_urls("failed for https://example.com/mcp/s/01234567890123456/v2/mcp"),
        "failed for https://example.com/mcp/s/01234567890123456/v2/mcp"
    );
    // Idempotent: the exporter scrubs text `mark_error` and
    // `redact_secrets` already scrubbed — scrubbing it again changes nothing.
    let once = scrub_urls(
        "for url (http://me:pw@h.example:8443/mcp/s/TEST-KEY-392-PATH-0001/mcp?apiKey=TEST-KEY-392#frag) done",
    );
    assert!(!once.contains("TEST-KEY-392") && !once.contains("me:pw"), "{once}");
    assert_eq!(scrub_urls(&once), once);
}

/// The heuristic `redact_secrets` (query values) and `scrub_urls` (path
/// segments) share from SME-392: 16 characters or more, letters and digits.
#[test]
fn test_looks_like_a_key_needs_length_letters_and_digits() {
    assert!(looks_like_a_key("TEST-KEY-392-PATH-0001"));
    assert!(looks_like_a_key("abcdefghij123456")); // exactly 16
    assert!(!looks_like_a_key("abcdefghi12345")); // 15
    assert!(!looks_like_a_key("01234567890123456")); // digits only
    assert!(!looks_like_a_key("web_search_exa_tool_name")); // letters only
}

/// The two rmcp lines of SME-392 (their exact texts, string-built with the
/// markers), a split-write URL, a plain line that must come out
/// byte-identical, and a final partial line: through the console writer as
/// the `fmt` layer drives it.
#[test]
fn test_the_console_writer_scrubs_urls_from_every_line() {
    use std::io::Write;
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone)]
    struct Buffer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap_or_else(|e| e.into_inner()).extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let url = "http://127.0.0.1:9/mcp/s/TEST-KEY-392-PATH-0001/mcp?apiKey=TEST-KEY-392";
    let line_740 = format!(
        "2026-11-11T00:00:00.000000Z ERROR rmcp::transport::streamable_http_client: \
         fail to get common stream: Client error: HTTP status client error (404 Not Found) for url ({url})"
    );
    let line_135 = format!(
        "2026-11-11T00:00:00.000000Z ERROR rmcp::transport::worker: worker quit with fatal: \
         Transport channel closed, when Client(reqwest::Error {{ kind: Request, url: \"{url}\", \
         source: Connection refused }})"
    );
    let plain = "2026-11-11T00:00:00.000000Z  INFO smelt: MCP server unreachable this turn\n";

    let out = std::sync::Arc::new(std::sync::Mutex::new(Vec::<u8>::new()));
    {
        let buffer = Buffer(out.clone());
        let make = Scrubbed::new(move || buffer.clone());
        let mut w = make.make_writer();
        w.write_all(line_740.as_bytes()).expect("write 740");
        w.write_all(b"\n").expect("newline");
        w.write_all(line_135.as_bytes()).expect("write 135");
        w.write_all(b"\n").expect("newline");
        w.write_all(plain.as_bytes()).expect("write plain");
        // A line split across writes is still one line.
        w.write_all(b"split url http://h/").expect("write part one");
        w.write_all(b"mcp/s/TEST-KEY-392-PATH-0001/mcp?apiKey=TEST-KEY-392 end\n").expect("write part two");
        // And a tail that never sees its newline is flushed on drop.
        w.write_all(b"cut off for url (https://h/x?apiKey=TEST-KEY-392-PATH-0001)").expect("write tail");
    }
    let logged = String::from_utf8(out.lock().unwrap_or_else(|e| e.into_inner()).clone()).expect("utf-8");
    assert!(!logged.contains("TEST-KEY-392"), "{logged}");
    assert!(
        logged.contains("fail to get common stream") && logged.contains("worker quit with fatal"),
        "the words survive: {logged}"
    );
    assert!(
        logged.contains("127.0.0.1:9") && logged.contains("404 Not Found") && logged.contains("Connection refused"),
        "the host and the reason survive: {logged}"
    );
    assert!(logged.contains("for url (http://127.0.0.1:9/mcp/s/<redacted>/mcp?…"), "the shape survives: {logged}");
    assert!(logged.contains(plain), "the plain line is byte-identical: {logged}");
    assert!(logged.contains("split url http://h/mcp/s/<redacted>/mcp?… end"), "the split line: {logged}");
    assert!(logged.ends_with("cut off for url (https://h/x?…)"), "the tail flushed, newline-free: {logged}");
}

/// Class-level end to end (SME-392): the mechanism `kube_client`'s warning
/// demonstrates — a dependency's WARN under an smelt INFO span is exported
/// as that span's event — must scrub what it exports, because `capture()`
/// wraps its test exporter the way `init()` wraps the production one.
#[tokio::test]
async fn test_a_dependency_warning_under_a_smelt_span_exports_no_url_key() {
    let ((), spans) = capture_spans(async {
        in_span(tracing::info_span!("work"), async {
            tracing::warn!(
                target: "rmcp::transport::streamable_http_client",
                "fail to get common stream: Client error: error sending request for url \
                 (https://mcp.example.com/mcp/s/TEST-KEY-392-PATH-0001/mcp?apiKey=TEST-KEY-392): connection refused"
            );
        })
        .await;
    })
    .await;
    let exported = format!("{spans:?}");
    assert!(
        spans.iter().flat_map(|span| span.events.iter()).count() > 0,
        "the warning is exported: {exported}"
    );
    assert!(!exported.contains("TEST-KEY-392"), "the export carries the key: {exported}");
    assert!(
        exported.contains("connection refused") && exported.contains("mcp.example.com"),
        "the useful text survives: {exported}"
    );
}

/// The export sink's decorator against a hand-built span: every string form
/// a dependency's text rides — an event's name (its message), string
/// attributes, a `Value::Array` of strings, and a failed status's
/// description — leaves scrubbed; the span name and the non-string
/// attributes don't move.
#[tokio::test]
async fn test_the_export_scrubs_event_names_attributes_and_status() {
    use opentelemetry::trace::{
        Event as OtelEvent, SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceState, TraceId,
    };
    use opentelemetry::{Array, InstrumentationScope, KeyValue, Value};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData, SpanEvents, SpanExporter, SpanLinks};

    let url = "http://127.0.0.1:9/mcp/s/TEST-KEY-392-PATH-0001/mcp?apiKey=TEST-KEY-392";
    let line_740 = format!(
        "fail to get common stream: Client error: HTTP status client error (404 Not Found) for url ({url})"
    );
    let line_135 = format!(
        "worker quit with fatal: Transport channel closed, when Client(reqwest::Error {{ kind: Request, url: \"{url}\" }})"
    );
    let now = std::time::SystemTime::now();
    let mut events = SpanEvents::default();
    events.events.push(OtelEvent::new(line_740.clone(), now, vec![KeyValue::new("message", line_135.clone())], 0));
    let span = SpanData {
        span_context: SpanContext::new(
            TraceId::INVALID,
            SpanId::INVALID,
            TraceFlags::default(),
            true,
            TraceState::default(),
        ),
        parent_span_id: SpanId::INVALID,
        parent_span_is_remote: false,
        span_kind: SpanKind::Client,
        name: "mcp connect".into(),
        start_time: now,
        end_time: now,
        attributes: vec![
            KeyValue::new("message", line_135.clone()),
            KeyValue::new("attempt", 2_i64),
            KeyValue::new("urls", Value::Array(Array::String(vec![url.to_string().into()]))),
        ],
        dropped_attributes_count: 0,
        events,
        links: SpanLinks::default(),
        status: Status::error(format!("failed for url ({url}): connection refused")),
        instrumentation_scope: InstrumentationScope::builder("smelt-test").build(),
    };

    let sink = InMemorySpanExporter::default();
    SpanExporter::export(&UrlScrubbed::new(sink.clone()), vec![span]).await.expect("export");
    let spans = sink.get_finished_spans().expect("the sink's spans");
    let exported = format!("{spans:?}");
    assert!(!exported.contains("TEST-KEY-392"), "{exported}");
    assert!(
        exported.contains("fail to get common stream") && exported.contains("worker quit with fatal"),
        "the words survive: {exported}"
    );
    assert!(
        exported.contains("127.0.0.1:9") && exported.contains("connection refused"),
        "the host and reason survive: {exported}"
    );
    let [span] = spans.as_slice() else { panic!("one span: {spans:?}") };
    assert_eq!(span.name, "mcp connect", "the span name is smelt's own and stays");
    assert!(
        span.attributes.iter().any(|kv| kv.key.as_str() == "attempt" && kv.value == Value::from(2_i64)),
        "non-strings untouched: {:?}",
        span.attributes
    );
    assert!(matches!(span.status, Status::Error { .. }), "{:?}", span.status);
}

/// SME-483: a span *link*'s attribute values can carry the same text an
/// event's can — a linked span's error text, a retry URL — and
/// `scrub_span_data` must scrub them like everything else, leaving the
/// non-string attributes, the link's span context and its dropped count
/// alone. (No live producer emits links today — tracing-opentelemetry 0.34
/// has no `follows_from` support — so this is future-proofing, per the
/// ticket.)
#[tokio::test]
async fn test_the_export_scrubs_span_link_attributes() {
    use opentelemetry::trace::{
        Link, SpanContext, SpanId, SpanKind, Status, TraceFlags, TraceState, TraceId,
    };
    use opentelemetry::{Array, InstrumentationScope, KeyValue, Value};
    use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData, SpanEvents, SpanExporter, SpanLinks};

    let url = "http://127.0.0.1:9/mcp/s/TEST-KEY-392-PATH-0001/mcp?apiKey=TEST-KEY-392";
    let now = std::time::SystemTime::now();
    let context = SpanContext::new(
        TraceId::INVALID,
        SpanId::INVALID,
        TraceFlags::default(),
        true,
        TraceState::default(),
    );
    // `SpanLinks` is `#[non_exhaustive]`, so build the default and push:
    // what `add_link` does.
    let mut links = SpanLinks::default();
    links.links.push(Link::new(
        context.clone(),
        vec![
            KeyValue::new("message", format!("retried after url ({url})")),
            KeyValue::new("urls", Value::Array(Array::String(vec![url.to_string().into()]))),
            KeyValue::new("attempt", 2_i64),
        ],
        0,
    ));
    let span = SpanData {
        span_context: context,
        parent_span_id: SpanId::INVALID,
        parent_span_is_remote: false,
        span_kind: SpanKind::Client,
        name: "link carrier".into(),
        start_time: now,
        end_time: now,
        attributes: vec![],
        dropped_attributes_count: 0,
        events: SpanEvents::default(),
        links,
        status: Status::Unset,
        instrumentation_scope: InstrumentationScope::builder("smelt-test").build(),
    };

    let sink = InMemorySpanExporter::default();
    SpanExporter::export(&UrlScrubbed::new(sink.clone()), vec![span]).await.expect("export");
    let spans = sink.get_finished_spans().expect("the sink's spans");
    let exported = format!("{spans:?}");
    assert!(!exported.contains("TEST-KEY-392"), "the export carries the key: {exported}");
    let [span] = spans.as_slice() else { panic!("one span: {spans:?}") };
    let [link] = span.links.links.as_slice() else { panic!("one link: {:?}", span.links) };
    let attributes = format!("{:?}", link.attributes);
    assert!(
        attributes.contains("http://127.0.0.1:9/mcp/s/<redacted>/mcp?…"),
        "the shape survives: {attributes}"
    );
    assert!(
        link.attributes.iter().any(|kv| kv.key.as_str() == "attempt" && kv.value == Value::from(2_i64)),
        "non-strings untouched: {:?}",
        link.attributes
    );
    assert_eq!(link.dropped_attributes_count, 0, "{link:?}");
}
/// SME-483: a panic never enters the tracing pipeline, so neither SME-392
/// sink can scrub it — the panic hook must produce the default hook's
/// report itself, scrubbed. Same shape and destination as the default
/// hook: a lead newline, `thread '<name>' panicked at <file>:<line>:<col>:`,
/// the message, then the captured backtrace or the note that points one
/// out; every string through [`scrub_urls`]. `tail` is that last part, as
/// the hook renders it (a backtrace's text, the note, or empty).
#[test]
fn test_the_panic_report_scrubs_urls_and_keeps_the_default_shape() {
    let url = "http://127.0.0.1:9/mcp/s/TEST-KEY-392-PATH-0001/mcp?apiKey=TEST-KEY-392";
    let note = "note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace\n";

    // A `String` payload — what `panic!("…{url}…")` and every `expect`
    // make — quoting a configured URL: the key goes, the sentence and the
    // shape stay.
    let mut out = Vec::new();
    write_scrubbed_panic_report(
        &mut out,
        "main",
        Some("src/mcp.rs:1:1"),
        &format!("failed for url ({url})"),
        note,
    )
    .expect("write to a Vec");
    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        format!(
            "\nthread 'main' panicked at src/mcp.rs:1:1:\n\
             failed for url (http://127.0.0.1:9/mcp/s/<redacted>/mcp?…)\n{note}"
        )
    );

    // A `&str` payload (a bare `panic!("literal")`) with a captured
    // backtrace where the note would be.
    let mut out = Vec::new();
    write_scrubbed_panic_report(
        &mut out,
        "worker",
        Some("src/main.rs:120:32"),
        &"transport died",
        "stack backtrace:\n   0: smelt::mcp::call\n",
    )
    .expect("write");
    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        "\nthread 'worker' panicked at src/main.rs:120:32:\n\
         transport died\nstack backtrace:\n   0: smelt::mcp::call\n"
    );

    // A payload that is neither renders as the default hook's
    // `Box<dyn Any>`, and no location as its "unknown location" — and no
    // user Display runs, so nothing here can panic (a panic inside the
    // hook aborts the process: std calls that "panic in a panic hook").
    let mut out = Vec::new();
    write_scrubbed_panic_report(&mut out, "<unnamed>", None, &42_i64, "").expect("write");
    assert_eq!(
        String::from_utf8(out).expect("utf-8"),
        "\nthread '<unnamed>' panicked at unknown location:\nBox<dyn Any>\n"
    );
}

/// The installed hook end to end (SME-483): a real panic, real payload,
/// real stderr — in a *child* copy of this test binary, so the panicking
/// never touches the test process. The child's stderr must carry the
/// default hook's shape with the key gone; on the unfixed hook the raw
/// URL rides through and the second assert fails.
#[test]
fn test_the_installed_hook_writes_a_scrubbed_report_to_stderr() {
    let output = std::process::Command::new(std::env::current_exe().expect("the test binary"))
        .args(["--exact", "telemetry::tests::the_panic_hook_child_panics", "--nocapture"])
        .env("SMELT_PANIC_HOOK_CHILD", "1")
        .output()
        .expect("spawn the child test");
    assert!(!output.status.success(), "the child must die of its panic: {output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("TEST-KEY-392"), "the stderr carries the key: {stderr}");
    assert!(
        stderr.contains("panicked at src/telemetry/tests.rs:")
            && stderr.contains("child panic for url (http://127.0.0.1:9/mcp/s/<redacted>/mcp?…"),
        "the default hook's shape with the URLs cut: {stderr}"
    );
}

/// The child of [`test_the_installed_hook_writes_a_scrubbed_report_to_stderr`]:
/// installs the hook and panics when the parent asked it to, so the hook's
/// real output (stderr, an unwinding panic process) is what gets checked.
/// Run directly it is inert — without the parent's env var this test just
/// passes, and no filter but the parent's exact-name one selects it.
#[test]
fn the_panic_hook_child_panics() {
    if std::env::var("SMELT_PANIC_HOOK_CHILD").is_err() {
        return;
    }
    install_scrubbed_panic_hook();
    panic!(
        "child panic for url (http://127.0.0.1:9/mcp/s/TEST-KEY-392-PATH-0001/mcp?apiKey=TEST-KEY-392)"
    );
}
