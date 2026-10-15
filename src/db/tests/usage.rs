use super::*;

/// Mechanical CRUD: a characterization round trip rather than
/// test-first (SME-106).
#[sqlx::test]
async fn test_model_calls_add_up_and_only_a_turns_call_is_the_last_known_usage(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let other = create_conversation(&pool).await.expect("another conversation");
    let provider = test_provider(&pool, "p").await;
    assert_eq!(
        get_conversation_spend(&pool, conversation.id).await.expect("spend"),
        crate::models::ConversationSpend::default(),
        "no calls yet"
    );

    let call = |kind, usage, cost_usd| ModelCall {
        conversation_id: conversation.id,
        provider_id: provider.id,
        model: "m",
        kind,
        usage,
        cost_usd,
    };
    record_model_call(&pool, &call(ModelCallKind::Turn, usage(10, 1, 100, 0), Some(0.5)))
        .await
        .expect("first turn call");
    record_model_call(&pool, &call(ModelCallKind::Turn, usage(20, 2, 0, 100), Some(0.25)))
        .await
        .expect("second turn call");
    record_model_call(&pool, &call(ModelCallKind::Compaction, usage(300, 3, 0, 0), None))
        .await
        .expect("compaction call");
    record_model_call(
        &pool,
        &ModelCall { conversation_id: other.id, ..call(ModelCallKind::Turn, usage(1, 1, 1, 1), Some(9.0)) },
    )
    .await
    .expect("another conversation's call");

    assert_eq!(
        get_conversation_spend(&pool, conversation.id).await.expect("spend"),
        crate::models::ConversationSpend {
            calls: 3,
            input_tokens: 330,
            output_tokens: 6,
            cache_creation_input_tokens: 100,
            cache_read_input_tokens: 100,
            cost_usd: Some(0.75),
            unpriced_calls: 1,
        }
    );
    assert_eq!(
        get_conversation_usage(&pool, conversation.id).await.expect("usage"),
        Some(usage(20, 2, 0, 100)),
        "the last turn call, not the compaction after it"
    );

    delete_inference_provider(&pool, provider.id).await.expect("delete provider");
    let kept: (Option<i64>, String) =
        sqlx::query_as("SELECT provider_id, model FROM model_call_usage WHERE conversation_id = $1 LIMIT 1")
            .bind(conversation.id)
            .fetch_one(&pool)
            .await
            .expect("rows outlive their provider");
    assert_eq!(kept, (None, "m".to_string()));
    assert_eq!(get_conversation_spend(&pool, conversation.id).await.expect("spend").calls, 3);

    delete_conversation(&pool, conversation.id).await.expect("delete conversation");
    assert_eq!(get_conversation_spend(&pool, conversation.id).await.expect("spend").calls, 0, "cascade");
    assert_eq!(get_conversation_spend(&pool, other.id).await.expect("spend").calls, 1);
}

#[sqlx::test]
async fn test_model_spend_sums_each_providers_model_over_the_period(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let kept = test_provider(&pool, "kept").await;
    let gone = test_provider(&pool, "gone").await;
    let call = |provider_id, model, input, cost_usd| ModelCall {
        conversation_id: conversation.id,
        provider_id,
        model,
        kind: ModelCallKind::Turn,
        usage: usage(input, 1, 0, 0),
        cost_usd,
    };
    for c in [
        call(kept.id, "a", 10, Some(1.0)),
        call(kept.id, "a", 20, None),
        call(kept.id, "b", 5, Some(3.0)),
        call(gone.id, "a", 7, Some(0.5)),
        call(kept.id, "old", 1, Some(100.0)),
    ] {
        record_model_call(&pool, &c).await.expect("record");
    }
    sqlx::query("UPDATE model_call_usage SET created_at = now() - interval '31 days' WHERE model = 'old'")
        .execute(&pool)
        .await
        .expect("age one call");
    delete_inference_provider(&pool, gone.id).await.expect("delete");

    let since = chrono::Utc::now().naive_utc() - chrono::Duration::days(30);
    let spend = model_spend_since(&pool, since).await.expect("spend");
    let rows: Vec<(Option<&str>, &str, i64, i64, Option<f64>, i64)> = spend
        .iter()
        .map(|s| (s.provider_name.as_deref(), s.model.as_str(), s.calls, s.input_tokens, s.cost_usd, s.unpriced_calls))
        .collect();
    assert_eq!(
        rows,
        vec![
            (Some("kept"), "b", 1, 5, Some(3.0), 0),
            (Some("kept"), "a", 2, 30, Some(1.0), 1),
            (None, "a", 1, 7, Some(0.5), 0),
        ],
        "costliest first; the 31-day-old call is out; a deleted provider's calls stay"
    );
}

/// B50 (SME-122): deleting a conversation keeps its calls in the spend
/// it was billed for, with no conversation; the conversation's own
/// spend goes with it.
#[sqlx::test]
async fn test_a_deleted_conversations_calls_stay_in_the_spend(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let provider = test_provider(&pool, "p").await;
    record_model_call(
        &pool,
        &ModelCall {
            conversation_id: conversation.id,
            provider_id: provider.id,
            model: "m",
            kind: ModelCallKind::Turn,
            usage: usage(10, 1, 0, 0),
            cost_usd: Some(2.0),
        },
    )
    .await
    .expect("record");

    delete_conversation(&pool, conversation.id).await.expect("delete");

    let since = chrono::Utc::now().naive_utc() - chrono::Duration::days(30);
    let spend = model_spend_since(&pool, since).await.expect("spend");
    let rows: Vec<(&str, i64, Option<f64>)> = spend.iter().map(|s| (s.model.as_str(), s.calls, s.cost_usd)).collect();
    assert_eq!(rows, vec![("m", 1, Some(2.0))], "the deleted conversation's call still counts");
    let orphaned: Option<i64> = sqlx::query_scalar("SELECT conversation_id FROM model_call_usage")
        .fetch_one(&pool)
        .await
        .expect("the call's row");
    assert_eq!(orphaned, None);
}

/// A turn reads its provider when it starts; the user may delete the
/// provider before the reply finishes. The call is still recorded, with
/// no provider, as rows the deletion itself nulls (code review 1).
#[sqlx::test]
async fn test_a_call_on_a_provider_deleted_mid_turn_is_recorded_without_it(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let provider = test_provider(&pool, "gone").await;
    delete_inference_provider(&pool, provider.id).await.expect("delete");

    record_model_call(
        &pool,
        &ModelCall {
            conversation_id: conversation.id,
            provider_id: provider.id,
            model: "m",
            kind: ModelCallKind::Turn,
            usage: usage(10, 1, 0, 0),
            cost_usd: None,
        },
    )
    .await
    .expect("a deleted provider doesn't fail the turn");

    let provider_id: Option<i64> =
        sqlx::query_scalar("SELECT provider_id FROM model_call_usage WHERE conversation_id = $1")
            .bind(conversation.id)
            .fetch_one(&pool)
            .await
            .expect("the row");
    assert_eq!(provider_id, None);
    assert_eq!(get_conversation_usage(&pool, conversation.id).await.expect("usage"), Some(usage(10, 1, 0, 0)));
}

/// The same, with the delete still uncommitted when the call is recorded:
/// the record waits for it and then sees no provider, rather than taking
/// the id and failing the foreign key once the delete commits (code
/// review 2).
#[sqlx::test]
async fn test_a_call_recorded_while_its_provider_is_being_deleted_is_recorded_without_it(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let provider = test_provider(&pool, "going").await;

    let mut deleting = pool.begin().await.expect("begin the delete");
    sqlx::query("DELETE FROM inference_providers WHERE id = $1")
        .bind(provider.id)
        .execute(&mut *deleting)
        .await
        .expect("delete, uncommitted");

    let record = tokio::spawn({
        let pool = pool.clone();
        let (conversation_id, provider_id) = (conversation.id, provider.id);
        async move {
            record_model_call(
                &pool,
                &ModelCall {
                    conversation_id,
                    provider_id,
                    model: "m",
                    kind: ModelCallKind::Turn,
                    usage: usage(10, 1, 0, 0),
                    cost_usd: None,
                },
            )
            .await
        }
    });
    // Commit only once the record is waiting on the delete's row lock.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
                  WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(&pool)
        .await
        .expect("pg_stat_activity");
        if waiting > 0 {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the record never waited on the delete");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    deleting.commit().await.expect("commit the delete");

    record
        .await
        .expect("record task")
        .expect("a provider deleted while the call is recorded doesn't fail the turn");
    let provider_id: Option<i64> =
        sqlx::query_scalar("SELECT provider_id FROM model_call_usage WHERE conversation_id = $1")
            .bind(conversation.id)
            .fetch_one(&pool)
            .await
            .expect("the row");
    assert_eq!(provider_id, None);
}
