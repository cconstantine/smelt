use super::*;
use crate::anthropic::ContentBlock;
use crate::db;
use crate::questions::Question;
use crate::turn::tests::start_partial_then_hanging_mock_upstream;
use crate::turn::{self, new_turn_generation, stop_turn_now, turn_generation, turn_running};
use sqlx::PgPool;
use std::time::Duration;

/// Every test in this file reads process-wide turn state by conversation
/// id, and every `#[sqlx::test]` database numbers from 1: the turn tests'
/// own discipline (shared lock, ids clear of other runs) applies here too.
async fn ids_clear_and_locked(pool: &PgPool) -> std::sync::MutexGuard<'static, ()> {
    crate::db::test_support::start_ids_clear_of_other_runs(pool).await.expect("ids clear of other runs");
    crate::providers::test_support::lock_turn_tests()
}

/// `conversation_id` waits on a two-question card asked of the user.
async fn ask_two(pool: &PgPool, conversation_id: i64) {
    let mut conn = pool.acquire().await.expect("connection");
    db::upsert_pending_question(
        &mut conn,
        conversation_id,
        "toolu_q",
        &[
            Question { question: "Which one?".into(), header: "Which".into(), options: vec![], multi_select: false },
            Question { question: "And how?".into(), header: "How".into(), options: vec![], multi_select: false },
        ],
        db::ASKED_OF_USER,
    )
    .await
    .expect("ask");
}

/// Renames it, so a title assertion reads the conversation's own words.
async fn rename(pool: &PgPool, conversation_id: i64, title: &str) {
    sqlx::query("UPDATE conversations SET title = $2 WHERE id = $1")
        .bind(conversation_id)
        .bind(title)
        .execute(pool)
        .await
        .expect("rename");
}

/// Waits (up to 10 s) for `conversation_id` to look idle again.
async fn wait_turn_done(conversation_id: i64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while turn_running(conversation_id) {
        assert!(std::time::Instant::now() < deadline, "the turn never ended");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A fresh question event answers `Some`: the title, and the first
/// question's header — the notification's whole body.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_get_attention_answers_some_for_a_fresh_question_with_title_and_header(pool: PgPool) {
    let _guard = ids_clear_and_locked(&pool).await;
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    rename(&pool, conversation.id, "rename the flag").await;
    ask_two(&pool, conversation.id).await;
    let details = attention_details(&pool, conversation.id, 0, Attention::Question).await.expect("query");
    assert_eq!(
        details,
        Some(AttentionDetails {
            title: "rename the flag".into(),
            attention: Attention::Question,
            question_header: Some("Which".into()),
            message: None,
        })
    );
}

/// The spell `conversation_id` runs now ends having called `notify_user`
/// with `message`, as a turn's call would note it (SME-377). Returns the
/// generation its `NeedsUser` carried.
fn notified_spell(conversation_id: i64, message: &str) -> u64 {
    let turn = crate::turn::count_a_test_turn(conversation_id);
    crate::turn::note_notified(conversation_id, message.to_string());
    drop(turn);
    turn_generation(conversation_id)
}

/// T10 (SME-377): a Notified whose spell published it answers `Some` with
/// the title and the model's message — a waiting question stays out of
/// it; once the generation moves on, `None`.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_get_attention_answers_some_for_notified_with_the_models_message(pool: PgPool) {
    let _guard = ids_clear_and_locked(&pool).await;
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    rename(&pool, conversation.id, "fix the flaky build").await;
    ask_two(&pool, conversation.id).await;
    let generation = notified_spell(conversation.id, "Built and tested");
    let details = attention_details(&pool, conversation.id, generation, Attention::Notified).await.expect("query");
    assert_eq!(
        details,
        Some(AttentionDetails {
            title: "fix the flaky build".into(),
            attention: Attention::Notified,
            question_header: None,
            message: Some("Built and tested".into()),
        })
    );
    let moved = new_turn_generation(conversation.id);
    assert_eq!(
        attention_details(&pool, conversation.id, moved, Attention::Notified).await.expect("query"),
        None,
        "the notice was for the old generation"
    );
    turn::forget_conversation_lock(conversation.id);
}

/// A Notified with no notice published for it (a restart, or a forged
/// event) answers `None`, even with every other check passing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_get_attention_answers_none_for_notified_without_a_published_notice(pool: PgPool) {
    let _guard = ids_clear_and_locked(&pool).await;
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let details = attention_details(&pool, conversation.id, 0, Attention::Notified).await.expect("query");
    assert_eq!(details, None, "nothing the model said is held for it");
}

/// An event from an older generation is stale news, even while everything
/// else about it still holds; the current generation answers `Some`.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_get_attention_answers_none_for_a_stale_generation(pool: PgPool) {
    let _guard = ids_clear_and_locked(&pool).await;
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let generation = new_turn_generation(conversation.id);
    assert_eq!(
        attention_details(&pool, conversation.id, generation - 1, Attention::Failed).await.expect("query"),
        None,
        "the user wrote since the event's turn: stale"
    );
    assert!(
        attention_details(&pool, conversation.id, generation, Attention::Failed).await.expect("query").is_some(),
        "the same event, current, still notifies"
    );
}

/// While a turn runs, no event is worth a notification: the plan's
/// between-publish-and-check race, driven by a real held turn.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_get_attention_answers_none_while_a_turn_runs(pool: PgPool) {
    let _guard = ids_clear_and_locked(&pool).await;
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_partial_then_hanging_mock_upstream(&pool, "streaming…").await;
    turn::start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(turn_running(conversation.id), "the mock holds the turn mid-stream");
    let details = attention_details(&pool, conversation.id, turn_generation(conversation.id), Attention::Failed).await.expect("query");
    assert_eq!(details, None, "a running turn is never 'needs the user'");
    stop_turn_now(conversation.id);
    wait_turn_done(conversation.id).await;
}

/// A question answered — here straight on the row, as the card's answer
/// would land it — is no one's notification any more.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_get_attention_answers_none_for_an_answered_question(pool: PgPool) {
    let _guard = ids_clear_and_locked(&pool).await;
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    ask_two(&pool, conversation.id).await;
    let answered = db::answer_pending_question(
        &pool,
        conversation.id,
        "toolu_q",
        &[crate::questions::QuestionAnswer::default(), crate::questions::QuestionAnswer::default()],
    )
    .await
    .expect("answer query");
    assert!(answered, "the row was waiting");
    let details = attention_details(&pool, conversation.id, 0, Attention::Question).await.expect("query");
    assert_eq!(details, None, "an answered question drops out");
}

/// A question the user dismissed by writing a message instead: the
/// message took the row, and the event with it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_get_attention_answers_none_for_a_question_the_user_dismissed(pool: PgPool) {
    let _guard = ids_clear_and_locked(&pool).await;
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    ask_two(&pool, conversation.id).await;
    let saved = db::create_message_taking_question(
        &pool,
        conversation.id,
        "user",
        vec![ContentBlock::Text { text: "just build it".into() }],
        true,
        |_| ContentBlock::Text { text: "(dismissed)".into() },
    )
    .await
    .expect("message query")
    .expect("the message was saved");
    assert!(saved.1.is_some(), "the waiting question was taken with it");
    let details = attention_details(&pool, conversation.id, 0, Attention::Question).await.expect("query");
    assert_eq!(details, None, "a dismissed question drops out too");
}

/// A row asked of the child's parent (`ask_parent`, SME-238) waits, but
/// no card can answer it: it never notifies (Revision 6's belt).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_get_attention_answers_none_for_a_question_asked_of_the_parent(pool: PgPool) {
    let _guard = ids_clear_and_locked(&pool).await;
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut conn = pool.acquire().await.expect("connection");
    db::upsert_pending_question(
        &mut conn,
        conversation.id,
        "toolu_p",
        &[Question { question: "Parent?".into(), header: "Parent".into(), options: vec![], multi_select: false }],
        db::ASKED_OF_PARENT,
    )
    .await
    .expect("ask the parent");
    let details = attention_details(&pool, conversation.id, 0, Attention::Question).await.expect("query");
    assert_eq!(details, None, "a parent-row question is no one's card");
}

/// A deleted conversation has no title, so nothing shows.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_get_attention_answers_none_for_a_deleted_conversation(pool: PgPool) {
    let _guard = ids_clear_and_locked(&pool).await;
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    db::delete_conversation(&pool, conversation.id).await.expect("delete");
    let details = attention_details(&pool, conversation.id, 0, Attention::Failed).await.expect("query");
    assert_eq!(details, None, "a conversation that isn't there has no title");
}

/// The delete-racing-a-turn case: the delete happens while the mock
/// upstream holds the turn, and the turn ends after it — whatever that
/// end published, the endpoint's title check answers `None`.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_get_attention_answers_none_for_a_delete_racing_a_turn(pool: PgPool) {
    let _guard = ids_clear_and_locked(&pool).await;
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_partial_then_hanging_mock_upstream(&pool, "streaming…").await;
    turn::start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(turn_running(conversation.id), "the mock holds the turn mid-stream");
    db::delete_conversation(&pool, conversation.id).await.expect("delete while the turn runs");
    stop_turn_now(conversation.id);
    wait_turn_done(conversation.id).await;
    let details = attention_details(&pool, conversation.id, turn_generation(conversation.id), Attention::Failed).await.expect("query");
    assert_eq!(details, None, "the deleted conversation's end notifies nothing");
}
