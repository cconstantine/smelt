use super::*;

/// SME-392: rmcp 3.1.2 logs transport failures at WARN/ERROR with reqwest's
/// error text, and that text quotes the full request URL — a key in the
/// query (`?apiKey=…`) or a key-shaped path segment (`…/mcp/s/<key>/mcp`)
/// rides to the console and, once any such event rides an smelt span, the
/// OTel export. Both markers below are obviously fake and both live in one
/// URL, so the single `!contains(TEST_KEY)` assertion per sink is
/// load-bearing for both fix parts: the query cut can't clear the path and
/// the path rule can't clear the query.
const TEST_KEY: &str = "TEST-KEY-392";
const PATH_KEY: &str = "TEST-KEY-392-PATH-0001";

fn marker_url(addr: &str) -> String {
    format!("http://{addr}/mcp/s/{PATH_KEY}/mcp?apiKey={TEST_KEY}")
}

fn console_text(console: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>) -> String {
    String::from_utf8_lossy(&console.lock().unwrap_or_else(|e| e.into_inner())).into_owned()
}

/// Polls `console` until `marker` reached it or 5 s passed. rmcp logs
/// transport failures from tasks of its own (`outside_spans`,
/// `mcp/mod.rs:528-531`), so the capture must stay open until rmcp speaks —
/// that is what makes these tests' redness deterministic instead of a race
/// against the worker task.
async fn drain_until(console: &std::sync::Arc<std::sync::Mutex<Vec<u8>>>, marker: &str) {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while !console_text(console).contains(marker) && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

/// The line that fires on every "server can't be reached" — rmcp's
/// `transport/worker.rs:135` (citation: external), at ERROR, so smelt's
/// default `RUST_LOG=error` prints it: rmcp formats reqwest's Debug into
/// it, url field included. The
/// line and its host must survive the console scrubbing — the fix hides the
/// key, not the diagnosis — and the key must reach neither sink.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_unreachable_servers_key_never_reaches_the_console_or_the_export(pool: sqlx::PgPool) {
    // A port nothing listens on: bound, then released (redaction.rs's shape).
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    };
    let config = create_static_server(&pool, &marker_url(&format!("127.0.0.1:{port}"))).await;
    let console = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (_, spans) = crate::telemetry::capture_spans_with_console(console.clone(), async {
        crate::telemetry::in_span(tracing::info_span!("caller"), async {
            let _check = connection_check(&pool, &config).await;
            drain_until(&console, "worker quit with fatal").await;
        })
        .await;
    })
    .await;
    evict(config.id).await;
    lock_failures().remove(&config.id);
    let logged = console_text(&console);
    // The reproduction fired, and rmcp's diagnostics survive the fix: the
    // line is there, with the host and port it failed to reach.
    assert!(logged.contains("worker quit with fatal"), "the reproduction didn't fire: {logged}");
    assert!(
        logged.contains("127.0.0.1") && logged.contains(&port.to_string()),
        "the useful text survives: {logged}"
    );
    // The key reaches neither sink.
    assert!(!logged.contains(TEST_KEY), "the console carries the key: {logged}");
    assert!(!format!("{spans:?}").contains(TEST_KEY), "the export carries the key: {spans:?}");
}

/// The site the ticket names: rmcp's common-stream GET
/// (`transport/streamable_http_client.rs:740`, citation: external) prints
/// reqwest's Display
/// (`for url (…)` and all) when the server answered `initialize` with an
/// `Mcp-Session-Id` response header — without it rmcp never opens the
/// common stream — and the GET then answers non-2xx. 404, not 405: 405
/// takes the silent `ServerDoesNotSupportSse` path.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_key_in_a_broken_common_streams_url_never_reaches_the_console_or_the_export(pool: sqlx::PgPool) {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    async fn post(axum::Json(request): axum::Json<serde_json::Value>) -> axum::response::Response {
        let Some(id) = request.get("id").cloned() else {
            return StatusCode::ACCEPTED.into_response(); // a notification
        };
        let initialize = request["method"] == "initialize";
        let result = match request["method"].as_str() {
            Some("initialize") => serde_json::json!({
                "protocolVersion": request["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock-392", "version": "0"},
            }),
            Some("tools/list") => serde_json::json!({"tools": [
                {"name": "echo", "description": "Echoes", "inputSchema": {"type": "object"}}
            ]}),
            _ => serde_json::json!({}),
        };
        let mut response =
            axum::Json(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response();
        if initialize {
            // The header that makes rmcp open the common stream at all.
            response.headers_mut().insert("Mcp-Session-Id", "sess-392-test".parse().expect("header value"));
        }
        response
    }
    async fn get() -> StatusCode {
        StatusCode::NOT_FOUND
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    // The marker URL carries a path, so the mock answers it, not `/mcp`.
    let app = axum::Router::new()
        .route(&format!("/mcp/s/{PATH_KEY}/mcp"), axum::routing::post(post).get(get));
    tokio::spawn(async move { axum::serve(listener, app).await });

    let config = create_static_server(&pool, &marker_url(&addr.to_string())).await;
    let console = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (_, spans) = crate::telemetry::capture_spans_with_console(console.clone(), async {
        crate::telemetry::in_span(tracing::info_span!("caller"), async {
            let _check = connection_check(&pool, &config).await;
            drain_until(&console, "fail to get common stream").await;
        })
        .await;
    })
    .await;
    evict(config.id).await;
    lock_failures().remove(&config.id);
    let logged = console_text(&console);
    // Same shape as the other test: the 740 line is there, host and port
    // too; the key is nowhere.
    assert!(logged.contains("fail to get common stream"), "the reproduction didn't fire: {logged}");
    assert!(
        logged.contains("127.0.0.1") && logged.contains(&addr.port().to_string()),
        "the useful text survives: {logged}"
    );
    assert!(!logged.contains(TEST_KEY), "the console carries the key: {logged}");
    assert!(!format!("{spans:?}").contains(TEST_KEY), "the export carries the key: {spans:?}");
}
