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
async fn test_start_agent_is_offered_to_a_root_and_a_child_alike(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = bare_child(&pool, root).await;
    let root_tools = crate::anthropic::tools::tool_definitions(&pool, root).await;
    assert!(names(&root_tools).contains(&"start_agent"), "{:?}", names(&root_tools));
    assert!(!names(&root_tools).contains(&"finish"), "{:?}", names(&root_tools));
    let child_tools = crate::anthropic::tools::tool_definitions(&pool, child).await;
    assert!(names(&child_tools).contains(&"start_agent"), "an ordinary writing tool, SME-241");
    assert!(names(&child_tools).contains(&"finish"), "{:?}", names(&child_tools));
}

/// `finish` from a conversation without a parent is refused by `execute`,
/// before `finish` itself runs.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_finish_called_by_a_root_is_refused(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let result =
        crate::anthropic::tools::execute(&pool, root, "toolu_1", "finish", &serde_json::json!({"report": "done"})).await;
    assert_eq!(result, Err("finish is only for an agent".to_string()));
}

/// `start_agent` is now an ordinary tool, so the only structural limit is
/// the depth ceiling: a conversation with a parent but no `agents` row
/// (only a test artifact — never one smelt started) has unknown depth and
/// is refused, without a child row being made.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_child_with_no_agent_row_is_refused_start_agent_without_running(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = bare_child(&pool, root).await;
    let result =
        crate::anthropic::tools::execute(&pool, child, "toolu_1", "start_agent", &serde_json::json!({"brief": "go"})).await;
    assert_eq!(
        result,
        Err("this conversation has a parent but no agent row, so its depth is unknown — \
            start_agent needs a conversation smelt started"
            .to_string())
    );
    let grandchildren = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM conversations WHERE parent_id = $1")
        .bind(child)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(grandchildren, 0, "the refused call made a conversation");
}
