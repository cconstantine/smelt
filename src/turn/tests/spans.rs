use super::*;

// SME-137: a turn's spans.

fn spans_named<'a>(
    spans: &'a [opentelemetry_sdk::trace::SpanData],
    name: &str,
) -> Vec<&'a opentelemetry_sdk::trace::SpanData> {
    spans.iter().filter(|span| span.name == name).collect()
}

fn span_attribute<'a>(span: &'a opentelemetry_sdk::trace::SpanData, key: &str) -> Option<&'a opentelemetry::Value> {
    let values: Vec<_> = span.attributes.iter().filter(|kv| kv.key.as_str() == key).map(|kv| &kv.value).collect();
    assert!(values.len() <= 1, "{key} recorded more than once: {values:?}");
    values.first().copied()
}

fn todoread_call_body() -> String {
    sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"todoread","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        ),
        ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ])
}

/// A turn is one `turn` span, with its model calls and tool calls under it,
/// all in one trace.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turns_model_and_tool_calls_are_children_of_its_span(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    start_mock_upstream(&pool, vec![todoread_call_body(), text_reply_body("done")]).await;
    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, conversation.id, hello())).await;
    result.expect("the turn runs");

    let turns = spans_named(&spans, "turn");
    let [turn] = turns.as_slice() else { panic!("one turn span: {spans:?}") };
    assert_eq!(span_attribute(turn, "conversation_id"), Some(&conversation.id.into()));
    assert!(span_attribute(turn, "model").is_some(), "{turn:?}");
    assert!(span_attribute(turn, "provider_id").is_some(), "{turn:?}");
    assert_eq!(span_attribute(turn, "turn.steps"), Some(&2_i64.into()));
    assert_eq!(turn.status, opentelemetry::trace::Status::Unset);

    let children: Vec<&str> = spans
        .iter()
        .filter(|span| span.parent_span_id == turn.span_context.span_id())
        .map(|span| span.name.as_ref())
        .collect();
    let calls: Vec<&str> = children.iter().copied().filter(|name| *name != "tool todoread").collect();
    assert_eq!(calls.len(), 2, "two model calls under the turn: {children:?}");
    assert!(calls.iter().all(|name| name.starts_with("chat ")), "{children:?}");
    assert!(children.contains(&"tool todoread"), "{children:?}");
    let trace = turn.span_context.trace_id();
    assert!(spans.iter().all(|span| span.span_context.trace_id() == trace), "one trace");
    assert!(!format!("{spans:?}").contains("hello"), "no message text in a span");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turn_whose_model_call_fails_is_a_failed_span(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    start_mock_upstream_failing_n_times(&pool, 0, None).await;
    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, conversation.id, hello())).await;
    result.expect_err("the model call fails");
    let turns = spans_named(&spans, "turn");
    let [turn] = turns.as_slice() else { panic!("one turn span: {spans:?}") };
    let opentelemetry::trace::Status::Error { description } = &turn.status else { panic!("{:?}", turn.status) };
    assert!(description.contains("error parsing tool call"), "{description}");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stopped_turn_says_so_without_failing_its_span(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    start_hanging_mock_upstream(&pool).await;
    let id = conversation.id;
    let ((), spans) = crate::telemetry::capture_spans(async {
        let turn = tokio::spawn({
            let pool = pool.clone();
            async move { run_turn(&pool, id, hello()).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        stop_turn_now(id);
        let result = tokio::time::timeout(std::time::Duration::from_secs(2), turn)
            .await
            .expect("the turn ends")
            .expect("join");
        assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");
    })
    .await;
    let turns = spans_named(&spans, "turn");
    let [turn] = turns.as_slice() else { panic!("one turn span: {spans:?}") };
    assert_eq!(span_attribute(turn, "turn.stopped"), Some(&true.into()));
    assert_eq!(turn.status, opentelemetry::trace::Status::Unset);
}

/// The user's turn runs in the background, as a child of the request that
/// sent it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_sent_turn_is_a_child_of_the_request_that_sent_it(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    start_mock_upstream(&pool, vec![text_reply_body("hi")]).await;
    let mut rx = events::subscribe(conversation.id);
    let ((), spans) = crate::telemetry::capture_spans(async {
        crate::telemetry::in_span(
            tracing::info_span!("request"),
            start_turn(pool.clone(), conversation.id, "hi".to_string()),
        )
        .await
        .expect("send");
        loop {
            if next_turn_state(&mut rx).await.expect("a turn state") == false {
                break;
            }
        }
        // The turn's span closes as its task ends, just after it says so.
        tokio::task::yield_now().await;
    })
    .await;
    let requests = spans_named(&spans, "request");
    let [request] = requests.as_slice() else { panic!("one request span: {spans:?}") };
    let turns = spans_named(&spans, "turn");
    let [turn] = turns.as_slice() else { panic!("one turn span: {spans:?}") };
    assert_eq!(turn.parent_span_id, request.span_context.span_id());
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_compaction_is_a_span_with_its_model_call_under_it(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    db::create_message(&pool, conversation.id, "user", &[anthropic::ContentBlock::Text { text: "earlier".to_string() }])
        .await
        .expect("seed");
    db::create_message(&pool, conversation.id, "assistant", &[anthropic::ContentBlock::Text { text: "reply".to_string() }])
        .await
        .expect("seed");
    start_mock_upstream(&pool, vec![text_reply_body("Summary: earlier.")]).await;
    let turn_model = crate::providers::resolve_turn_model(&pool, conversation.id)
        .await
        .expect("the mock is the default");
    let (result, spans) =
        crate::telemetry::capture_spans(compact_conversation(&pool, conversation.id, &turn_model)).await;
    result.expect("compaction");
    let compactions = spans_named(&spans, "compaction");
    let [compaction] = compactions.as_slice() else { panic!("one compaction span: {spans:?}") };
    assert_eq!(span_attribute(compaction, "conversation_id"), Some(&conversation.id.into()));
    let chat = spans.iter().find(|span| span.name.starts_with("chat ")).expect("its model call");
    assert_eq!(chat.parent_span_id, compaction.span_context.span_id());
}
