//! `todowrite` and `todoread`: storing and returning the conversation's list.

use super::*;

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_todowrite_tool_rejects_empty_content(pool: sqlx::PgPool) {
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let result = todowrite_tool(
        &pool,
        conversation.id,
        &serde_json::json!({"todos": [{"content": "", "status": "pending"}]}),
    )
    .await;
    let message = result.expect_err("expected empty content to be rejected");
    assert!(
        message.contains("empty"),
        "expected an error naming the empty content, got: {message}"
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_todowrite_tool_persists_and_returns_the_list(pool: sqlx::PgPool) {
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let result = todowrite_tool(
        &pool,
        conversation.id,
        &serde_json::json!({"todos": [
            {"content": "write the plan", "status": "completed"},
            {"content": "implement", "status": "in_progress"}
        ]}),
    )
    .await
    .expect("todowrite should succeed");

    let stored = db::get_conversation_todos(&pool, conversation.id)
        .await
        .expect("lookup");
    assert_eq!(
        stored,
        vec![
            TodoItem {
                content: "write the plan".to_string(),
                status: TodoStatus::Completed,
            },
            TodoItem {
                content: "implement".to_string(),
                status: TodoStatus::InProgress,
            },
        ]
    );
    let returned: Vec<TodoItem> =
        serde_json::from_str(&result).expect("todowrite should return the stored list");
    assert_eq!(returned, stored);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_todowrite_tool_overwrites_previous_list(pool: sqlx::PgPool) {
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    todowrite_tool(
        &pool,
        conversation.id,
        &serde_json::json!({"todos": [{"content": "first", "status": "pending"}]}),
    )
    .await
    .expect("first todowrite should succeed");

    todowrite_tool(
        &pool,
        conversation.id,
        &serde_json::json!({"todos": [{"content": "second", "status": "pending"}]}),
    )
    .await
    .expect("second todowrite should succeed");

    let stored = db::get_conversation_todos(&pool, conversation.id)
        .await
        .expect("lookup");
    assert_eq!(
        stored,
        vec![TodoItem {
            content: "second".to_string(),
            status: TodoStatus::Pending,
        }]
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_todoread_tool_returns_current_list(pool: sqlx::PgPool) {
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::set_conversation_todos(
        &pool,
        conversation.id,
        &[TodoItem {
            content: "seeded".to_string(),
            status: TodoStatus::Pending,
        }],
    )
    .await
    .expect("seed todos");

    let result = todoread_tool(&pool, conversation.id)
        .await
        .expect("todoread should succeed");
    let returned: Vec<TodoItem> =
        serde_json::from_str(&result).expect("todoread should return the stored list");
    assert_eq!(
        returned,
        vec![TodoItem {
            content: "seeded".to_string(),
            status: TodoStatus::Pending,
        }]
    );
}

