use super::*;

/// Mechanical CRUD: a characterization round trip rather than
/// test-first (SME-106).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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
        finished: true,
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

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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
        finished: true,
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
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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
            finished: true,
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
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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
            finished: true,
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
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
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
                    finished: true,
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

/// One `Turn` call on `conversation_id`, `finished` as given.
fn turn_call(conversation_id: i64, provider_id: i64, finished: bool) -> ModelCall<'static> {
    ModelCall {
        conversation_id,
        provider_id,
        model: "m",
        kind: ModelCallKind::Turn,
        usage: usage(10, 1, 0, 0),
        cost_usd: Some(2.0),
        finished,
    }
}

async fn conversation_of_the_only_call(pool: &PgPool) -> Option<i64> {
    sqlx::query_scalar("SELECT conversation_id FROM model_call_usage")
        .fetch_one(pool)
        .await
        .expect("the call's row")
}

/// SME-178 B25: a call that ends after its conversation was deleted is
/// still recorded, with no conversation, and no context usage for it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_call_whose_conversation_is_gone_is_recorded_without_it(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let provider = test_provider(&pool, "p").await;
    delete_conversation(&pool, conversation.id).await.expect("delete");

    record_model_call(&pool, &turn_call(conversation.id, provider.id, true))
        .await
        .expect("a deleted conversation doesn't lose the call");

    assert_eq!(conversation_of_the_only_call(&pool).await, None);
    let since = chrono::Utc::now().naive_utc() - chrono::Duration::days(30);
    let spend = model_spend_since(&pool, since).await.expect("spend");
    assert_eq!(spend.iter().map(|s| (s.calls, s.cost_usd)).collect::<Vec<_>>(), vec![(1, Some(2.0))]);
    let context_rows: i64 = sqlx::query_scalar("SELECT count(*) FROM conversation_context_usage")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(context_rows, 0);
}

/// The same with the conversation's delete still uncommitted: the
/// record waits for it, then sees no conversation, rather than taking
/// the id and failing the foreign key once the delete commits.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_call_recorded_while_its_conversation_is_being_deleted_is_recorded_without_it(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let provider = test_provider(&pool, "p").await;

    let mut deleting = pool.begin().await.expect("begin the delete");
    sqlx::query("DELETE FROM conversations WHERE id = $1")
        .bind(conversation.id)
        .execute(&mut *deleting)
        .await
        .expect("delete, uncommitted");

    let record = tokio::spawn({
        let pool = pool.clone();
        let (conversation_id, provider_id) = (conversation.id, provider.id);
        async move { record_model_call(&pool, &turn_call(conversation_id, provider_id, true)).await }
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

    tokio::time::timeout(std::time::Duration::from_secs(10), record)
        .await
        .expect("the record ends once the delete commits")
        .expect("record task")
        .expect("a conversation deleted while the call is recorded doesn't fail it");
    assert_eq!(conversation_of_the_only_call(&pool).await, None);
}

/// A call that didn't finish (stopped, failed) is counted, but isn't the
/// conversation's last-known usage: its reply may not be in the history.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_unfinished_turn_call_is_counted_but_isnt_the_last_known_usage(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let provider = test_provider(&pool, "p").await;
    record_model_call(&pool, &ModelCall { usage: usage(5, 5, 0, 0), ..turn_call(conversation.id, provider.id, true) })
        .await
        .expect("a finished call");

    record_model_call(&pool, &turn_call(conversation.id, provider.id, false))
        .await
        .expect("an unfinished call");

    assert_eq!(get_conversation_spend(&pool, conversation.id).await.expect("spend").calls, 2);
    assert_eq!(
        get_conversation_usage(&pool, conversation.id).await.expect("usage"),
        Some(usage(5, 5, 0, 0)),
        "the finished call's usage stands"
    );
}

/// Waits (up to 10 s) until a query in this database waits on a lock.
async fn wait_for_a_lock_waiter(pool: &PgPool, what: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_stat_activity
                  WHERE datname = current_database() AND wait_event_type = 'Lock'",
        )
        .fetch_one(pool)
        .await
        .expect("pg_stat_activity");
        if waiting > 0 {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "{what} never waited");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// A finished call whose turn is no longer current (stopped while its
/// record was on its way) is counted, but isn't the conversation's
/// last-known usage: a newer turn's may already be (code review 1).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_call_no_longer_current_is_counted_but_isnt_the_last_known_usage(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let provider = test_provider(&pool, "p").await;
    record_model_call(&pool, &ModelCall { usage: usage(5, 5, 0, 0), ..turn_call(conversation.id, provider.id, true) })
        .await
        .expect("the current call");

    let wrote = record_model_call_if_current(&pool, &turn_call(conversation.id, provider.id, true), || false)
        .await
        .expect("a call no longer current");

    assert!(!wrote, "said it wrote the last-known usage");
    assert_eq!(get_conversation_spend(&pool, conversation.id).await.expect("spend").calls, 2);
    assert_eq!(get_conversation_usage(&pool, conversation.id).await.expect("usage"), Some(usage(5, 5, 0, 0)));
}

/// Holds `conversation_id`'s row as an older call's record does from its
/// insert on, once it has seen its turn is still current.
async fn an_older_record(pool: &PgPool, conversation_id: i64) -> sqlx::Transaction<'static, sqlx::Postgres> {
    let mut older = pool.begin().await.expect("begin the older record");
    sqlx::query("SELECT id FROM conversations WHERE id = $1 FOR NO KEY UPDATE")
        .bind(conversation_id)
        .execute(&mut *older)
        .await
        .expect("hold the conversation");
    older
}

/// The older record writes its usage, which a newer write waited for,
/// and commits.
async fn finish_the_older_record(mut older: sqlx::Transaction<'static, sqlx::Postgres>, conversation_id: i64) {
    upsert_conversation_usage(&mut *older, conversation_id, &usage(1000, 0, 0, 0))
        .await
        .expect("the older usage");
    older.commit().await.expect("commit the older record");
}

/// A record that checked its turn was current, and then lost a race to
/// the next turn's, still commits first: the next turn's record waits
/// for the conversation's row and writes after it (code review 1).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_newer_record_waits_for_an_older_one_and_its_usage_stands(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let provider = test_provider(&pool, "p").await;
    let older = an_older_record(&pool, conversation.id).await;

    let newer = tokio::spawn({
        let pool = pool.clone();
        let (conversation_id, provider_id) = (conversation.id, provider.id);
        async move { record_model_call(&pool, &turn_call(conversation_id, provider_id, true)).await }
    });
    wait_for_a_lock_waiter(&pool, "the newer record").await;
    finish_the_older_record(older, conversation.id).await;

    tokio::time::timeout(std::time::Duration::from_secs(10), newer)
        .await
        .expect("the newer record ends")
        .expect("record task")
        .expect("the newer record");
    assert_eq!(get_conversation_usage(&pool, conversation.id).await.expect("usage"), Some(usage(10, 1, 0, 0)));
}

/// The same for a compaction's clear: it waits for the older record,
/// so the usage of the history it summarized doesn't come back.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_clear_waits_for_an_older_record_and_leaves_no_usage(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let older = an_older_record(&pool, conversation.id).await;

    let clear = tokio::spawn({
        let pool = pool.clone();
        let conversation_id = conversation.id;
        async move { clear_conversation_usage(&pool, conversation_id).await }
    });
    wait_for_a_lock_waiter(&pool, "the clear").await;
    finish_the_older_record(older, conversation.id).await;

    tokio::time::timeout(std::time::Duration::from_secs(10), clear)
        .await
        .expect("the clear ends")
        .expect("clear task")
        .expect("the clear");
    assert_eq!(get_conversation_usage(&pool, conversation.id).await.expect("usage"), None);
}
