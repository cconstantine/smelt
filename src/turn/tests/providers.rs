use super::*;

#[test]
fn test_is_ollama_thinking_tool_call_corruption_matches_the_known_error_shape() {
    let real_error = r#"Anthropic API error 500 Internal Server Error: {"type":"error","error":{"type":"api_error","message":"error parsing tool call: raw='...' err=invalid character 'T' looking for beginning of value"},"request_id":"req_123"}"#;
    assert!(is_ollama_thinking_tool_call_corruption(real_error));
}

#[test]
fn test_is_ollama_thinking_tool_call_corruption_does_not_match_unrelated_errors() {
    assert!(!is_ollama_thinking_tool_call_corruption(
        "Anthropic API error 529 Overloaded: the server is overloaded"
    ));
    assert!(!is_ollama_thinking_tool_call_corruption(
        "timed out waiting for Anthropic to respond"
    ));
}

/// A bearer provider's turn authenticates with its token as a bearer
/// header, and no `x-api-key` (an Anthropic-compatible gateway such as
/// Hugging Face's wants exactly that).
#[sqlx::test]
async fn test_a_bearer_providers_turn_sends_its_token_as_a_bearer_header(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = seen.clone();
    let body = text_reply_body("Hi!");
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |headers: axum::http::HeaderMap| {
            let recorded = recorded.clone();
            let body = body.clone();
            async move {
                recorded.lock().unwrap_or_else(|e| e.into_inner()).push(headers);
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], body)
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    let provider = db::create_inference_provider(
        &pool,
        "gateway",
        "other",
        &format!("http://{addr}"),
        "bearer",
        "hf-token",
        false,
        None,
        true,
    )
    .await
    .expect("create provider");
    db::set_default_model(&pool, provider.id, "some-model")
        .await
        .expect("set default");

    let messages = run_turn(&pool, conversation.id, hello())
        .await
        .expect("the turn should run on the bearer provider");

    assert_eq!(messages.len(), 2);
    let seen = seen.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let headers = seen.first().expect("the mock should have been called");
    assert_eq!(headers.get("authorization").expect("Authorization header"), "Bearer hf-token");
    assert!(headers.get("x-api-key").is_none(), "no x-api-key for a bearer provider");
}
