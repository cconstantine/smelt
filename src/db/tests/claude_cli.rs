use super::*;

/// A conversation's Claude Code session: set, replaced, read back,
/// deleted, and gone with its conversation (SME-430).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_claude_cli_session_round_trips_and_goes_with_its_conversation(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = create_conversation(&pool).await.expect("conversation");
    let provider = create_inference_provider(&pool, "Claude", "claude_cli", "claude", "api_key", "", false, None, true, None).await.expect("provider");
    assert_eq!(get_claude_cli_session(&pool, conversation.id).await.expect("read"), None);
    let first = ClaudeCliSession { provider_id: provider.id, session_id: "s-1".into(), synced_count: 1, synced_hash: "h1".into() };
    set_claude_cli_session(&pool, conversation.id, &first).await.expect("set");
    let second = ClaudeCliSession { synced_count: 3, synced_hash: "h3".into(), ..first.clone() };
    set_claude_cli_session(&pool, conversation.id, &second).await.expect("replace");
    assert_eq!(get_claude_cli_session(&pool, conversation.id).await.expect("read"), Some(second.clone()));
    delete_claude_cli_session(&pool, conversation.id).await.expect("delete");
    assert_eq!(get_claude_cli_session(&pool, conversation.id).await.expect("read"), None);
    set_claude_cli_session(&pool, conversation.id, &second).await.expect("set again");
    delete_conversation(&pool, conversation.id).await.expect("delete the conversation");
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM claude_cli_sessions WHERE conversation_id = $1").bind(conversation.id).fetch_one(&pool).await.expect("count");
    assert_eq!(left, 0, "the row goes with its conversation");
}
