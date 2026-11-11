//! SME-73, the half that `overflow.rs` shed when it neared the size
//! ceiling (SME-422): the two Stop races the pause hooks exist for —
//! what a Stop mid-recovery leaves saved — and the compaction's own
//! summary refusals, C1 recovered once and C2 fatal the second time.
//! The mock and helpers are the parent's.

use super::*;

/// Stop race one: a Stop that catches the recovery between the window
/// write and the summary call ends the turn stopped with only the
/// window saved — the summary is never asked for, nothing else lands.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_before_the_summary_saves_nothing_but_the_window(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let upstream = start_refusal_mock(&pool, vec![too_long(120_000, 131_072), Ok(text_reply_body("Summary: never sent."))]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let id = conversation.id;

    let (reached, resume) = test_hooks::pause_before_summary_call(id);
    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, id, hello()).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), reached)
        .await
        .expect("the recovery reached the summary call")
        .expect("hook");

    stop_turn_now(id);
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), turn)
        .await
        .expect("the stopped turn ends")
        .expect("join");
    assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");
    drop(resume);

    assert_eq!(upstream.count(), 1, "the summary was never asked for");
    assert!(compaction_summaries(&pool, id).await.is_empty(), "nothing of a compaction landed");
    let saved = db::list_messages(&pool, id).await.expect("messages");
    assert_eq!(last_reply(&saved), None, "no reply");
    assert_eq!(saved_learned_context_window(&pool).await, Some(131_072), "the window write happened before the pause and stays");
}

/// Stop race two: a Stop after the compaction, before the retry, keeps
/// both (the compacted conversation and the learned window) and sends
/// the retry never.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_before_the_retry_keeps_the_compaction_and_the_window(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let upstream = start_refusal_mock(
        &pool,
        vec![
            too_long(120_000, 131_072),
            Ok(text_reply_body("Summary: it was going.")),
            Ok(text_reply_body("Never sent.")),
        ],
    )
    .await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    db::create_message(&pool, conversation.id, "user", &[anthropic::ContentBlock::Text { text: "earlier".to_string() }])
        .await
        .expect("seed");
    let id = conversation.id;

    let (reached, resume) = test_hooks::pause_before_retry(id);
    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, id, hello()).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(10), reached)
        .await
        .expect("the recovery reached the retry")
        .expect("hook");

    stop_turn_now(id);
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), turn)
        .await
        .expect("the stopped turn ends")
        .expect("join");
    assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");
    drop(resume);

    assert_eq!(upstream.count(), 2, "the refusal and the summary went, the retry never did");
    assert_eq!(compaction_summaries(&pool, id).await, ["Summary: it was going."], "a finished compaction stays");
    let saved = db::list_messages(&pool, id).await.expect("messages");
    assert_eq!(last_reply(&saved), None, "the retry's reply is never sent, so nothing to discard");
    assert_eq!(saved_learned_context_window(&pool).await, Some(131_072));
}

/// C1: a compaction whose own summary call is refused gets the same
/// treatment once, inside `compact_conversation`: learn the named
/// window, summarise again with the transcript sized to it, and carry
/// on — the compaction span stays green, the refused summary's chat
/// span stays marked.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_compaction_that_overflows_retries_its_summary_once(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let upstream = start_refusal_mock(
        &pool,
        vec![
            too_long(150_000, 131_072),
            Ok(text_reply_body("Summary: recovered on the second ask.")),
            Ok(text_reply_body("Answered after the recovered compaction.")),
        ],
    )
    .await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    db::create_message(&pool, conversation.id, "user", &[anthropic::ContentBlock::Text { text: "earlier".to_string() }])
        .await
        .expect("seed");
    let near_limit = anthropic::TokenUsage {
        input_tokens: 190_000,
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
    };
    db::upsert_conversation_usage(&pool, conversation.id, &near_limit).await.expect("seed usage");
    let id = conversation.id;

    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, id, hello())).await;
    result.expect("the refused summary recovers and the turn carries on");

    assert_eq!(upstream.count(), 3, "refused summary, summary, reply");
    assert_eq!(
        compaction_summaries(&pool, id).await,
        ["Summary: recovered on the second ask."],
        "the summary asked for once more is the divider"
    );
    assert_eq!(
        compaction_blocks(&pool, id).await[0].1.as_deref(),
        Some("the model's server refused a request that didn't fit its 131,072-token window"),
        "a compaction answering its own refusal names that refusal on the divider (SME-73)"
    );
    let saved = db::list_messages(&pool, id).await.expect("messages");
    assert_eq!(last_reply(&saved).as_deref(), Some("Answered after the recovered compaction."));
    assert_eq!(saved_learned_context_window(&pool).await, Some(131_072), "the compaction's refusal learns too");

    let compactions: Vec<_> = spans.iter().filter(|s| s.name == "compaction").collect();
    let [compaction] = compactions.as_slice() else { panic!("one compaction span: {spans:?}") };
    assert_eq!(compaction.status, opentelemetry::trace::Status::Unset, "a self-recovered compaction ends green");
    assert_eq!(turn_span(&spans).status, opentelemetry::trace::Status::Unset);
    let chats = chat_spans(&spans);
    assert_eq!(chats.len(), 3, "two summary tries and the reply: {chats:?}");
    let opentelemetry::trace::Status::Error { description } = &chats[0].status else {
        panic!("the refused summary stays marked: {:?}", chats[0].status);
    };
    assert!(description.contains("prompt is too long"), "{description}");
    assert!(matches!(chats[1].status, opentelemetry::trace::Status::Unset), "the second summary succeeded");
}

/// C2: a second refusal fails the turn with the "Couldn't compact…"
/// wording — the window it names is still learned, and the compaction
/// span ends red.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_compaction_refused_twice_fails_the_turn_with_the_worded_reason(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let upstream = start_refusal_mock(&pool, vec![too_long(150_000, 131_072), llama_refused(24_000, 24_576)]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    db::create_message(&pool, conversation.id, "user", &[anthropic::ContentBlock::Text { text: "earlier".to_string() }])
        .await
        .expect("seed");
    let near_limit = anthropic::TokenUsage {
        input_tokens: 190_000,
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
    };
    db::upsert_conversation_usage(&pool, conversation.id, &near_limit).await.expect("seed usage");
    let id = conversation.id;

    let (result, spans) = crate::telemetry::capture_spans(run_turn(&pool, id, hello())).await;
    let error = result.expect_err("the summary refused twice fails the turn").to_string();
    assert!(error.contains("Couldn't compact the conversation"), "{error}");
    assert!(error.contains("even the summary request didn't fit"), "{error}");
    assert!(error.contains("(24,576 tokens)"), "the second refusal's window: {error}");
    assert!(error.contains("The server said:"), "{error}");
    assert_eq!(upstream.count(), 2, "one summary, one more, no third ask");
    assert!(compaction_summaries(&pool, id).await.is_empty(), "nothing half-done was saved");
    assert_eq!(saved_learned_context_window(&pool).await, Some(24_576), "both refusals learn, the smaller lands");

    let compactions: Vec<_> = spans.iter().filter(|s| s.name == "compaction").collect();
    let [compaction] = compactions.as_slice() else { panic!("one compaction span: {spans:?}") };
    let opentelemetry::trace::Status::Error { description } = &compaction.status else {
        panic!("the failed compaction is marked: {:?}", compaction.status);
    };
    assert!(description.contains("Couldn't compact"), "{description}");
}
