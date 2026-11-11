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
