use super::*;

/// SME-51 B7: one live pod per conversation is the database's rule,
/// not only a check `create_pod` makes before inserting.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_conversation_cant_have_two_live_pods(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let first = create_sandbox_pod(&pool, conversation.id).await.expect("first pod");
    let second = create_sandbox_pod(&pool, conversation.id).await;
    assert!(
        second.as_ref().is_err_and(|e| e.as_database_error().is_some_and(|d| d.is_unique_violation())),
        "a second live pod was recorded: {second:?}"
    );
    terminate_sandbox_pod(&pool, first.id).await.expect("terminate");
    create_sandbox_pod(&pool, conversation.id).await.expect("a pod after the first ended");
}

/// SME-41 D11: the default title is now sentence case; a conversation
/// still carrying the old "New Conversation" gets auto-titled too.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_old_style_default_title_is_still_replaced(pool: PgPool) {
    assert_eq!(DEFAULT_TITLE, "New conversation");
    let conversation = create_conversation(&pool).await.expect("create conversation");
    sqlx::query("UPDATE conversations SET title = 'New Conversation' WHERE id = $1")
        .bind(conversation.id)
        .execute(&pool)
        .await
        .expect("give it the old title");
    create_message(
        &pool,
        conversation.id,
        "user",
        &[crate::anthropic::ContentBlock::Text { text: "fix the build".to_string() }],
    )
    .await
    .expect("send a message");
    let titled = list_conversations(&pool).await.expect("list").into_iter().find(|c| c.id == conversation.id).expect("found");
    assert_eq!(titled.title, "fix the build");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_create_and_list_conversations_round_trip(pool: PgPool) {
    let created = create_conversation(&pool)
        .await
        .expect("create conversation");
    assert_eq!(created.title, DEFAULT_TITLE);

    let all = list_conversations(&pool).await.expect("list conversations");
    assert!(
        all.iter().any(|c| c.id == created.id),
        "created conversation should appear in list, got: {all:?}"
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_get_conversation_usage_returns_none_when_never_set(pool: PgPool) {
    let conversation = create_conversation(&pool)
        .await
        .expect("create conversation");
    assert_eq!(
        get_conversation_usage(&pool, conversation.id)
            .await
            .expect("lookup"),
        None
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_upsert_conversation_usage_round_trips_and_overwrites(pool: PgPool) {
    let conversation = create_conversation(&pool)
        .await
        .expect("create conversation");
    let first = crate::anthropic::TokenUsage {
        input_tokens: 100,
        output_tokens: 20,
        cache_creation_input_tokens: 5,
        cache_read_input_tokens: 0,
    };
    upsert_conversation_usage(&pool, conversation.id, &first)
        .await
        .expect("upsert should succeed");
    assert_eq!(
        get_conversation_usage(&pool, conversation.id)
            .await
            .expect("lookup"),
        Some(first)
    );

    // A second call overwrites, rather than accumulating — "last known
    // only," not a history.
    let second = crate::anthropic::TokenUsage {
        input_tokens: 150,
        output_tokens: 30,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 5,
    };
    upsert_conversation_usage(&pool, conversation.id, &second)
        .await
        .expect("second upsert should succeed");
    assert_eq!(
        get_conversation_usage(&pool, conversation.id)
            .await
            .expect("lookup"),
        Some(second)
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_get_conversation_todos_is_empty_when_never_written(pool: PgPool) {
    let conversation = create_conversation(&pool)
        .await
        .expect("create conversation");
    assert_eq!(
        get_conversation_todos(&pool, conversation.id)
            .await
            .expect("lookup"),
        Vec::new()
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_set_conversation_todos_round_trips_and_overwrites(pool: PgPool) {
    use crate::anthropic::tools::{TodoItem, TodoStatus};

    let conversation = create_conversation(&pool)
        .await
        .expect("create conversation");
    let first = vec![
        TodoItem {
            content: "write the plan".to_string(),
            status: TodoStatus::Completed,
        },
        TodoItem {
            content: "implement".to_string(),
            status: TodoStatus::InProgress,
        },
    ];
    set_conversation_todos(&pool, conversation.id, &first)
        .await
        .expect("set should succeed");
    assert_eq!(
        get_conversation_todos(&pool, conversation.id)
            .await
            .expect("lookup"),
        first
    );

    // A second call overwrites wholesale, rather than merging with the
    // first — "last known only," not an append log.
    let second = vec![TodoItem {
        content: "ship it".to_string(),
        status: TodoStatus::Pending,
    }];
    set_conversation_todos(&pool, conversation.id, &second)
        .await
        .expect("second set should succeed");
    assert_eq!(
        get_conversation_todos(&pool, conversation.id)
            .await
            .expect("lookup"),
        second
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_create_message_round_trips_and_lists_in_order(pool: PgPool) {
    let conversation = create_conversation(&pool)
        .await
        .expect("create conversation");
    let first = create_message(
        &pool,
        conversation.id,
        "user",
        &[ContentBlock::Text {
            text: "hello".to_string(),
        }],
    )
    .await
    .expect("create first message");
    let second = create_message(
        &pool,
        conversation.id,
        "assistant",
        &[ContentBlock::Text {
            text: "hi there".to_string(),
        }],
    )
    .await
    .expect("create second message");

    let messages = list_messages(&pool, conversation.id)
        .await
        .expect("list messages");
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].id, first.id);
    assert_eq!(messages[0].role, "user");
    assert_eq!(messages[1].id, second.id);
    assert_eq!(messages[1].role, "assistant");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_create_message_stores_content_as_json_blocks(pool: PgPool) {
    let conversation = create_conversation(&pool)
        .await
        .expect("create conversation");
    let blocks = vec![
        ContentBlock::ToolUse {
            id: "toolu_01".to_string(),
            name: "add".to_string(),
            input: serde_json::json!({"a": 1, "b": 2}),
        },
        ContentBlock::Text {
            text: "done".to_string(),
        },
    ];
    let saved = create_message(&pool, conversation.id, "assistant", &blocks)
        .await
        .expect("create message");

    assert_eq!(
        saved
            .blocks()
            .expect("stored content should parse as blocks"),
        blocks
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_first_user_message_auto_titles_conversation(pool: PgPool) {
    let conversation = create_conversation(&pool)
        .await
        .expect("create conversation");
    create_message(
        &pool,
        conversation.id,
        "user",
        &[ContentBlock::Text {
            text: "What's the weather like today?".to_string(),
        }],
    )
    .await
    .expect("create message");

    let all = list_conversations(&pool).await.expect("list conversations");
    let updated = all
        .iter()
        .find(|c| c.id == conversation.id)
        .expect("conversation should still exist");
    assert_eq!(updated.title, "What's the weather like today?");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_second_message_does_not_overwrite_title(pool: PgPool) {
    let conversation = create_conversation(&pool)
        .await
        .expect("create conversation");
    create_message(
        &pool,
        conversation.id,
        "user",
        &[ContentBlock::Text {
            text: "first message".to_string(),
        }],
    )
    .await
    .expect("create first message");
    create_message(
        &pool,
        conversation.id,
        "assistant",
        &[ContentBlock::Text {
            text: "a reply".to_string(),
        }],
    )
    .await
    .expect("create assistant message");
    create_message(
        &pool,
        conversation.id,
        "user",
        &[ContentBlock::Text {
            text: "second message".to_string(),
        }],
    )
    .await
    .expect("create second message");

    let all = list_conversations(&pool).await.expect("list conversations");
    let updated = all
        .iter()
        .find(|c| c.id == conversation.id)
        .expect("conversation should still exist");
    assert_eq!(updated.title, "first message");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_delete_conversation_removes_it_from_list(pool: PgPool) {
    let conversation = create_conversation(&pool)
        .await
        .expect("create conversation");
    delete_conversation(&pool, conversation.id)
        .await
        .expect("delete conversation");

    let all = list_conversations(&pool).await.expect("list conversations");
    assert!(
        !all.iter().any(|c| c.id == conversation.id),
        "deleted conversation should not appear in list, got: {all:?}"
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_delete_conversation_cascades_to_messages(pool: PgPool) {
    let conversation = create_conversation(&pool)
        .await
        .expect("create conversation");
    create_message(
        &pool,
        conversation.id,
        "user",
        &[ContentBlock::Text {
            text: "hello".to_string(),
        }],
    )
    .await
    .expect("create message");

    delete_conversation(&pool, conversation.id)
        .await
        .expect("delete conversation");

    let messages = list_messages(&pool, conversation.id)
        .await
        .expect("list messages");
    assert!(
        messages.is_empty(),
        "messages should be cascade-deleted with their conversation, got: {messages:?}"
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_delete_nonexistent_conversation_is_a_no_op(pool: PgPool) {
    delete_conversation(&pool, -1)
        .await
        .expect("deleting a nonexistent conversation should not error");
}

/// SME-78: a quiet notice is saved without moving its conversation in the
/// sidebar or titling an untitled one with the notice's text.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_create_message_in_place_leaves_updated_at_and_a_default_title(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    sqlx::query("UPDATE conversations SET updated_at = updated_at - interval '1 hour' WHERE id = $1")
        .bind(conversation.id)
        .execute(&pool)
        .await
        .expect("age it");
    let before = list_conversations(&pool).await.expect("read").into_iter().find(|c| c.id == conversation.id).expect("row");

    let saved = create_message_in_place(&pool, conversation.id, "user", &[ContentBlock::Text { text: "Sandbox pod 7 was stopped".to_string() }])
        .await
        .expect("save it");

    let after = list_conversations(&pool).await.expect("read").into_iter().find(|c| c.id == conversation.id).expect("row");
    assert_eq!(after.updated_at, before.updated_at, "the sidebar order stays");
    assert_eq!(after.title, before.title, "still the default title");
    let messages = list_messages(&pool, conversation.id).await.expect("list");
    assert_eq!(messages.iter().map(|m| m.id).collect::<Vec<_>>(), vec![saved.id], "the message is saved");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_brief_titles_its_conversation_by_its_bold_phrase(pool: PgPool) {
    let parent = create_conversation(&pool).await.expect("create the parent");
    let conversation = create_conversation(&pool).await.expect("create");
    sqlx::query("UPDATE conversations SET parent_id = $1 WHERE id = $2")
        .bind(parent.id)
        .bind(conversation.id)
        .execute(&pool)
        .await
        .expect("make it an agent");
    let brief = "You are the **Planner seat on SME-9** (Linear, team Smelt Agent). Plan it.";
    create_message(&pool, conversation.id, "user", &[crate::anthropic::ContentBlock::Text { text: brief.to_string() }])
        .await
        .expect("message");
    let titled = list_conversations(&pool).await.expect("list").into_iter().find(|c| c.id == conversation.id).expect("found");
    assert_eq!(titled.title, "Planner seat on SME-9");
    // The user's own conversation keeps its line, bold marks dropped
    // (review 1 M2).
    create_message(&pool, parent.id, "user", &[crate::anthropic::ContentBlock::Text { text: "Fix the **login** page".to_string() }])
        .await
        .expect("message");
    let own = list_conversations(&pool).await.expect("list").into_iter().find(|c| c.id == parent.id).expect("found");
    assert_eq!(own.title, "Fix the login page");
}

/// The SME-123 DR1 migration, run again over titles saved the old way.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_title_migration_tidies_titles_saved_with_markdown(pool: PgPool) {
    let cases = [
        ("You are the **Ticket Driver for SME-345** (Linear, team Smelt", "Ticket Driver for SME-345"),
        ("You are the **Developer seat for a very long ticket name that", "You are the Developer seat for a very long ticket name that"),
        ("fix the build", "fix the build"),
    ];
    let parent: i64 = sqlx::query_scalar("INSERT INTO conversations (title) VALUES ('Fix the **login** page') RETURNING id")
        .fetch_one(&pool)
        .await
        .expect("insert the user's own");
    let mut ids = Vec::new();
    for (saved, _) in cases {
        let id: i64 = sqlx::query_scalar("INSERT INTO conversations (title, parent_id) VALUES ($1, $2) RETURNING id")
            .bind(saved)
            .bind(parent)
            .fetch_one(&pool)
            .await
            .expect("insert");
        ids.push(id);
    }
    sqlx::raw_sql(include_str!("../../../migrations/20261103120000_conversation_titles_without_markdown.sql"))
        .execute(&pool)
        .await
        .expect("run the migration");
    let own: String = sqlx::query_scalar("SELECT title FROM conversations WHERE id = $1").bind(parent).fetch_one(&pool).await.expect("title");
    assert_eq!(own, "Fix the login page", "the user's own title keeps its words (review 1 M2)");
    for (id, (_, want)) in ids.into_iter().zip(cases) {
        let title: String = sqlx::query_scalar("SELECT title FROM conversations WHERE id = $1").bind(id).fetch_one(&pool).await.expect("title");
        assert_eq!(title, want);
    }
}

/// SME-354 B19's migration: a failure notice saved before the mark gains
/// it, so it still shows as a notice; nothing else changes, and a second
/// run changes nothing more.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_failure_notice_migration_marks_the_notices_saved_before_it(pool: PgPool) {
    use crate::anthropic::ContentBlock;
    let conversation = create_conversation(&pool).await.expect("conversation");
    let legacy = format!("{}model provider error 500", crate::api::chat::FAILURE_NOTICE_START);
    let text = |t: &str| vec![ContentBlock::Text { text: t.to_string() }];
    let notice = create_message(&pool, conversation.id, "user", &text(&legacy)).await.expect("legacy notice");
    let other = create_message(&pool, conversation.id, "user", &text("hello")).await.expect("plain text");
    let reply = create_message(&pool, conversation.id, "assistant", &text(&legacy)).await.expect("a reply quoting it");
    for _ in 0..2 {
        sqlx::raw_sql(include_str!("../../../migrations/20261103135400_mark_failure_notices.sql"))
            .execute(&pool)
            .await
            .expect("run the migration");
    }
    let saved = |id: i64| {
        let pool = pool.clone();
        async move {
            let content: String =
                sqlx::query_scalar("SELECT content FROM messages WHERE id = $1").bind(id).fetch_one(&pool).await.expect("content");
            let blocks: Vec<ContentBlock> = serde_json::from_str(&content).expect("still JSON blocks");
            match blocks.as_slice() {
                [ContentBlock::Text { text }] => text.clone(),
                other => panic!("one text block: {other:?}"),
            }
        }
    };
    assert_eq!(
        crate::api::chat::parse_failure_notice(&saved(notice.id).await),
        Some("model provider error 500"),
        "the old notice still reads as one"
    );
    assert_eq!(saved(notice.id).await, crate::api::chat::failure_notice("model provider error 500"), "marked once");
    assert_eq!(saved(other.id).await, "hello");
    assert_eq!(saved(reply.id).await, legacy, "only the user role's notices");
}
