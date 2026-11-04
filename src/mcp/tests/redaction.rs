//! SME-359 T9: no credential reaches a call's error text. A script's
//! call (`call_tool_raw`) hands its error to a process in the pod, and
//! the model's (`call_tool_classed`) to the model, which can write it
//! there; so the server's URL query, its header values and its OAuth
//! tokens are redacted on both. One marker per source, each in the shape
//! a real server gives it.

use super::*;

use axum::http::{HeaderMap, StatusCode, Uri};
use axum::response::IntoResponse;

/// An MCP endpoint that answers every POST with a 500 whose body quotes
/// the request's URL and every header, as a careless server's error page
/// might. A GET (OAuth metadata discovery) is a 404, as `oauth_http`'s
/// provider answers it, so an OAuth client goes on to send its token.
async fn spawn_echo_server() -> String {
    async fn echo(method: axum::http::Method, uri: Uri, headers: HeaderMap) -> axum::response::Response {
        if method == axum::http::Method::GET {
            return StatusCode::NOT_FOUND.into_response();
        }
        let mut body = format!("bad request to {uri}\n");
        for (name, value) in &headers {
            body.push_str(&format!("{name}: {}\n", value.to_str().unwrap_or("?")));
        }
        (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    let app = axum::Router::new().fallback(echo);
    tokio::spawn(async move { axum::serve(listener, app).await });
    format!("http://{addr}/mcp")
}

/// A `static_headers` server row, with an id of its own.
async fn static_server(pool: &sqlx::PgPool, url: &str, headers: &[(&str, &str)]) -> McpServerConfig {
    let id = next_server_id(pool).await;
    let headers: HashMap<String, String> = headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    crate::db::create_mcp_server_config(pool, &format!("redact-{id}"), url, &headers, "static_headers", None, None)
        .await
        .expect("create mcp server config")
}

/// Both callers' error text for one call, twice each: the second call
/// can answer from the recent-failure record instead of connecting.
async fn error_texts(pool: &sqlx::PgPool, config: &McpServerConfig) -> Vec<String> {
    let mut texts = Vec::new();
    for _ in 0..2 {
        texts.push(call_tool(pool, config, "echo", serde_json::json!({})).await.expect_err("the call fails"));
        texts.push(
            call_tool_raw(pool, config, "echo", serde_json::json!({}), |_, _| Ok(()))
                .await
                .expect_err("the call fails"),
        );
    }
    texts
}

fn assert_no_marker(texts: &[String], marker: &str) {
    for text in texts {
        assert!(!text.contains(marker), "the error text quotes {marker:?}: {text}");
        assert!(!text.is_empty(), "an empty error text says nothing");
    }
}

/// reqwest's connect error quotes the whole URL, query and all.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_connect_error_doesnt_quote_the_urls_query(pool: sqlx::PgPool) {
    // A port nothing listens on: bound, then released.
    let port = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        listener.local_addr().expect("addr").port()
    };
    let marker = "QUERYKEY-359-closed";
    let config = static_server(&pool, &format!("http://127.0.0.1:{port}/mcp?exaApiKey={marker}"), &[]).await;
    let texts = error_texts(&pool, &config).await;
    assert_no_marker(&texts, marker);
}

/// A transport error can quote the server's reply, which here echoes the
/// request's headers and URL.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_quoting_the_request_doesnt_leak_its_header_or_query(pool: sqlx::PgPool) {
    let url = spawn_echo_server().await;
    let (header_marker, query_marker) = ("HEADERKEY-359-echo", "QUERYKEY-359-echo");
    let config = static_server(&pool, &format!("{url}?key={query_marker}"), &[("x-api-key", header_marker)]).await;
    let texts = error_texts(&pool, &config).await;
    assert_no_marker(&texts, header_marker);
    assert_no_marker(&texts, query_marker);
}

/// An OAuth server's bearer token, echoed back in its error page.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_quoting_the_request_doesnt_leak_the_oauth_token(pool: sqlx::PgPool) {
    let url = spawn_echo_server().await;
    let id = next_server_id(&pool).await;
    let config = crate::db::create_mcp_server_config(&pool, &format!("redact-oauth-{id}"), &url, &HashMap::new(), "oauth", Some("client-359"), None)
        .await
        .expect("create mcp server config");
    let (access, refresh) = ("ACCESSTOKEN-359-echo", "REFRESHTOKEN-359-echo");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).expect("clock").as_secs();
    let credentials = serde_json::json!({
        "client_id": "client-359",
        "token_response": {"access_token": access, "token_type": "bearer", "expires_in": 3600, "refresh_token": refresh},
        "granted_scopes": [],
        "token_received_at": now,
    });
    crate::db::set_mcp_server_oauth_credentials(&pool, config.id, Some(credentials)).await.expect("save grant");
    let config = crate::db::get_mcp_server_config(&pool, config.id).await.expect("get").expect("exists");
    let texts = error_texts(&pool, &config).await;
    assert_no_marker(&texts, access);
    assert_no_marker(&texts, refresh);
}

/// Found by SME-359's hands-on check: Exa's built-in URL is
/// `…/mcp?tools=web_search_exa`, and redacting each query value on its own
/// turned that tool's name into `<redacted>` in every error naming it. A
/// value that doesn't look like a key (long, with letters and digits)
/// stays; the whole query, and a value that does look like one, go.
#[test]
fn test_a_query_value_that_isnt_a_key_stays_in_the_text() {
    let mut config = test_config(-359_009, "exa");
    config.url = "https://mcp.exa.ai/mcp?tools=web_search_exa&exaApiKey=3f2a9c1e-77b4-4d0e-9a31-5c8e2b6f0d47".to_string();
    let text = "MCP tool call to \"web_search_exa\" failed: Unknown tool (availableTools: [\"web_search_exa\"]) \
                for https://mcp.exa.ai/mcp?tools=web_search_exa&exaApiKey=3f2a9c1e-77b4-4d0e-9a31-5c8e2b6f0d47; key 3f2a9c1e-77b4-4d0e-9a31-5c8e2b6f0d47";
    let redacted = redact_secrets(&config, text);
    assert_eq!(redacted.matches("web_search_exa").count(), 2, "the tool's name stays: {redacted}");
    assert!(!redacted.contains("3f2a9c1e"), "the key goes, alone and in the URL: {redacted}");
}
