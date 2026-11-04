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

// --- A wake after the notification leaves it standing (SME-386 N1) ---

/// Runs an idle-notice wake on `conversation_id` (a command running
/// untouched past its interval), and at the moment the wake is admitted
/// — the `TurnsChanged` the notifier revalidates its open notifications
/// on — asks `get_attention` about `(attention, generation)`; then lets
/// the wake finish and asks again. Returns both answers.
async fn ask_around_a_wake(
    pool: &PgPool,
    conversation_id: i64,
    attention: events::Attention,
    generation: u64,
) -> (
    Option<crate::api::notifications::AttentionDetails>,
    Option<crate::api::notifications::AttentionDetails>,
) {
    untouched_running_command(pool, conversation_id, "cmd-idle", 601).await;
    let (reached, release) = test_hooks::pause_after_notice_admitted(conversation_id);
    let wake = tokio::spawn({
        let pool = pool.clone();
        async move { wake_conversation(&pool, conversation_id).await.map(|_| ()) }
    });
    tokio::time::timeout(Duration::from_secs(10), reached)
        .await
        .expect("the wake was admitted within 10 s")
        .expect("the hook fired");
    assert!(turn_running(conversation_id), "the wake counts as a running turn here");
    let during = crate::api::notifications::attention_details(pool, conversation_id, generation, attention)
        .await
        .expect("get_attention during the wake");
    let _ = release.send(());
    tokio::time::timeout(Duration::from_secs(10), wake)
        .await
        .expect("the wake ended within 10 s")
        .expect("the wake's task")
        .expect("the wake");
    let after = crate::api::notifications::attention_details(pool, conversation_id, generation, attention)
        .await
        .expect("get_attention after the wake");
    (during, after)
}

/// N1: the model said it's done with `notify_user`, and then a command's
/// idle notice woke it. Nothing the user did or was told changed, so the
/// notification stands: `get_attention` keeps answering `Some` with the
/// model's message, while the wake runs (when the notifier revalidates
/// on its `TurnsChanged`) and after it, and the wake publishes nothing
/// new. (Red before the fix: `None` during, since a turn was running,
/// and `None` after, since the wake took a newer generation.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_after_notify_user_leaves_the_notification_standing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_mock_upstream(&pool, vec![notify_body("Built and tested"), done_body("Still running.")]).await;
    let recording = events::AppRecording::start();
    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    let (attention, generation) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Notified);
    wait_turn_done(conversation.id).await;

    let (during, after) = ask_around_a_wake(&pool, conversation.id, attention, generation).await;
    assert_eq!(during.and_then(|d| d.message), Some("Built and tested".to_string()), "standing while the wake runs");
    assert_eq!(after.and_then(|d| d.message), Some("Built and tested".to_string()), "standing after the wake");
    let seen = published_after_done(&recording, conversation.id).await;
    assert_eq!(seen, vec![(attention, generation)], "the wake published nothing of its own");
    forget_conversation_lock(conversation.id);
}

/// N1's Question variant: a question waits on the user, and a command's
/// idle notice wakes the conversation (the turn saves the notice and
/// waits on the answer). The question still waits, so its notification
/// stands through the wake and after it. (Red before the fix: `None`
/// during, since a turn was running.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_while_a_question_waits_leaves_the_notification_standing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_mock_upstream(&pool, vec![ask_body(), done_body("Still running.")]).await;
    let recording = events::AppRecording::start();
    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    let (attention, generation) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Question);
    wait_turn_done(conversation.id).await;

    let (during, after) = ask_around_a_wake(&pool, conversation.id, attention, generation).await;
    assert_eq!(during.and_then(|d| d.question_header), Some("Which".to_string()), "standing while the wake runs");
    assert_eq!(after.and_then(|d| d.question_header), Some("Which".to_string()), "standing after the wake");
    forget_conversation_lock(conversation.id);
}

/// Review round 1, L1 (SME-386): the user's send queued behind a running
/// wake clears what was published, and tells the notifier to revalidate
/// then — not only at the wake's end — since a queued admission publishes
/// no `TurnsChanged` of its own. (Red before the fix: no app event between
/// the send and the wake's release.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_send_queued_behind_a_wake_revalidates_at_once(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_mock_upstream(&pool, vec![notify_body("Built"), done_body("Ok.")]).await;
    let recording = events::AppRecording::start();
    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    let (attention, generation) = wait_needs_user(&recording, conversation.id).await;
    wait_turn_done(conversation.id).await;

    untouched_running_command(&pool, conversation.id, "cmd-idle", 601).await;
    let (reached, release) = test_hooks::pause_after_notice_admitted(conversation.id);
    let wake = tokio::spawn({
        let pool = pool.clone();
        async move { wake_conversation(&pool, conversation.id).await.map(|_| ()) }
    });
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the wake was admitted").expect("the hook fired");
    let before = recording.events().len();
    start_turn(pool.clone(), conversation.id, "and now?".to_string()).await.expect("the queued send");
    let after_send: Vec<_> = recording.events().into_iter().skip(before).collect();
    let revalidates = after_send
        .iter()
        .any(|e| matches!(e, events::AppEvent::QuestionsChanged | events::AppEvent::TurnsChanged));
    assert_eq!(
        crate::api::notifications::attention_details(&pool, conversation.id, generation, attention).await.expect("query"),
        None,
        "the user's send made the notification stale"
    );
    assert!(revalidates, "the notifier was told to revalidate at the send, saw {after_send:?}");
    let _ = release.send(());
    tokio::time::timeout(Duration::from_secs(10), wake).await.expect("the wake ended").expect("its task").expect("the wake");
    wait_turn_done(conversation.id).await;
    forget_conversation_lock(conversation.id);
}
