use super::*;

/// SME-40 F12: a base URL ending in `/` (as the dev `.env`'s does)
/// produced `//v1/messages`.
#[test]
fn test_messages_url_ignores_a_trailing_slash() {
    let url = |base: &str| {
        Endpoint { base_url: base.to_string(), auth: Auth::ApiKey(String::new()), slots_of: None }.url("/v1/messages")
    };
    assert_eq!(url("https://api.anthropic.com"), "https://api.anthropic.com/v1/messages");
    assert_eq!(url("https://gateway.example/"), "https://gateway.example/v1/messages");
    assert_eq!(url("https://gateway.example/proxy/"), "https://gateway.example/proxy/v1/messages");
}

/// SME-72 review 2: reqwest drops `Authorization` on a cross-host
/// redirect but not `x-api-key`, so a provider that redirects could
/// hand the key to another host. Provider requests don't follow
/// redirects.
#[tokio::test]
async fn test_a_redirect_never_carries_the_key_to_another_host() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let elsewhere = listener.local_addr().expect("addr");
    let reached = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = reached.clone();
    let app = axum::Router::new().fallback(move || {
        let flag = flag.clone();
        async move {
            flag.store(true, std::sync::atomic::Ordering::SeqCst);
            "gotcha"
        }
    });
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let provider = listener.local_addr().expect("addr");
    let app = axum::Router::new().fallback(move || async move {
        axum::response::Redirect::temporary(&format!("http://localhost:{}/steal", elsewhere.port()))
    });
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    let endpoint = Endpoint {
        base_url: format!("http://{provider}"),
        auth: Auth::ApiKey("sk-secret".to_string()),
        slots_of: None,
    };
    let request = CreateMessageRequest {
        model: "m".to_string(),
        max_tokens: 10,
        system: None,
        messages: vec![],
        stream: true,
        tools: vec![],
        thinking: None,
        prompt_caching: false,
        output_config: None,
        chat_template_kwargs: None,
    };
    let result = stream_anthropic_message(&endpoint, &request, |_| {}).await;
    assert!(result.is_err(), "a redirect isn't a reply");
    assert!(!reached.load(std::sync::atomic::Ordering::SeqCst), "the redirect was followed");
}

/// A credential never reaches a log line through `{:?}`.
#[test]
fn test_an_endpoints_debug_hides_its_secret() {
    let endpoint = Endpoint {
        base_url: "https://api.anthropic.com".to_string(),
        auth: Auth::Bearer("sk-very-secret".to_string()),
        slots_of: None,
    };
    let shown = format!("{endpoint:?}");
    assert!(!shown.contains("sk-very-secret"), "{shown}");
    assert!(shown.contains("Bearer"), "{shown}");
}

/// Regression test for SME-8's retrospective's hung
/// live model call: before `RESPONSE_TIMEOUT` existed, a connection
/// that never got a response at all (accepted, then silence) hung
/// `stream_anthropic_message` forever. Accepts the connection but never
/// writes a response, so `send_and_await_response` has nothing to read
/// — a short `response_timeout` (not the real `RESPONSE_TIMEOUT`, which
/// would make this test take 10 real minutes) must still make it
/// return promptly rather than hang.
#[tokio::test]
async fn test_send_and_await_response_times_out_when_upstream_never_responds() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            // Hold the connection open without ever writing a response,
            // for comfortably longer than the test's own timeout below.
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            drop(stream);
        }
    });

    let request = CreateMessageRequest {
        model: "claude-opus-4-8".to_string(),
        max_tokens: 100,
        system: None,
        messages: vec![],
        stream: true,
        tools: vec![],
        thinking: None,
        prompt_caching: false,
        output_config: None,
        chat_template_kwargs: None,
    };

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        send_and_await_response(
            &test_endpoint(addr),
            &request,
            Binding::AsIs,
            std::time::Duration::from_millis(50),
        ),
    )
    .await
    .expect("send_and_await_response itself should not hang past its own timeout");

    match result {
        Err(e) => assert_eq!(e, "timed out waiting for Anthropic to respond"),
        Ok(_) => panic!("expected a timeout error, got a response"),
    }
}

/// Spins up a throwaway mock upstream that captures the request's
/// headers (instead of caring about the body) and calls
/// `send_and_await_response` against it — for asserting exactly which
/// auth header a given credential actually sends.
async fn send_and_capture_headers(auth: Auth) -> (
    Result<reqwest::Response, String>,
    Option<axum::http::HeaderMap>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");

    let captured: std::sync::Arc<std::sync::Mutex<Option<axum::http::HeaderMap>>> =
        std::sync::Arc::new(std::sync::Mutex::new(None));
    let captured_for_route = captured.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |headers: axum::http::HeaderMap| {
            let captured = captured_for_route.clone();
            async move {
                *captured.lock().unwrap_or_else(|e| e.into_inner()) = Some(headers);
                (
                    [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                    "",
                )
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });

    let request = CreateMessageRequest {
        model: "claude-opus-4-8".to_string(),
        max_tokens: 100,
        system: None,
        messages: vec![],
        stream: true,
        tools: vec![],
        thinking: None,
        prompt_caching: false,
        output_config: None,
        chat_template_kwargs: None,
    };

    let endpoint = Endpoint {
        base_url: format!("http://{addr}"),
        auth,
        slots_of: None,
    };
    let result =
        send_and_await_response(&endpoint, &request, Binding::AsIs, std::time::Duration::from_secs(5)).await;
    let headers = captured.lock().unwrap_or_else(|e| e.into_inner()).clone();
    (result, headers)
}

#[tokio::test]
async fn test_send_and_await_response_uses_bearer_auth_for_a_bearer_provider() {
    let (result, headers) =
        send_and_capture_headers(Auth::Bearer("hf-token-value".to_string())).await;
    result.expect("request should succeed");
    let headers = headers.expect("mock upstream should have received the request");
    assert_eq!(
        headers.get("authorization").expect("Authorization header"),
        "Bearer hf-token-value"
    );
    assert!(
        headers.get("x-api-key").is_none(),
        "should not also send x-api-key for a bearer provider"
    );
}

#[tokio::test]
async fn test_send_and_await_response_uses_x_api_key_for_an_api_key_provider() {
    let (result, headers) =
        send_and_capture_headers(Auth::ApiKey("api-key-value".to_string())).await;
    result.expect("request should succeed");
    let headers = headers.expect("mock upstream should have received the request");
    assert_eq!(
        headers.get("x-api-key").expect("x-api-key header"),
        "api-key-value"
    );
    assert!(
        headers.get("authorization").is_none(),
        "should not send Authorization for an api-key provider"
    );
}

/// SME-41 D11: a failed request names the model, not Claude, since
/// the endpoint is often something else (a local llama.cpp here).
#[tokio::test]
async fn test_an_unreachable_model_endpoint_is_reported_as_the_model() {
    let result = send_and_await_response(
        &Endpoint {
            base_url: "http://127.0.0.1:1".to_string(),
            auth: Auth::ApiKey("key".to_string()),
            slots_of: None,
        },
        &CreateMessageRequest {
            model: "local".to_string(),
            max_tokens: 100,
            system: None,
            messages: vec![],
            stream: true,
            tools: vec![],
            thinking: None,
            prompt_caching: false,
            output_config: None,
            chat_template_kwargs: None,
        },
        Binding::AsIs,
        std::time::Duration::from_secs(5),
    )
    .await;
    let message = result.expect_err("nothing listens on port 1");
    assert!(message.starts_with("The model request failed"), "got: {message}");
}
