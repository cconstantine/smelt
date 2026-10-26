use super::*;

// --- Per-tool class overrides (mcp_tool_classes, SME-240) ---

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_set_mcp_tool_class_upserts_one_row_per_tool(pool: PgPool) {
    use crate::anthropic::tools::ToolClass;

    let server = create_mcp_server_config(
        &pool, "classes-1", "https://example.com/mcp", &HashMap::new(), "static_headers", None, None,
    )
    .await
    .expect("create mcp server config");

    set_mcp_tool_class(&pool, server.id, "get_pr", ToolClass::ReadOnly)
        .await
        .expect("first set");
    // A tiny pause so the `updated_at` move is visible at Postgres's
    // microsecond resolution.
    tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    set_mcp_tool_class(&pool, server.id, "get_pr", ToolClass::Destructive)
        .await
        .expect("the second set is an update, not a conflict");

    let (count, class, created, updated): (i64, String, NaiveDateTime, NaiveDateTime) =
        sqlx::query_as(
            "SELECT count(*), min(class), min(created_at), max(updated_at) \
             FROM mcp_tool_classes WHERE server_id = $1 AND tool_name = 'get_pr'",
        )
        .bind(server.id)
        .fetch_one(&pool)
        .await
        .expect("read the row back");
    assert_eq!(count, 1, "one row per (server, tool)");
    assert_eq!(class, "destructive", "the latest class wins");
    assert!(updated > created, "the update moves updated_at");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_clear_mcp_tool_class_removes_or_says_no(pool: PgPool) {
    use crate::anthropic::tools::ToolClass;

    let server = create_mcp_server_config(
        &pool, "classes-2", "https://example.com/mcp", &HashMap::new(), "static_headers", None, None,
    )
    .await
    .expect("create mcp server config");
    set_mcp_tool_class(&pool, server.id, "get_pr", ToolClass::Writing)
        .await
        .expect("set an override");

    assert!(
        clear_mcp_tool_class(&pool, server.id, "get_pr").await.expect("clear the override"),
        "clearing an existing row says true"
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mcp_tool_classes WHERE server_id = $1")
        .bind(server.id)
        .fetch_one(&pool)
        .await
        .expect("count the rows");
    assert_eq!(count, 0, "the row is gone");
    assert!(
        !clear_mcp_tool_class(&pool, server.id, "get_pr").await.expect("clear again"),
        "clearing a missing row says false — the page names it to the user"
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_list_mcp_tool_classes_is_per_server(pool: PgPool) {
    use crate::anthropic::tools::ToolClass;

    let a = create_mcp_server_config(
        &pool, "classes-a", "https://example.com/mcp", &HashMap::new(), "static_headers", None, None,
    )
    .await
    .expect("create server a");
    let b = create_mcp_server_config(
        &pool, "classes-b", "https://example.com/mcp", &HashMap::new(), "static_headers", None, None,
    )
    .await
    .expect("create server b");
    set_mcp_tool_class(&pool, a.id, "zeta", ToolClass::ReadOnly).await.expect("set a.zeta");
    set_mcp_tool_class(&pool, a.id, "alpha", ToolClass::Writing).await.expect("set a.alpha");
    set_mcp_tool_class(&pool, b.id, "other", ToolClass::Destructive).await.expect("set b.other");

    let rows = list_mcp_tool_classes(&pool, a.id).await.expect("list a's overrides");
    assert_eq!(rows, vec![
        ("alpha".to_string(), "writing".to_string()),
        ("zeta".to_string(), "read-only".to_string()),
    ], "a's rows only, by tool name");
    let rows = list_mcp_tool_classes(&pool, b.id).await.expect("list b's overrides");
    assert_eq!(rows, vec![("other".to_string(), "destructive".to_string())]);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_deleting_a_server_deletes_its_overrides(pool: PgPool) {
    use crate::anthropic::tools::ToolClass;

    let server = create_mcp_server_config(
        &pool, "classes-gone", "https://example.com/mcp", &HashMap::new(), "static_headers", None, None,
    )
    .await
    .expect("create mcp server config");
    set_mcp_tool_class(&pool, server.id, "get_pr", ToolClass::ReadOnly)
        .await
        .expect("set an override");

    delete_mcp_server_config(&pool, server.id).await.expect("delete the server");
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mcp_tool_classes WHERE server_id = $1")
        .bind(server.id)
        .fetch_one(&pool)
        .await
        .expect("count the rows");
    assert_eq!(count, 0, "the cascade takes the overrides with the server");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_class_check_refuses_an_unknown_class(pool: PgPool) {
    let server = create_mcp_server_config(
        &pool, "classes-bad", "https://example.com/mcp", &HashMap::new(), "static_headers", None, None,
    )
    .await
    .expect("create mcp server config");
    let result = sqlx::query("INSERT INTO mcp_tool_classes (server_id, tool_name, class) VALUES ($1, 'x', 'nope')")
        .bind(server.id)
        .execute(&pool)
        .await;
    assert!(result.is_err(), "the CHECK is the source of truth for valid classes");
}
