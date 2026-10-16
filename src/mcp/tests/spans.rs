use super::*;

/// SME-137: an MCP tool call is an `mcp call` span under its caller's,
/// failed when the server refuses the call.
#[tokio::test]
async fn test_an_mcp_call_is_a_span_under_its_caller() {
    let server_id = -137_001;
    register_test_connection(server_id, false).await;
    let config = test_config(server_id, "test-server");
    let ((ok, refused), spans) = crate::telemetry::capture_spans(async {
        crate::telemetry::in_span(tracing::info_span!("caller"), async {
            let ok = call_tool(&test_pool(), &config, "echo", serde_json::json!({"x": "SECRET-ARG"})).await;
            let refused = call_tool(&test_pool(), &config, "no_such_tool", serde_json::json!({})).await;
            (ok, refused)
        })
        .await
    })
    .await;
    REGISTRY.lock().await.remove(&server_id);
    assert!(ok.is_ok(), "{ok:?}");
    assert!(refused.is_err(), "{refused:?}");
    let caller = spans.iter().find(|span| span.name == "caller").expect("the caller's span");
    let calls: Vec<_> = spans.iter().filter(|span| span.name == "mcp call").collect();
    let [ok_span, refused_span] = calls.as_slice() else { panic!("two mcp calls: {spans:?}") };
    for span in [ok_span, refused_span] {
        assert_eq!(span.parent_span_id, caller.span_context.span_id());
        assert!(span.attributes.iter().any(|kv| kv.key.as_str() == "smelt.mcp.server" && kv.value == "test-server".into()));
    }
    assert_eq!(ok_span.status, opentelemetry::trace::Status::Unset);
    assert!(matches!(refused_span.status, opentelemetry::trace::Status::Error { .. }), "{:?}", refused_span.status);
    assert!(!format!("{spans:?}").contains("SECRET-ARG"), "no arguments in a span");
}

/// The connects a turn's tool list starts run in the background, but
/// as the turn's children, not roots of their own.
#[sqlx::test]
async fn test_a_tool_lists_connect_is_a_child_of_its_caller(pool: sqlx::PgPool) {
    let config = create_static_server(&pool, &test_config(0, "unused").url).await;
    let (_, spans) = crate::telemetry::capture_spans(async {
        crate::telemetry::in_span(tracing::info_span!("caller"), async {
            tool_definitions_for(&pool, std::slice::from_ref(&config)).await
        })
        .await
    })
    .await;
    let caller = spans.iter().find(|span| span.name == "caller").expect("the caller's span");
    let connect = spans.iter().find(|span| span.name == "mcp connect").expect("a connect span");
    assert_eq!(connect.parent_span_id, caller.span_context.span_id());
}

/// Review 1: a failed connect's error quotes the server's URL, and a
/// query can hold its key (`?apiKey=`). Neither a span's status nor the
/// tool list's warning exports it.
#[sqlx::test]
async fn test_a_failed_connect_exports_no_query_from_the_servers_url(pool: sqlx::PgPool) {
    let config = create_static_server(&pool, "http://127.0.0.1:1/mcp?apiKey=SECRET-KEY").await;
    let (_, spans) = crate::telemetry::capture_spans(async {
        crate::telemetry::in_span(tracing::info_span!("caller"), async {
            let _ = connection_check(&pool, &config).await;
            evict(config.id).await;
            lock_failures().remove(&config.id);
            tool_definitions_for(&pool, std::slice::from_ref(&config)).await
        })
        .await
    })
    .await;
    assert!(spans.iter().any(|span| span.name == "mcp connect"), "{spans:?}");
    let exported = format!("{spans:?}");
    assert!(!exported.contains("SECRET-KEY"), "{exported}");
}

/// Review 1: a tool's own error output, and input it was refused for,
/// stay out of the span; smelt's own failures keep their message.
#[tokio::test]
async fn test_a_tools_reported_error_exports_none_of_its_output() {
    let server_id = -137_005;
    register_test_connection(server_id, false).await;
    let config = test_config(server_id, "test-server");
    let ((failed, refused), spans) = crate::telemetry::capture_spans(async {
        let failed = call_tool(&test_pool(), &config, "fail", serde_json::json!({})).await;
        let refused = call_tool(&test_pool(), &config, "echo", serde_json::json!(["SECRET-INPUT"])).await;
        (failed, refused)
    })
    .await;
    REGISTRY.lock().await.remove(&server_id);
    assert!(failed.is_err_and(|e| e.contains("SECRET-OUTPUT")), "the model still gets the output");
    assert!(refused.is_err());
    let calls: Vec<_> = spans.iter().filter(|span| span.name == "mcp call").collect();
    assert_eq!(calls.len(), 2, "{spans:?}");
    assert!(calls.iter().all(|span| matches!(span.status, opentelemetry::trace::Status::Error { .. })));
    let exported = format!("{spans:?}");
    assert!(!exported.contains("SECRET"), "{exported}");
}

/// Review 2: a server's non-JSON-RPC error reply to `tools/call` comes
/// back as a transport error quoting its body, which can echo the
/// call's input; the span names only the kind of failure.
#[test]
fn test_a_transport_failures_span_description_quotes_no_reply_body() {
    let error = rmcp::service::ServiceError::TransportSend(rmcp::transport::DynamicTransportError::from_parts(
        "streamable-http",
        std::any::TypeId::of::<()>(),
        Box::new(std::io::Error::other(
            "unexpected server response: HTTP 500 Internal Server Error: crashed on SECRET-ECHOED-INPUT",
        )),
    ));
    let description = transport_failure_description("search", "probe", &error);
    assert!(!description.contains("SECRET"), "{description}");
    assert!(description.contains("search") && description.contains("probe"), "{description}");
    assert!(description.contains("transport"), "{description}");
}

/// Review 2, in the real caller's shape: an MCP server over real HTTP
/// that answers `tools/call` with an HTTP 500 whose plain-text body
/// echoes the call's input. rmcp quotes that body in a transport
/// error; the model gets it, the exported spans don't.
#[sqlx::test]
async fn test_a_tools_call_answered_with_a_non_json_rpc_500_exports_none_of_the_body(pool: sqlx::PgPool) {
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    async fn handler(axum::Json(request): axum::Json<serde_json::Value>) -> axum::response::Response {
        let Some(id) = request.get("id").cloned() else {
            return StatusCode::ACCEPTED.into_response();
        };
        let result = match request["method"].as_str() {
            Some("initialize") => serde_json::json!({
                "protocolVersion": request["params"]["protocolVersion"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "mock", "version": "0"},
            }),
            Some("tools/list") => serde_json::json!({"tools": [
                {"name": "search", "description": "Searches", "inputSchema": {"type": "object"}}
            ]}),
            Some("tools/call") => {
                let echoed = request["params"]["arguments"]["q"].as_str().unwrap_or_default().to_string();
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("crashed on {echoed}")).into_response();
            }
            _ => serde_json::json!({}),
        };
        axum::Json(serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local addr");
    let app = axum::Router::new().route("/mcp", axum::routing::post(handler));
    tokio::spawn(async move { axum::serve(listener, app).await });

    let config = create_static_server(&pool, &format!("http://{addr}/mcp")).await;
    let (result, spans) = crate::telemetry::capture_spans(async {
        call_tool(&pool, &config, "search", serde_json::json!({"q": "SECRET-ECHOED-INPUT"})).await
    })
    .await;
    evict(config.id).await;
    lock_failures().remove(&config.id);
    let error = result.expect_err("a 500 fails the call");
    assert!(error.contains("SECRET-ECHOED-INPUT"), "the model gets the reply: {error}");
    let call = spans.iter().find(|span| span.name == "mcp call").expect("an mcp call span");
    assert!(matches!(call.status, opentelemetry::trace::Status::Error { .. }), "{:?}", call.status);
    let exported = format!("{spans:?}");
    assert!(!exported.contains("SECRET"), "{exported}");
}

#[sqlx::test]
async fn test_a_failed_mcp_connect_is_a_failed_span(pool: sqlx::PgPool) {
    let config = create_static_server(&pool, &test_config(0, "unused").url).await;
    let (result, spans) = crate::telemetry::capture_spans(connection_check(&pool, &config)).await;
    assert!(result.is_err());
    let connects: Vec<_> = spans.iter().filter(|span| span.name == "mcp connect").collect();
    assert!(!connects.is_empty(), "{spans:?}");
    assert!(
        connects.iter().all(|span| matches!(span.status, opentelemetry::trace::Status::Error { .. })),
        "{connects:?}"
    );
    let check = spans.iter().find(|span| span.name == "mcp connection_check").expect("the check's span");
    assert!(matches!(check.status, opentelemetry::trace::Status::Error { .. }), "{:?}", check.status);
}
