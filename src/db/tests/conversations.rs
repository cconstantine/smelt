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

/// SME-357's migration: one saved notice per family gains the mark, so the
/// readers that now require it keep showing them as notices — plain text,
/// another role's rows and already-marked notices are left alone, and a
/// second run changes nothing more.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_notice_migration_marks_every_saved_notice_family(pool: PgPool) {
    use crate::anthropic::ContentBlock;
    let conversation = create_conversation(&pool).await.expect("conversation");
    let unmark = |t: &str| {
        t.strip_prefix(crate::api::chat::NOTICE_MARK)
            .unwrap_or_else(|| panic!("the builder marks: {t}"))
            .to_string()
    };
    // One legacy row per family: the builders' saved words without the
    // mark, and the writers' wording where no builder has one (plan §
    // Migration: the 13 families plus Q1's task tags and Q2's `Notice: `).
    let legacy: Vec<String> = vec![
        crate::api::chat::STOP_NOTICE.to_string(),
        unmark(&crate::api::chat::cut_off_notice(1024, crate::api::chat::ReplyLimit::OutputCap, None)),
        crate::api::chat::REFUSAL_NOTICE_START.to_string(),
        crate::text_calls::EMPTY_REPLY_NOTICE.to_string(),
        unmark(&crate::text_calls::text_call_notice(
            &[crate::text_calls::TextCall { name: Some("read_file".to_string()) }],
            &["read_file"],
        )),
        "Terminal command c1 finished: exit code 0.".to_string(),
        "Sandbox pod 4 was stopped after 60 minutes with no activity and no command running. create_pod (or clone_repo) starts a new pod with /workspace as it was.".to_string(),
        "Sandbox pod 4 stopped unexpectedly (OOMKilled); every terminal running in it is no longer available.".to_string(),
        "Sandbox pod 4's storage was removed after 7 days stopped. /workspace and Docker's images, build cache and volumes are gone. The next sandbox starts fresh with empty volumes.".to_string(),
        "Docker in sandbox pod 4 was restarted, so the containers it was running have stopped. Images, terminals and /workspace are unaffected.".to_string(),
        "The user stopped sandbox pod 4. Its terminals, and any files outside /workspace and mounted volumes, are gone. Create a new pod if you need one.".to_string(),
        "The user removed this conversation's sandbox storage: /workspace and the Docker images, build cache and volumes are gone. The next sandbox starts with fresh empty volumes.".to_string(),
        "Language server rust-analyzer stopped. The sandbox is unaffected. start_language_server starts it again.".to_string(),
        "The user now trusts https://example.com/r.git. You can start agents on it; call start_agent again.".to_string(),
        "The user chose not to trust https://example.com/r.git, so no agent can work on it.".to_string(),
        r#"<task-notification task_id="t1" tool="count">finished: Counted to 3</task-notification>"#.to_string(),
        r#"<task-output task_id="t1" tool="count" stream="stdout">3</task-output>"#.to_string(),
        "Notice: the user deleted agent #7".to_string(),
    ];
    let text = |t: &str| vec![ContentBlock::Text { text: t.to_string() }];
    let mut ids = Vec::new();
    for words in &legacy {
        let notice = create_message(&pool, conversation.id, "user", &text(words)).await.expect("legacy notice");
        ids.push(notice.id);
    }
    let other = create_message(&pool, conversation.id, "user", &text("hello")).await.expect("plain text");
    let reply = create_message(&pool, conversation.id, "assistant", &text(&legacy[5])).await.expect("a reply quoting a notice's words");
    let marked = create_message(
        &pool,
        conversation.id,
        "user",
        &text(&format!("{}Sandbox pod 4 is big", crate::api::chat::NOTICE_MARK)),
    )
    .await
    .expect("an already-marked notice");
    for _ in 0..2 {
        sqlx::raw_sql(include_str!("../../../migrations/20261104150000_mark_saved_notices.sql"))
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
    for (id, words) in ids.iter().zip(&legacy) {
        assert_eq!(saved(*id).await, format!("{}{words}", crate::api::chat::NOTICE_MARK), "marked once: {words}");
    }
    assert_eq!(saved(other.id).await, "hello");
    assert_eq!(saved(reply.id).await, legacy[5], "only the user role's notices");
    assert_eq!(
        saved(marked.id).await,
        format!("{}Sandbox pod 4 is big", crate::api::chat::NOTICE_MARK),
        "an already-marked row keeps its single mark"
    );
}

/// SME-71: a notification names its conversation, so the attention
/// endpoint needs its title — and `None` for an id that isn't one.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_get_conversation_title_reads_it_and_misses_quietly(pool: PgPool) {
    let conversation = create_conversation(&pool).await.expect("conversation");
    let title = get_conversation_title(&pool, conversation.id).await.expect("query");
    assert_eq!(title.as_deref(), Some(DEFAULT_TITLE));
    let missing = get_conversation_title(&pool, 999_999_999).await.expect("query");
    assert_eq!(missing, None, "an id that isn't a conversation reads as no title");
}

/// SME-386 A6: a notice delivered as a wake turn's message while an
/// answered question waited is saved after the answer, in one row
/// (`create_message_taking_question`, as `deliver_notice`'s turn saves it).
/// SME-357's migration looked at a row's first block only and left such a
/// notice unmarked; the follow-up marks it, leaves the answer and every
/// other row alone, and a second run changes nothing more.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_notice_saved_after_an_answer_is_marked_too(pool: PgPool) {
    use crate::anthropic::ContentBlock;
    let conversation = create_conversation(&pool).await.expect("conversation");
    let words = "Terminal command c1 finished: exit code 0.";
    let question: crate::questions::Question =
        serde_json::from_value(serde_json::json!({"question": "Which?", "header": "Pick"})).expect("a question");
    create_pending_question(&pool, conversation.id, "toolu_q", &[question]).await.expect("asked");
    let answer = crate::questions::QuestionAnswer { selected: vec![], other: Some("This one".to_string()) };
    assert!(answer_pending_question(&pool, conversation.id, "toolu_q", &[answer]).await.expect("answered"));
    // The answer quotes the words too: a block's string is never marked.
    let result_for = |q: &StoredQuestion| ContentBlock::ToolResult {
        tool_use_id: q.tool_use_id.clone(),
        content: format!("The user answered: {words}"),
        is_error: None,
    };
    let (row, _) = create_message_taking_question(
        &pool,
        conversation.id,
        "user",
        vec![ContentBlock::Text { text: words.to_string() }],
        false,
        result_for,
    )
    .await
    .expect("saved")
    .expect("a row");
    let typed = create_message(&pool, conversation.id, "user", &[ContentBlock::Text { text: format!("see: {words}") }])
        .await
        .expect("a user's own text");
    let reply = create_message(
        &pool,
        conversation.id,
        "assistant",
        &[ContentBlock::Text { text: "ok".to_string() }, ContentBlock::Text { text: words.to_string() }],
    )
    .await
    .expect("a reply quoting it");
    let content = |id: i64| {
        let pool = pool.clone();
        async move {
            let content: String =
                sqlx::query_scalar("SELECT content FROM messages WHERE id = $1").bind(id).fetch_one(&pool).await.expect("content");
            content
        }
    };
    let (typed_before, reply_before) = (content(typed.id).await, content(reply.id).await);
    for _ in 0..2 {
        sqlx::raw_sql(include_str!("../../../migrations/20261104150000_mark_saved_notices.sql"))
            .execute(&pool)
            .await
            .expect("run SME-357's migration");
        sqlx::raw_sql(include_str!("../../../migrations/20261105160000_mark_notices_after_a_result.sql"))
            .execute(&pool)
            .await
            .expect("run the follow-up");
    }
    let saved = content(row.id).await;
    let blocks: Vec<ContentBlock> = serde_json::from_str(&saved).expect("still JSON blocks");
    match blocks.as_slice() {
        [ContentBlock::ToolResult { content, .. }, ContentBlock::Text { text }] => {
            assert_eq!(content, &format!("The user answered: {words}"), "the answer is left alone");
            assert_eq!(text, &format!("{}{words}", crate::api::chat::NOTICE_MARK), "marked once");
        }
        other => panic!("the answer, then the notice: {other:?}"),
    }
    assert!(saved.contains(r#""type":"tool_result""#), "still compact JSON: {saved}");
    assert_eq!(content(typed.id).await, typed_before, "a user's text that only contains the words");
    assert_eq!(content(reply.id).await, reply_before, "only the user role's rows");
}
