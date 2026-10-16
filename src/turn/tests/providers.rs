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
        None,
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

/// SME-162, the whole chain (the provider's column, its endpoint, the
/// model call): two conversations on a provider allowed one request at
/// a time take turns at the server, and both turns finish.
#[sqlx::test]
async fn test_two_conversations_on_a_one_request_provider_take_turns(pool: PgPool) {
    let _guard = lock_turn_tests();
    let first = db::create_conversation(&pool).await.expect("create conversation");
    let second = db::create_conversation(&pool).await.expect("create conversation");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let arrived = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let body = text_reply_body("Hi!");
    let app = {
        let (arrived, active, peak, release) = (arrived.clone(), active.clone(), peak.clone(), release.clone());
        axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(move || {
                let (arrived, active, peak, release, body) =
                    (arrived.clone(), active.clone(), peak.clone(), release.clone(), body.clone());
                async move {
                    arrived.fetch_add(1, Ordering::SeqCst);
                    peak.fetch_max(active.fetch_add(1, Ordering::SeqCst) + 1, Ordering::SeqCst);
                    release.acquire().await.expect("the gate stays open").forget();
                    active.fetch_sub(1, Ordering::SeqCst);
                    ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], body)
                }
            }),
        )
    };
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    // The slots are process-wide by provider id, and every test database
    // numbers its providers from 1: an id no other test uses.
    sqlx::query("ALTER TABLE inference_providers ALTER COLUMN id RESTART WITH 9162000101")
        .execute(&pool)
        .await
        .expect("renumber");
    let provider = db::create_inference_provider(&pool, "one slot", "llama_cpp", &format!("http://{addr}"), "bearer", "k", false, None, true, Some(1))
        .await
        .expect("create provider");
    assert_eq!(provider.id, 9_162_000_101);
    for conversation in [first.id, second.id] {
        db::set_conversation_model(&pool, conversation, provider.id, "m").await.expect("pick");
    }

    let gate = async {
        let waited = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while arrived.load(Ordering::SeqCst) == 0 {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await;
        assert!(waited.is_ok(), "no turn reached the server");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let while_one_runs = arrived.load(Ordering::SeqCst);
        release.add_permits(2);
        while_one_runs
    };
    let (one, two, while_one_runs) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        async { tokio::join!(run_turn(&pool, first.id, hello()), run_turn(&pool, second.id, hello()), gate) },
    )
    .await
    .expect("both turns end");

    assert_eq!(while_one_runs, 1, "the second turn waited for the slot");
    assert_eq!(peak.load(Ordering::SeqCst), 1, "never two requests at once");
    assert_eq!(one.expect("the first turn finishes").len(), 2);
    assert_eq!(two.expect("the second turn finishes").len(), 2);
}
