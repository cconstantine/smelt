//! `notify_user` (SME-377) in the busy spell: the last call's message
//! wins, a question outranks it, and a Stop clears it. The single-call
//! case, with its ordering checks, is `mod.rs`'s first test.

use super::*;

/// T4: two `notify_user` calls in one busy spell (two turns, a counted
/// turn holding the spell open between them) publish one event, and the
/// message kept for it is the last call's.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_two_notify_user_calls_in_one_spell_publish_once_with_the_last_message(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_mock_upstream(&pool, vec![notify_body("first"), notify_body("second")]).await;
    let recording = events::AppRecording::start();

    let spell = TurnInFlight::start(conversation.id);
    run_turn(&pool, conversation.id, hello()).await.expect("the first turn");
    run_turn(&pool, conversation.id, hello()).await.expect("the second turn");
    assert!(needs_user_of(&recording, conversation.id).is_empty(), "nothing while the spell runs");
    drop(spell);

    let generation = turn_generation(conversation.id);
    assert_eq!(needs_user_of(&recording, conversation.id), vec![(events::Attention::Notified, generation)]);
    assert_eq!(published_notice(conversation.id, generation), Some("second".to_string()), "the last call wins");
}

/// T5's second half: a reply that asks the user and notifies in one go
/// ends on the question, and the spell publishes Question, which
/// outranks the Notified; the message is kept behind it (M1).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_ask_user_and_notify_user_in_one_reply_publish_question(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let ask = serde_json::json!({"questions": [{"question": "Which one?", "header": "Which", "options": []}]});
    let both = super::super::ask_user::tool_calls_body(&[
        ("toolu_ask", "ask_user", ask),
        ("toolu_notify", "notify_user", serde_json::json!({"message": "Look at this"})),
    ]);
    start_mock_upstream(&pool, vec![both, done_body("After.")]).await;
    let recording = events::AppRecording::start();

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    let seen = published_after_done(&recording, conversation.id).await;
    let generation = turn_generation(conversation.id);
    assert_eq!(seen, vec![(events::Attention::Question, generation)], "the question outranks the notification");
    assert_eq!(published_notice(conversation.id, generation), Some("Look at this".to_string()), "kept behind the question");
}

/// T6: a Stop after the note (`clear_attention`, which a stopped turn's
/// end calls) takes the message with it: nothing is published, and no
/// notice is kept for `get_attention`.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_after_notify_user_publishes_nothing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let recording = events::AppRecording::start();

    let turn = TurnInFlight::start(conversation.id);
    note_notified(conversation.id, "Done".to_string());
    clear_attention(conversation.id);
    drop(turn);

    assert!(needs_user_of(&recording, conversation.id).is_empty(), "a Stop clears the spell's debt");
    assert_eq!(published_notice(conversation.id, turn_generation(conversation.id)), None);
}
