use super::*;

/// The 400 an account enforcing preserved thinking returns when a
/// replayed thinking block's prefix changed (SME-93). Its wording is
/// the Preserved thinking page's; no enforced account was available
/// to capture a real one.
const BINDING_BODY: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation. Remove the block, or set `thinking.block_binding.prefix_mismatch_behavior` to \"drop_block\". That setting requires the `thinking-binding-controls-2026-08-01` value in the `anthropic-beta` header."}}"#;

/// What a server without the controls beta says to `block_binding`.
const EXTRA_INPUTS_BODY: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"thinking.adaptive.block_binding: Extra inputs are not permitted"}}"#;

const BINDING_BETA: &str = "thinking-binding-controls-2026-08-01";

/// One request a recording upstream received.
#[derive(Debug, Clone)]
struct Seen {
    beta: Option<String>,
    user_agent: Option<String>,
    body: Value,
}

impl Seen {
    fn drop_block(&self) -> Option<&str> {
        self.body.pointer("/thinking/block_binding/prefix_mismatch_behavior")?.as_str()
    }

    fn thinking_blocks(&self) -> usize {
        self.body["messages"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|m| m["content"].as_array().into_iter().flatten())
            .filter(|b| b["type"] == "thinking")
            .count()
    }

    fn has_binding_beta(&self) -> bool {
        self.beta.as_deref().is_some_and(|b| b.split(',').any(|v| v.trim() == BINDING_BETA))
    }
}

/// An upstream serving `responses` in order (the last one repeats),
/// recording each request; returns its endpoint and the record.
async fn recording_upstream(
    responses: Vec<(u16, &'static str)>,
) -> (Endpoint, std::sync::Arc<std::sync::Mutex<Vec<Seen>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<Seen>::new()));
    let record = seen.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |headers: axum::http::HeaderMap, body: String| {
            let responses = responses.clone();
            let record = record.clone();
            async move {
                let beta = headers
                    .get("anthropic-beta")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let user_agent = headers
                    .get(axum::http::header::USER_AGENT)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                let body = serde_json::from_str(&body).unwrap_or(Value::Null);
                let i = {
                    let mut record = record.lock().expect("record lock");
                    record.push(Seen { beta, user_agent, body });
                    record.len() - 1
                };
                let (status, body) = responses[i.min(responses.len() - 1)];
                let content_type = if status == 200 { "text/event-stream" } else { "application/json" };
                (
                    axum::http::StatusCode::from_u16(status).expect("valid status"),
                    [(axum::http::header::CONTENT_TYPE, content_type)],
                    body,
                )
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    // A credential of its own: `RECOVERY` outlives the test, and a later
    // test can be handed the same port.
    static UPSTREAMS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let n = UPSTREAMS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let endpoint = Endpoint {
        base_url: format!("http://{addr}"),
        auth: Auth::ApiKey(format!("recording-upstream-{n}")),
        slots_of: None,
    };
    (endpoint, seen)
}

/// A request replaying an earlier turn's thinking block.
fn request_replaying_thinking(thinking: Option<super::super::super::types::ThinkingConfig>) -> CreateMessageRequest {
    CreateMessageRequest {
        model: "claude-opus-5-5".to_string(),
        max_tokens: 100,
        system: Some("prompt".to_string()),
        messages: vec![
            super::super::super::types::AnthropicMessage {
                role: "user".to_string(),
                content: vec![ContentBlock::Text { text: "hi".to_string() }],
            },
            super::super::super::types::AnthropicMessage {
                role: "assistant".to_string(),
                content: vec![
                    ContentBlock::Thinking { thinking: String::new(), signature: "sig".to_string() },
                    ContentBlock::Text { text: "hello".to_string() },
                ],
            },
            super::super::super::types::AnthropicMessage {
                role: "user".to_string(),
                content: vec![ContentBlock::Text { text: "again".to_string() }],
            },
        ],
        stream: true,
        tools: vec![],
        thinking,
        prompt_caching: false,
        output_config: None,
        chat_template_kwargs: None,
    }
}

fn requests_seen(record: &std::sync::Arc<std::sync::Mutex<Vec<Seen>>>) -> Vec<Seen> {
    record.lock().expect("record lock").clone()
}

#[test]
fn test_dropped_thinking_counts_the_drops_message_start_reports() {
    let start = serde_json::json!({"type": "message_start", "message": {"id": "msg_1", "input_transformations": [
        {"type": "thinking_dropped", "path": "messages.1.content.0", "reason": "prefix_binding_mismatch"},
        {"type": "thinking_dropped", "path": "messages.3.content.0", "reason": "model_binding_mismatch"},
        {"type": "thinking_mismatch_allowed", "path": "messages.5.content.0", "reason": "prefix_binding_mismatch"},
        {"type": "some_future_entry"}
    ]}});
    assert_eq!(dropped_thinking(&start), 2);
    let empty = serde_json::json!({"type": "message_start", "message": {"input_transformations": []}});
    assert_eq!(dropped_thinking(&empty), 0);
    let absent = serde_json::json!({"type": "message_start", "message": {"id": "msg_1"}});
    assert_eq!(dropped_thinking(&absent), 0);
    let other = serde_json::json!({"type": "content_block_stop", "index": 0});
    assert_eq!(dropped_thinking(&other), 0);
}

#[tokio::test]
async fn test_a_binding_mismatch_is_retried_with_drop_block() {
    let (endpoint, record) = recording_upstream(vec![(400, BINDING_BODY), (200, OK_BODY)]).await;
    let request = request_replaying_thinking(Some(super::super::super::types::ThinkingConfig::Adaptive));
    let result = stream_anthropic_message(&endpoint, &request, |_| {}).await;
    assert!(result.is_ok(), "should recover: {result:?}");
    let seen = requests_seen(&record);
    assert_eq!(seen.len(), 2);
    assert!(!seen[0].has_binding_beta() && seen[0].drop_block().is_none(), "first try is as before: {:?}", seen[0]);
    assert!(seen[1].has_binding_beta(), "the retry carries the beta: {:?}", seen[1].beta);
    assert_eq!(seen[1].drop_block(), Some("drop_block"));
    assert_eq!(seen[1].thinking_blocks(), 1, "drop_block replays the history verbatim");
}

#[tokio::test]
async fn test_a_provider_that_needed_drop_block_gets_it_from_the_start() {
    let (endpoint, record) = recording_upstream(vec![(400, BINDING_BODY), (200, OK_BODY)]).await;
    let request = request_replaying_thinking(Some(super::super::super::types::ThinkingConfig::Adaptive));
    stream_anthropic_message(&endpoint, &request, |_| {}).await.expect("first turn recovers");
    stream_anthropic_message(&endpoint, &request, |_| {}).await.expect("second turn");
    let seen = requests_seen(&record);
    assert_eq!(seen.len(), 3, "the second turn needs no retry");
    assert!(seen[2].has_binding_beta());
    assert_eq!(seen[2].drop_block(), Some("drop_block"));

    let (other, other_record) = recording_upstream(vec![(200, OK_BODY)]).await;
    stream_anthropic_message(&other, &request, |_| {}).await.expect("another provider");
    let other_seen = requests_seen(&other_record);
    assert!(!other_seen[0].has_binding_beta() && other_seen[0].drop_block().is_none(), "another provider is untouched");
}

/// smelt says who it is: Kimi Code's terms forbid a tool hiding or
/// faking its identity (SME-106).
#[tokio::test]
async fn test_a_model_request_names_smelt_and_its_version() {
    let (endpoint, record) = recording_upstream(vec![(200, OK_BODY)]).await;
    stream_anthropic_message(&endpoint, &request_replaying_thinking(None), |_| {})
        .await
        .expect("streams");
    assert_eq!(
        requests_seen(&record)[0].user_agent.as_deref(),
        Some(concat!("smelt/", env!("CARGO_PKG_VERSION")))
    );
}

#[tokio::test]
async fn test_a_caching_request_carries_both_markers_even_when_retried_stripped() {
    let (endpoint, record) = recording_upstream(vec![(400, BINDING_BODY), (200, OK_BODY)]).await;
    let mut request = request_replaying_thinking(None);
    request.prompt_caching = true;
    stream_anthropic_message(&endpoint, &request, |_| {}).await.expect("should recover");
    let seen = requests_seen(&record);
    assert_eq!(seen.len(), 2);
    for (i, sent) in seen.iter().enumerate() {
        assert_eq!(sent.body["cache_control"], serde_json::json!({"type": "ephemeral"}), "request {i}");
        assert_eq!(
            sent.body["system"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral"}),
            "request {i}"
        );
    }
}

/// What a strict server says to the top-level `cache_control`.
const CACHE_CONTROL_REFUSED_BODY: &str = r#"{"type":"error","error":{"type":"invalid_request_error","message":"cache_control: Extra inputs are not permitted"}}"#;

#[tokio::test]
async fn test_a_refused_cache_marker_names_the_provider_setting() {
    let (endpoint, _record) = recording_upstream(vec![(400, CACHE_CONTROL_REFUSED_BODY)]).await;
    let mut request = request_replaying_thinking(None);
    request.prompt_caching = true;
    let error = stream_anthropic_message(&endpoint, &request, |_| {}).await.expect_err("refused");
    assert!(error.contains("cache_control: Extra inputs are not permitted"), "keeps the server's words: {error}");
    assert!(error.contains("Prompt caching"), "names the setting to turn off: {error}");
}

#[tokio::test]
async fn test_a_400_without_caching_gets_no_caching_hint() {
    let (endpoint, _record) = recording_upstream(vec![(400, CACHE_CONTROL_REFUSED_BODY)]).await;
    let request = request_replaying_thinking(None);
    let error = stream_anthropic_message(&endpoint, &request, |_| {}).await.expect_err("refused");
    assert!(!error.contains("Prompt caching"), "{error}");
}

/// Synthetic refusals of `output_config.effort`, one naming each field: a
/// model without effort, and one without the level asked for.
const EFFORT_REFUSED_BODIES: [&str; 2] = [
    r#"{"type":"error","error":{"type":"invalid_request_error","message":"This model does not support the effort parameter."}}"#,
    r#"{"type":"error","error":{"type":"invalid_request_error","message":"output_config: Extra inputs are not permitted"}}"#,
];

#[tokio::test]
async fn test_a_refused_effort_names_the_models_effort_setting() {
    for body in EFFORT_REFUSED_BODIES {
        let (endpoint, _record) = recording_upstream(vec![(400, body)]).await;
        let mut request = request_replaying_thinking(None);
        request.output_config = Some(super::super::super::types::OutputConfig { effort: super::super::super::types::Effort::Xhigh });
        let error = stream_anthropic_message(&endpoint, &request, |_| {}).await.expect_err("refused");
        assert!(error.contains(&provider_error_message(body)), "keeps the server's words: {error}");
        assert!(error.contains("\u{201c}Effort\u{201d}"), "names the setting to change: {error}");
    }
}

#[tokio::test]
async fn test_a_400_without_effort_gets_no_effort_hint() {
    for body in EFFORT_REFUSED_BODIES {
        let (endpoint, _record) = recording_upstream(vec![(400, body)]).await;
        let request = request_replaying_thinking(None);
        let error = stream_anthropic_message(&endpoint, &request, |_| {}).await.expect_err("refused");
        assert!(!error.contains("Effort"), "{error}");
    }
}

#[tokio::test]
async fn test_a_binding_mismatch_without_thinking_is_retried_stripped() {
    let (endpoint, record) = recording_upstream(vec![(400, BINDING_BODY), (200, OK_BODY)]).await;
    let request = request_replaying_thinking(None);
    let result = stream_anthropic_message(&endpoint, &request, |_| {}).await;
    assert!(result.is_ok(), "should recover: {result:?}");
    let seen = requests_seen(&record);
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].thinking_blocks(), 0, "the retry drops every thinking block");
    assert!(!seen[1].has_binding_beta() && seen[1].drop_block().is_none());
    assert_eq!(seen[1].body["messages"][1]["content"][0]["text"], "hello", "the rest of the turn stays");
}

#[tokio::test]
async fn test_a_strip_for_a_thinking_off_request_isnt_kept_for_thinking_on_ones() {
    let (endpoint, record) = recording_upstream(vec![
        (400, BINDING_BODY),
        (200, OK_BODY),
        (400, BINDING_BODY),
        (200, OK_BODY),
    ])
    .await;
    stream_anthropic_message(&endpoint, &request_replaying_thinking(None), |_| {})
        .await
        .expect("thinking off recovers by stripping");
    let thinking_on = request_replaying_thinking(Some(super::super::super::types::ThinkingConfig::Adaptive));
    stream_anthropic_message(&endpoint, &thinking_on, |_| {}).await.expect("thinking on recovers");
    let seen = requests_seen(&record);
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[2].thinking_blocks(), 1, "a thinking-on request goes out as built first");
    assert!(!seen[2].has_binding_beta());
    assert_eq!(seen[3].drop_block(), Some("drop_block"), "and is retried with drop_block, not stripped");
}

#[tokio::test]
async fn test_a_remembered_drop_block_that_is_refused_tries_as_built_first() {
    let (endpoint, record) = recording_upstream(vec![
        (400, BINDING_BODY),
        (200, OK_BODY),
        (400, EXTRA_INPUTS_BODY),
        (200, OK_BODY),
    ])
    .await;
    let request = request_replaying_thinking(Some(super::super::super::types::ThinkingConfig::Adaptive));
    stream_anthropic_message(&endpoint, &request, |_| {}).await.expect("learns drop_block");
    stream_anthropic_message(&endpoint, &request, |_| {})
        .await
        .expect("a route that refuses the field but not the history works as built");
    let seen = requests_seen(&record);
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[2].drop_block(), Some("drop_block"), "the remembered form goes first");
    assert_eq!(seen[3].thinking_blocks(), 1, "then the request as built, not stripped");
    assert!(!seen[3].has_binding_beta() && seen[3].drop_block().is_none());
}

#[tokio::test]
async fn test_a_refused_beta_header_counts_as_refusing_drop_block() {
    let beta_refused = r#"{"type":"error","error":{"type":"invalid_request_error","message":"Unexpected value(s) `thinking-binding-controls-2026-08-01` for the `anthropic-beta` header. Please consult our documentation at docs.anthropic.com or try again without the header."}}"#;
    let (endpoint, record) = recording_upstream(vec![
        (400, BINDING_BODY),
        (200, OK_BODY),
        (400, beta_refused),
        (200, OK_BODY),
        (200, OK_BODY),
    ])
    .await;
    let request = request_replaying_thinking(Some(super::super::super::types::ThinkingConfig::Adaptive));
    stream_anthropic_message(&endpoint, &request, |_| {}).await.expect("learns drop_block");
    stream_anthropic_message(&endpoint, &request, |_| {})
        .await
        .expect("a route that refuses the beta takes the request as built");
    stream_anthropic_message(&endpoint, &request, |_| {}).await.expect("next turn");
    let seen = requests_seen(&record);
    assert_eq!(seen.len(), 5);
    assert_eq!(seen[3].thinking_blocks(), 1, "retried as built");
    assert!(!seen[3].has_binding_beta());
    assert!(!seen[4].has_binding_beta(), "and drop_block is no longer remembered");
}

#[tokio::test]
async fn test_a_refused_drop_block_falls_back_to_stripping() {
    let (endpoint, record) =
        recording_upstream(vec![(400, BINDING_BODY), (400, EXTRA_INPUTS_BODY), (200, OK_BODY)]).await;
    let request = request_replaying_thinking(Some(super::super::super::types::ThinkingConfig::Adaptive));
    let result = stream_anthropic_message(&endpoint, &request, |_| {}).await;
    assert!(result.is_ok(), "should recover by stripping: {result:?}");
    let seen = requests_seen(&record);
    assert_eq!(seen[2].thinking_blocks(), 0);
    assert!(!seen[2].has_binding_beta() && seen[2].drop_block().is_none());
    stream_anthropic_message(&endpoint, &request, |_| {}).await.expect("next turn");
    let seen = requests_seen(&record);
    assert_eq!(seen.len(), 4);
    assert_eq!(seen[3].thinking_blocks(), 1, "the next turn isn't stripped ahead of a refusal");
}

#[tokio::test]
async fn test_an_unrelated_400_under_drop_block_is_not_retried() {
    let too_long = r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 1200000 tokens > 1000000 maximum"}}"#;
    let (endpoint, record) =
        recording_upstream(vec![(400, BINDING_BODY), (200, OK_BODY), (400, too_long), (200, OK_BODY)]).await;
    let request = request_replaying_thinking(Some(super::super::super::types::ThinkingConfig::Adaptive));
    stream_anthropic_message(&endpoint, &request, |_| {}).await.expect("learns drop_block");
    let error = stream_anthropic_message(&endpoint, &request, |_| {})
        .await
        .expect_err("an unrelated 400 is returned");
    assert!(error.contains("prompt is too long"), "the real cause is shown: {error}");
    assert_eq!(requests_seen(&record).len(), 3, "not retried stripped");
    stream_anthropic_message(&endpoint, &request, |_| {}).await.expect("next turn");
    let seen = requests_seen(&record);
    assert_eq!(seen[3].drop_block(), Some("drop_block"), "the provider still gets drop_block");
}

#[tokio::test]
async fn test_a_binding_mismatch_on_the_last_retry_is_returned() {
    let (endpoint, record) = recording_upstream(vec![(400, BINDING_BODY)]).await;
    let request = request_replaying_thinking(None);
    let result = stream_anthropic_message(&endpoint, &request, |_| {}).await;
    let error = result.expect_err("a stripped history that still fails can't be fixed by retrying");
    assert!(error.contains("bound to a different conversation"), "{error}");
    assert_eq!(requests_seen(&record).len(), 2);
}

#[tokio::test]
async fn test_a_persistent_provider_error_gives_up_readably() {
    let (result, requests) = run_against_responses(vec![(503, PAUSED_PROVIDER_BODY)]).await;
    assert_eq!(requests, 3, "three attempts, then give up");
    let error = result.expect_err("a provider that stays down should fail");
    assert!(error.contains("Provider 'featherless-ai' is currently failing"), "got {error}");
    assert!(!error.contains("{"), "raw JSON leaked into the message: {error}");
}

#[test]
fn test_provider_error_message_unwraps_nested_json() {
    let message = provider_error_message(PAUSED_PROVIDER_BODY);
    assert_eq!(
        message,
        "Provider 'featherless-ai' is currently failing for model 'Qwen/Qwen3.8-27B' and has \
             been paused by the circuit breaker. Retry later or use another provider."
    );
}

#[test]
fn test_provider_error_message_falls_back_to_the_raw_body() {
    assert_eq!(provider_error_message("upstream exploded"), "upstream exploded");
}
