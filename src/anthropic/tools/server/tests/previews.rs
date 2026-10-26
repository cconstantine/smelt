//! `sandbox_preview_url`: the ports, hosts and pods it will and won't name.

use super::*;

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_sandbox_preview_url_without_a_pod_says_to_create_one(pool: sqlx::PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let message = sandbox_preview_url_tool(&pool, conversation.id, &serde_json::json!({"port": 3000}))
        .await
        .expect_err("no pod, no preview");
    assert!(message.contains("create_pod"), "should say how to get a sandbox: {message}");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_sandbox_preview_url_refuses_the_sandbox_agents_port(pool: sqlx::PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let message = sandbox_preview_url_tool(&pool, conversation.id, &serde_json::json!({"port": 8088}))
        .await
        .expect_err("the agent's port is never previewed");
    assert!(message.contains("sandbox agent"), "{message}");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_sandbox_preview_url_refuses_a_host_that_is_not_a_container_in_the_pod(pool: sqlx::PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    for host in ["10.0.0.5", "172.24.0.1", "web", "localhost", ""] {
        let message = sandbox_preview_url_tool(
            &pool,
            conversation.id,
            &serde_json::json!({"port": 3000, "host": host}),
        )
        .await
        .expect_err("not a container address");
        assert!(message.contains("docker inspect"), "{host:?}: {message}");
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_sandbox_preview_url_refuses_a_port_that_is_not_a_port(pool: sqlx::PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    for input in [
        serde_json::json!({}),
        serde_json::json!({"port": 0}),
        serde_json::json!({"port": 70000}),
        serde_json::json!({"port": "3000"}),
        serde_json::json!({"port": -1}),
    ] {
        let message = sandbox_preview_url_tool(&pool, conversation.id, &input)
            .await
            .expect_err("not a port");
        assert!(message.contains("1 to 65535"), "{input}: {message}");
    }
}

