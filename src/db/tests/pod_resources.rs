//! SME-484: a conversation's pod resources, which every pod start reads.

use super::*;

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_conversations_pod_resources_are_none_until_set_and_can_be_cleared(pool: PgPool) {
    let id = create_conversation(&pool).await.expect("conversation").id;
    assert_eq!(conversation_pod_resources(&pool, id).await.expect("read"), None);
    let asks = serde_json::json!({"docker": {"memory_request": "12Gi"}});
    set_conversation_pod_resources(&pool, id, Some(&asks)).await.expect("set");
    assert_eq!(conversation_pod_resources(&pool, id).await.expect("read"), Some(asks));
    let other = create_conversation(&pool).await.expect("another").id;
    assert_eq!(conversation_pod_resources(&pool, other).await.expect("read"), None, "only its own row");
    set_conversation_pod_resources(&pool, id, None).await.expect("clear");
    assert_eq!(conversation_pod_resources(&pool, id).await.expect("read"), None);
}
