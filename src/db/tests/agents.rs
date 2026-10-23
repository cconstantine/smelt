//! `agents` and `agent_messages` (SME-236).

use super::*;

/// A wake finds work in an undelivered report or notice, as in a finished
/// command (SME-218): otherwise a report queued to an idle parent would
/// never be drained.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_wake_has_work_sees_an_undelivered_agent_message(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    assert!(!wake_has_work(&pool, conversation.id).await.expect("wake_has_work"), "nothing queued yet");
    let mut conn = pool.acquire().await.expect("a connection");
    queue_agent_message(&mut conn, conversation.id, None, "agent #9", KIND_REPORT, "done")
        .await
        .expect("queue a report");
    assert!(wake_has_work(&pool, conversation.id).await.expect("wake_has_work"), "an undelivered report is work");
    sqlx::query("UPDATE agent_messages SET delivered_at = now()")
        .execute(&pool)
        .await
        .expect("mark it delivered");
    assert!(!wake_has_work(&pool, conversation.id).await.expect("wake_has_work"), "a delivered one isn't");
}
