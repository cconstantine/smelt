//! Who is offered the agent tools, and who may call them.

use super::*;

fn names(definitions: &[crate::anthropic::ToolDefinition]) -> Vec<&str> {
    definitions.iter().map(|d| d.name.as_str()).collect()
}

/// A conversation with a parent but no `agents` row: only `execute`'s own
/// check can refuse it a root's tool, since `start_agent`'s transaction
/// refuses only a caller with an agent row.
async fn bare_child(pool: &PgPool, parent: i64) -> i64 {
    sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('child', $1) RETURNING id")
        .bind(parent)
        .fetch_one(pool)
        .await
        .expect("a child row")
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_root_is_offered_start_agent_and_a_child_is_not(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = bare_child(&pool, root).await;
    let root_tools = crate::anthropic::tools::tool_definitions(&pool, root).await;
    assert!(names(&root_tools).contains(&"start_agent"), "{:?}", names(&root_tools));
    let child_tools = crate::anthropic::tools::tool_definitions(&pool, child).await;
    assert!(!names(&child_tools).contains(&"start_agent"), "{:?}", names(&child_tools));
}

/// Checked from the database at the call, not from what was offered: a
/// call can come from history written before the conversation changed.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_start_agent_called_by_a_child_is_refused_without_running(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = bare_child(&pool, root).await;
    let result =
        crate::anthropic::tools::execute(&pool, child, "toolu_1", "start_agent", &serde_json::json!({"brief": "go"})).await;
    assert_eq!(result, Err("start_agent is only for a conversation without a parent".to_string()));
    let grandchildren = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM conversations WHERE parent_id = $1")
        .bind(child)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(grandchildren, 0, "the refused call made a conversation");
}
