use super::*;

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_run_turn_for_a_missing_conversation_says_so(pool: PgPool) {
    let _guard = lock_turn_tests();
    start_mock_upstream(&pool, vec![String::new()]).await;
    let error = run_turn(&pool, 987_654_321, hello())
        .await
        .expect_err("a turn for a conversation that doesn't exist should fail")
        .to_string();
    assert!(error.contains("conversation not found"), "got: {error}");
    assert!(!error.contains("foreign key"), "a raw database error leaked: {error}");
}

/// With no model to run on the turn can't run, but what the user
/// typed is still theirs: it shouldn't vanish on the next reload.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_run_turn_without_a_model_still_saves_the_message(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let error = run_turn(&pool, conversation.id, hello())
        .await
        .expect_err("no provider should fail the turn");
    assert_eq!(error.message(), crate::providers::NO_MODEL_CONFIGURED);
    let saved = db::list_messages(&pool, conversation.id)
        .await
        .expect("list messages");
    assert_eq!(saved.len(), 1, "the user's message should have been saved");
    assert_eq!(saved[0].role, "user");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_run_turn_persists_user_and_assistant_messages_for_text_only_reply(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    let body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi!"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_mock_upstream(&pool, vec![body]).await;

    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text {
            text: "hello".to_string(),
        }],
    };

    let messages = run_turn(&pool, conversation.id, new_message)
        .await
        .expect("run_turn should succeed");

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].role, "user");
    assert_eq!(
        messages[0].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::Text {
            text: "hello".to_string()
        }]
    );
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(
        messages[1].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::Text {
            text: "Hi!".to_string()
        }]
    );
}

/// Regression test for conversation 43's real "500 error parsing tool
/// call" incident: with thinking on (the default) and pointed at a
/// mock upstream that fails the *first* request with Ollama's exact
/// error shape, `run_turn` should retry without thinking and still
/// complete — not surface the 500 to the caller.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_run_turn_retries_without_thinking_after_ollama_tool_call_corruption(
    pool: PgPool,
) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    let success_body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"pong"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_mock_upstream_failing_n_times(&pool, 1, Some(success_body)).await;

    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text {
            text: "ping".to_string(),
        }],
    };

    let messages = run_turn(&pool, conversation.id, new_message)
        .await
        .expect("run_turn should recover from the failed first attempt and succeed");

    assert_eq!(messages.len(), 2);
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(
        messages[1].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::Text {
            text: "pong".to_string()
        }],
        "the retried (thinking-free) attempt's reply should be what actually got persisted"
    );
}

/// Dropping `thinking` doesn't help every case (a local model can flub
/// a tool call's JSON on its own — see `is_ollama_thinking_tool_call_corruption`'s
/// doc comment) — this fails *twice*, past the thinking-drop, and
/// relies on `TOOL_CALL_PARSE_RETRIES` allowing one further plain
/// regeneration to still recover.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_run_turn_recovers_after_two_ollama_tool_call_failures(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    let success_body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"third time's the charm"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_mock_upstream_failing_n_times(&pool, 2, Some(success_body)).await;

    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text {
            text: "ping".to_string(),
        }],
    };

    let messages = run_turn(&pool, conversation.id, new_message)
        .await
        .expect(
            "run_turn should recover after exhausting the thinking-drop and one plain retry",
        );

    assert_eq!(
        messages[1].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::Text {
            text: "third time's the charm".to_string()
        }]
    );
}

/// Once `TOOL_CALL_PARSE_RETRIES` is exhausted, `run_turn` gives up and
/// surfaces the error rather than retrying forever against a call the
/// model is reliably bad at.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_run_turn_gives_up_after_exhausting_tool_call_parse_retries(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    // `fail_count` is irrelevant when `success_body` is `None` — every
    // request fails regardless (see the helper's doc comment).
    start_mock_upstream_failing_n_times(&pool, 0, None).await;

    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text {
            text: "ping".to_string(),
        }],
    };

    let err = run_turn(&pool, conversation.id, new_message)
        .await
        .expect_err("should give up and surface the error once retries are exhausted");
    assert!(
        err.to_string().contains("error parsing tool call"),
        "got {err}"
    );
}
