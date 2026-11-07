//! SME-431's pieces of compaction on a Claude Code provider: the session
//! reset after a compaction, and how the one-shot call's failures map.
//! The turns that compact are `turn::tests::claude_cli::compaction`.

use serde_json::Value;

use super::fake::fixture_lines;

/// The `result` line of fixture `name`.
fn result_of(name: &str) -> Value {
    fixture_lines(name).into_iter().find(|l| l["type"] == "result").expect("a result line")
}

/// An overflow `result` (the synthesised fixture) is an overflow; any other
/// error is the CLI's mapped message.
#[test]
fn test_an_overflow_result_maps_to_an_overflow() {
    let overflow = crate::claude_cli::once::result_failure(&result_of("overflow"), None);
    assert!(matches!(overflow, Some(crate::anthropic::stream::ModelError::Overflow(_))), "{overflow:?}");
    let other = crate::claude_cli::once::result_failure(&result_of("bad_model"), None);
    assert!(matches!(other, Some(crate::anthropic::stream::ModelError::Other(_))), "{other:?}");
    let fine = super::fake::result_ok();
    assert!(crate::claude_cli::once::result_failure(&fine, None).is_none());
}

/// The reset deletes the conversation's session row at once, marks its run
/// to end, and keeps the replaced session for its files to go; on another
/// kind of provider it does nothing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reset_deletes_the_session_and_marks_the_run(pool: sqlx::PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = crate::db::create_conversation(&pool).await.expect("conversation");
    let provider = crate::db::create_inference_provider(&pool, "Claude reset", "claude_cli", "claude", "api_key", "", false, Some("anthropic"), true, None)
        .await
        .expect("a provider");
    let session = crate::db::ClaudeCliSession { provider_id: provider.id, session_id: "00000000-0000-4000-8000-0000000000aa".to_string(), synced_count: 2, synced_hash: "h".to_string() };
    crate::db::set_claude_cli_session(&pool, conversation.id, &session).await.expect("a session");
    use crate::claude_cli::session::{reset, take_reset, take_retired};
    use crate::providers::ProviderKind;

    reset(&pool, conversation.id, ProviderKind::Anthropic).await;
    assert!(crate::db::get_claude_cli_session(&pool, conversation.id).await.expect("read").is_some(), "another kind's compaction leaves it");
    assert!(!take_reset(conversation.id));

    reset(&pool, conversation.id, ProviderKind::ClaudeCli).await;
    assert!(crate::db::get_claude_cli_session(&pool, conversation.id).await.expect("read").is_none(), "the row goes at once");
    assert!(take_reset(conversation.id), "the run is marked to end");
    assert!(!take_reset(conversation.id), "once");
    assert_eq!(take_retired(conversation.id), vec![session.session_id]);
    crate::claude_cli::session::forget(conversation.id);
}

/// SME-444's delivered mark: noted, taken once, and cleared by a reset
/// and by the conversation's delete; never another conversation's.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_delivered_mark_is_taken_once_and_cleared_by_reset_and_forget(pool: sqlx::PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = crate::db::create_conversation(&pool).await.expect("conversation");
    let other = crate::db::create_conversation(&pool).await.expect("another conversation");
    use crate::claude_cli::session::{forget, note_delivered, reset, take_delivered};
    let ids = |list: &[&str]| list.iter().map(|id| id.to_string()).collect::<std::collections::HashSet<_>>();

    note_delivered(conversation.id, ["toolu_a".to_string()]);
    note_delivered(conversation.id, ["toolu_b".to_string()]);
    assert!(take_delivered(other.id).is_empty(), "another conversation's");
    assert_eq!(take_delivered(conversation.id), ids(&["toolu_a", "toolu_b"]));
    assert!(take_delivered(conversation.id).is_empty(), "taken once");

    note_delivered(conversation.id, ["toolu_c".to_string()]);
    reset(&pool, conversation.id, crate::providers::ProviderKind::ClaudeCli).await;
    assert!(take_delivered(conversation.id).is_empty(), "a reset clears it");

    note_delivered(conversation.id, ["toolu_d".to_string()]);
    forget(conversation.id);
    assert!(take_delivered(conversation.id).is_empty(), "a delete clears it");
    forget(other.id);
}
