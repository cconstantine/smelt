use super::*;

/// SME-112: on llama.cpp, whose blocks overlap, the commentary before a
/// tool call is saved with it (it used to show while streaming and then
/// vanish), every tab gets it, and the next request replays it.
#[sqlx::test]
async fn test_commentary_before_a_tool_call_on_llama_cpp_is_saved(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_309)
        .await
        .expect("create conversation");
    let llama_cpp = include_str!("../../anthropic/fixtures/llama_cpp_messages_stream.sse");
    let requests =
        start_recording_mock_upstream(&pool, vec![llama_cpp.to_string(), text_reply_body("Nothing to do.")]).await;
    let mut rx = events::subscribe(conversation.id);

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("run_turn should succeed");

    let commentary = anthropic::ContentBlock::Text {
        text: "Let me check your todo list.".to_string(),
    };
    let saved = db::list_messages(&pool, conversation.id).await.expect("list messages");
    let first_reply = &saved[1];
    assert_eq!(first_reply.role, "assistant");
    let blocks = first_reply.blocks().expect("blocks");
    assert!(
        matches!(&blocks[..], [anthropic::ContentBlock::Thinking { .. }, text, anthropic::ContentBlock::ToolUse { .. }] if *text == commentary),
        "{blocks:?}"
    );
    let published = drain_events(&mut rx).await.into_iter().any(|event| match event {
        events::ConversationEvent::MessagesAppended { messages } => {
            messages.iter().any(|m| m.role == "assistant" && m.blocks().expect("blocks").contains(&commentary))
        }
        _ => false,
    });
    assert!(published, "every tab gets the commentary");
    let second_request = requests.lock().expect("the request log")[1].clone();
    let replayed = &second_request["messages"][1];
    assert_eq!(replayed["role"], "assistant");
    assert_eq!(replayed["content"][1], serde_json::json!({"type": "text", "text": "Let me check your todo list."}));
}

#[sqlx::test]
async fn test_run_turn_executes_tool_and_persists_full_round_trip(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    let tool_use_body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"todoread","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    let final_body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Sum is 5"}}"#,
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
    start_mock_upstream(&pool, vec![tool_use_body, final_body]).await;

    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text {
            text: "what's on the todo list?".to_string(),
        }],
    };

    let messages = run_turn(&pool, conversation.id, new_message)
        .await
        .expect("run_turn should succeed");

    assert_eq!(
        messages.len(),
        4,
        "expected user, tool_use, tool_result, final assistant"
    );
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].role, "assistant");
    assert_eq!(
        messages[1].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::ToolUse {
            id: "toolu_01".to_string(),
            name: "todoread".to_string(),
            input: serde_json::json!({}),
        }]
    );
    assert_eq!(messages[2].role, "user");
    assert_eq!(
        messages[2].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::ToolResult {
            tool_use_id: "toolu_01".to_string(),
            content: "[]".to_string(),
            is_error: None,
        }]
    );
    assert_eq!(messages[3].role, "assistant");
    assert_eq!(
        messages[3].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::Text {
            text: "Sum is 5".to_string()
        }]
    );
}

#[sqlx::test]
async fn test_describe_live_state_lists_real_pods_and_terminals(pool: PgPool) {
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    // Deliberately real rows, not hand-built
    // strings — this is the exact data `compact_conversation`'s
    // summarization prompt hands the model, so the format it actually
    // produces from real fixtures is what matters, not an assumption
    // about it.
    let pod = db::create_sandbox_pod(&pool, conversation.id)
        .await
        .expect("create pod");
    let terminal = db::create_sandbox_terminal(&pool, pod.id)
        .await
        .expect("create terminal");
    let description = describe_live_state(&pool, conversation.id).await;

    assert!(
        description.contains(&format!("pod_id {}", pod.id)),
        "expected the real pod id in: {description}"
    );
    assert!(
        description.contains(&format!("terminal_id {} in pod_id {}", terminal.id, pod.id)),
        "expected the real terminal id (and its pod) in: {description}"
    );
}

#[sqlx::test]
async fn test_describe_live_state_lists_the_current_todo_list(pool: PgPool) {
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::set_conversation_todos(
        &pool,
        conversation.id,
        &[
            anthropic::tools::TodoItem {
                content: "write the plan".to_string(),
                status: anthropic::tools::TodoStatus::Completed,
            },
            anthropic::tools::TodoItem {
                content: "implement".to_string(),
                status: anthropic::tools::TodoStatus::InProgress,
            },
        ],
    )
    .await
    .expect("seed todos");

    let description = describe_live_state(&pool, conversation.id).await;

    assert!(
        description.contains("write the plan") && description.contains("completed"),
        "expected the first todo and its status in: {description}"
    );
    assert!(
        description.contains("implement") && description.contains("in_progress"),
        "expected the second todo and its status in: {description}"
    );
}

/// SME-51 B10: the usage that triggered a compaction describes the old,
/// long history. Kept after it, a failed or stopped next call left the
/// following turn compacting again at once.
#[sqlx::test]
async fn test_a_compaction_forgets_the_usage_that_triggered_it(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    db::create_message(&pool, conversation.id, "user", &[anthropic::ContentBlock::Text { text: "earlier".to_string() }])
        .await
        .expect("seed");
    db::create_message(&pool, conversation.id, "assistant", &[anthropic::ContentBlock::Text { text: "reply".to_string() }])
        .await
        .expect("seed");
    let near_limit = anthropic::TokenUsage {
        input_tokens: 190_000,
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
    };
    db::upsert_conversation_usage(&pool, conversation.id, &near_limit).await.expect("seed usage");
    start_mock_upstream(&pool, vec![text_reply_body("Summary: earlier.")]).await;
    let turn_model = crate::providers::resolve_turn_model(&pool, conversation.id)
        .await
        .expect("the mock is the default");
    compact_conversation(&pool, conversation.id, &turn_model)
        .await
        .expect("compaction");
    assert_eq!(db::get_conversation_usage(&pool, conversation.id).await.expect("usage"), None);
}

#[sqlx::test]
async fn test_run_turn_compacts_before_sending_when_usage_is_near_the_ceiling(pool: PgPool) {
    let _guard = lock_turn_tests();
    // An id no other test's database hands out: what this test hears on
    // its conversation's channel mustn't be another test's (SME-135).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    // A prior turn's persisted message plus usage close enough to the
    // reserved ceiling (200_000 - 16_384 = 183_616 for the default
    // "claude-opus-4-8" model — see `context_window_for`/`MIN_REPLY_TOKENS`)
    // that the very next turn must compact before sending, regardless
    // of how small the new message's own estimate is.
    db::create_message(
        &pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text {
            text: "earlier message".to_string(),
        }],
    )
    .await
    .expect("seed earlier message");
    db::upsert_conversation_usage(
        &pool,
        conversation.id,
        &anthropic::TokenUsage {
            input_tokens: 190_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    )
    .await
    .expect("seed usage");

    // First request the mock upstream sees is the compaction's own
    // summarization call; the second is the real turn, sent afterward
    // using the now-compacted history.
    let summary_body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Summary: discussed earlier topics. Nothing currently live."}}"#,
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
    let real_body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Hi again"}}"#,
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
    start_mock_upstream(&pool, vec![summary_body, real_body]).await;

    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text {
            text: "new question".to_string(),
        }],
    };

    let mut events = events::subscribe(conversation.id);
    let messages = run_turn(&pool, conversation.id, new_message)
        .await
        .expect("run_turn should succeed");

    // SME-40 F5: the compaction's messages reach watching tabs live,
    // like every other saved message, not only after a reload.
    let published_summary = drain_events(&mut events).await.into_iter().any(|event| match event {
        events::ConversationEvent::MessagesAppended { messages } => messages.iter().any(|m| {
            m.blocks().unwrap_or_default().iter().any(|b| {
                matches!(b, anthropic::ContentBlock::CompactionSummary { .. })
            })
        }),
        _ => false,
    });
    assert!(published_summary, "the compaction summary wasn't published to watching tabs");

    let all_messages = db::list_messages(&pool, conversation.id)
        .await
        .expect("list messages");
    let compaction_summary = all_messages.iter().find_map(|m| {
        m.blocks().ok()?.into_iter().find_map(|b| match b {
            anthropic::ContentBlock::CompactionSummary {
                summary,
                covers_through_message_id,
            } => Some((summary, covers_through_message_id)),
            _ => None,
        })
    });
    assert!(
        compaction_summary.is_some(),
        "expected a CompactionSummary message to have been persisted, got: {all_messages:?}"
    );
    assert!(
        compaction_summary
            .unwrap()
            .0
            .contains("discussed earlier topics"),
        "expected the real summarization response's text to be what got persisted"
    );

    // Nothing already stored was rewritten or deleted — the original
    // seeded message and the new question are both still there in
    // full, untouched.
    assert!(
        all_messages
            .iter()
            .any(|m| m
                .blocks()
                .unwrap_or_default()
                .contains(&anthropic::ContentBlock::Text {
                    text: "earlier message".to_string()
                }))
    );
    assert!(
        all_messages
            .iter()
            .any(|m| m
                .blocks()
                .unwrap_or_default()
                .contains(&anthropic::ContentBlock::Text {
                    text: "new question".to_string()
                }))
    );

    // The real turn still completed successfully, replying to the
    // post-compaction continuation prompt.
    let last = messages.last().expect("at least one message");
    assert_eq!(last.role, "assistant");
    assert_eq!(
        last.blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::Text {
            text: "Hi again".to_string()
        }]
    );
}

#[sqlx::test]
async fn test_run_turn_errors_when_max_turns_exceeded(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    // Always responds with a tool_use turn calling `todoread` (a fast,
    // valid call), so the loop never reaches a final reply and must give up
    // after MAX_TURNS rather than looping forever.
    let tool_use_body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_01","name":"todoread","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_mock_upstream(&pool, vec![tool_use_body]).await;

    let new_message = anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text {
            text: "loop forever".to_string(),
        }],
    };

    // Goes through run_turn_bounded directly with a small bound rather
    // than run_turn (which would replay the mock upstream the real
    // MAX_TURNS — 10,000 — times just to prove the same "give up and
    // error" behavior).
    let result = run_turn_bounded(&pool, conversation.id, Some(new_message), 3, false, false).await;
    assert!(result.is_err(), "expected an error, got {result:?}");
}

#[test]
fn test_conversation_lock_is_shared_per_conversation_id_only() {
    let a1 = conversation_lock(9001);
    let a2 = conversation_lock(9001);
    let b = conversation_lock(9002);
    assert!(
        Arc::ptr_eq(&a1, &a2),
        "same conversation id should share one lock"
    );
    assert!(
        !Arc::ptr_eq(&a1, &b),
        "different conversation ids should get different locks"
    );
}

#[sqlx::test]
async fn test_run_turn_serializes_concurrent_calls_for_the_same_conversation(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    let body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
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

    let conversation_id = conversation.id;
    let pool_a = pool.clone();
    let pool_b = pool.clone();

    let task_a = tokio::spawn(async move {
        let message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "first".to_string(),
            }],
        };
        run_turn(&pool_a, conversation_id, message).await
    });
    let task_b = tokio::spawn(async move {
        let message = anthropic::AnthropicMessage {
            role: "user".to_string(),
            content: vec![anthropic::ContentBlock::Text {
                text: "second".to_string(),
            }],
        };
        run_turn(&pool_b, conversation_id, message).await
    });

    let (result_a, result_b) = tokio::join!(task_a, task_b);
    result_a
        .expect("task a should not panic")
        .expect("run_turn a should succeed");
    result_b
        .expect("task b should not panic")
        .expect("run_turn b should succeed");

    let all = db::list_messages(&pool, conversation_id)
        .await
        .expect("list messages");
    assert_eq!(all.len(), 4);
    let roles: Vec<&str> = all.iter().map(|m| m.role.as_str()).collect();
    assert_eq!(
        roles,
        vec!["user", "assistant", "user", "assistant"],
        "the per-conversation lock should serialize the two calls into two complete \
         (user, assistant) pairs, never interleaved"
    );
}
