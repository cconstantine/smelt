//! SME-481 R3: a script's MCP call from a top-level conversation set to a
//! persona is ruled by that persona's chain, as the model's own call is.
//! (`tests.rs` is at the size ceiling, so this is a file of its own.)

use super::*;
use crate::mcp::tests::{forget_mock_server, mock_server_for_tests};
use std::sync::atomic::Ordering;

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_script_mcp_call_from_a_top_level_persona_is_ruled(pool: PgPool) {
    sqlx::query("SELECT setval(pg_get_serial_sequence('conversations', 'id'), $1)")
        .bind(crate::id_alloc::FIXTURE_IDS.next_base())
        .execute(&pool)
        .await
        .expect("move the conversation id sequence");
    let (config, calls) = mock_server_for_tests(&pool, "mock-r3").await;
    let conversation = db::create_conversation(&pool).await.expect("conversation").id;
    sqlx::query(
        "INSERT INTO top_level_personas (conversation_id, persona_name, persona_path, persona_body, persona_settings, rule_chain, credentials, applied_at)
         VALUES ($1, 'driver', '/workspace/app/.agents/personas/driver.md', 'Drive.', '{}', $2, '{}', now())",
    )
    .bind(conversation)
    .bind(json!([{"origin": "persona driver", "allow": ["*"], "deny": ["mock-r3/second_tool"]}]))
    .execute(&pool)
    .await
    .expect("a top-level persona");
    let pod = db::create_sandbox_pod(&pool, conversation).await.expect("pod row").id;
    let caller = Caller { conversation_id: conversation, pod_id: pod, command_id: None };
    let answer = handle(&pool, caller, McpRequest::Call { body: json!({"tool": "mock-r3/second_tool"}).to_string() }).await;
    forget_mock_server(config.id).await;
    assert_eq!(answer.status, 403, "{}", answer.body);
    assert!(answer.body.contains("denied") && answer.body.contains("mock-r3/second_tool"), "{}", answer.body);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "a refused call is never sent");
}
