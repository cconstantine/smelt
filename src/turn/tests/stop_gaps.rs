//! A Stop landing in the gap between a turn's start and its count
//! (SME-216, SME-217): right after `send_message` or `answer_question`
//! returns, and as a wake or a delivery is let in. `#[sqlx::test]` runs
//! on a current-thread runtime, so a task the code under test spawns
//! isn't polled until the test awaits: a Stop made straight after the
//! call returns lands before the task's first step, with no hook.

use super::*;

/// E1: a Stop sent right after `send_message` returns ends that send's
/// turn: no model call, the message kept once, the stop noted once, and
/// the conversation paused. Before SME-216 the turn wasn't counted yet,
/// so the Stop only paused and the turn went on to the model.
#[sqlx::test]
async fn test_a_stop_right_after_a_send_ends_its_turn(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;
    let mut rx = events::subscribe(conversation_id);
    let held = conversation_lock(conversation_id).lock_owned().await;

    start_turn(pool.clone(), conversation_id, "hi".to_string()).await.expect("the send is taken");
    assert!(turn_running(conversation_id), "the send's turn isn't counted when send_message returns");
    stop_turn_now(conversation_id);
    release_after_the_stop_is_seen(held).await;
    wait_until_idle(&mut rx).await;

    assert!(requests.lock().expect("the request log").is_empty(), "the stopped send called the model");
    let saved = db::list_messages(&pool, conversation_id).await.expect("list");
    assert_eq!(saved.len(), 2, "the message, then the stop's note: {saved:?}");
    assert!(saved[0].content.contains("\"hi\""), "{saved:?}");
    assert!(saved[1].content.contains(STOP_NOTICE), "{saved:?}");
    let told: Vec<_> = drain_events(&mut rx).await;
    let stopped = told
        .iter()
        .any(|e| matches!(e, events::ConversationEvent::TurnError { message } if message.contains(TURN_STOPPED)));
    assert!(stopped, "tabs are told the turn stopped: {told:?}");
    assert!(is_paused(conversation_id), "the Stop pauses the conversation");
    resume_turns(conversation_id);
}

/// E1, the card's answer: a Stop sent right after `answer_question`
/// returns ends the answer's turn. The model isn't called again, and the
/// answer stays recorded for the next turn to take.
#[sqlx::test]
async fn test_a_stop_right_after_an_answer_ends_its_turn(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    let requests =
        start_recording_mock_upstream(&pool, vec![super::ask_user::ask_body(), text_reply_body("Deleting.")]).await;
    run_turn(&pool, conversation_id, hello()).await.expect("asks");
    let mut rx = events::subscribe(conversation_id);
    let held = conversation_lock(conversation_id).lock_owned().await;

    let answers =
        vec![crate::questions::QuestionAnswer { selected: vec!["Yes (Recommended)".to_string()], other: None }];
    answer_question(pool.clone(), conversation_id, "toolu_ask".to_string(), answers)
        .await
        .expect("the answer is taken");
    stop_turn_now(conversation_id);
    release_after_the_stop_is_seen(held).await;
    wait_until_idle(&mut rx).await;

    assert_eq!(requests.lock().expect("the request log").len(), 1, "the stopped answer called the model");
    let question = db::get_pending_question(&pool, conversation_id)
        .await
        .expect("read")
        .expect("the question is still there for the next turn");
    assert!(question.answer.is_some(), "the answer stays recorded");
    assert!(is_paused(conversation_id), "the Stop pauses the conversation");
    resume_turns(conversation_id);
}

/// The Delete × queued cell, newly reachable for a just-sent turn: the
/// API's delete (Stop, delete, forget) landing right after
/// `send_message` returns ends with no turn state left. A guard: before
/// SME-216 the turn wasn't stopped, and failed at its exists check.
#[sqlx::test]
async fn test_a_deletes_stop_catching_a_just_sent_turn_leaves_no_turn_state(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;
    let mut rx = events::subscribe(conversation_id);
    let held = conversation_lock(conversation_id).lock_owned().await;

    start_turn(pool.clone(), conversation_id, "hi".to_string()).await.expect("the send is taken");
    // The API's delete, inline: `delete_as_the_api_does` asserts no turn
    // state at once, and this turn is still counted.
    stop_turn_now(conversation_id);
    db::delete_conversation(&pool, conversation_id).await.expect("delete");
    forget_conversation_lock(conversation_id);
    release_after_the_stop_is_seen(held).await;
    wait_until_idle(&mut rx).await;

    assert!(requests.lock().expect("the request log").is_empty(), "a deleted conversation's turn called the model");
    assert!(!has_turn_state(conversation_id), "the turn left turn state");
    drop(rx);
    events::forget(conversation_id);
}

/// E2 (SME-217), interleaving: a Stop lands right after a wake is let in
/// past the pause, with the turn lock held so only the stop can wake it.
/// The wake ends stopped without calling the model, and isn't reported
/// as a failed notification. Before SME-217 the wake checked the pause,
/// then counted in a second step, so the Stop only paused and the wake
/// went on to the model.
#[sqlx::test]
async fn test_a_stop_as_a_wake_is_let_in_ends_it(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    unnotified_finished_command(&pool, conversation_id, &format!("cmd-stop-gap-{conversation_id}")).await;
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Noted.")]).await;
    let mut rx = events::subscribe(conversation_id);
    let held = conversation_lock(conversation_id).lock_owned().await;
    let (admitted, release) = test_hooks::pause_after_notice_admitted(conversation_id);

    let wake = tokio::spawn({
        let pool = pool.clone();
        async move { wake_conversation(&pool, conversation_id).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), admitted)
        .await
        .expect("the wake is let in")
        .expect("hook");
    stop_turn_now(conversation_id);
    release.send(()).expect("the wake waits at the hook");
    release_after_the_stop_is_seen(held).await;

    let result = tokio::time::timeout(std::time::Duration::from_secs(10), wake)
        .await
        .expect("the wake ends within 10 s")
        .expect("join");
    assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");
    assert!(requests.lock().expect("the request log").is_empty(), "the stopped wake called the model");
    let told = drain_events(&mut rx).await;
    let failed = told.iter().any(|e| matches!(e, events::ConversationEvent::NotificationDeliveryFailed { .. }));
    assert!(!failed, "a stop isn't a failed notification: {told:?}");
    resume_turns(conversation_id);
}

/// E2 (SME-217), as the test above with a delivered notice: it's saved
/// once and the stop noted once, without a model call.
#[sqlx::test]
async fn test_a_stop_as_a_delivery_is_let_in_ends_it(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Noted.")]).await;
    let held = conversation_lock(conversation_id).lock_owned().await;
    let (admitted, release) = test_hooks::pause_after_notice_admitted(conversation_id);
    let notice = "Docker in your sandbox was restarted.";

    let delivery = tokio::spawn({
        let pool = pool.clone();
        async move { deliver_notice(&pool, conversation_id, notice.to_string()).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), admitted)
        .await
        .expect("the delivery is let in")
        .expect("hook");
    stop_turn_now(conversation_id);
    release.send(()).expect("the delivery waits at the hook");
    release_after_the_stop_is_seen(held).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), delivery)
        .await
        .expect("the delivery ends within 10 s")
        .expect("join");

    assert!(requests.lock().expect("the request log").is_empty(), "the stopped delivery called the model");
    let saved = db::list_messages(&pool, conversation_id).await.expect("list");
    assert_eq!(saved.iter().filter(|m| m.content.contains(notice)).count(), 1, "the notice once: {saved:?}");
    assert_eq!(saved.iter().filter(|m| m.content.contains(STOP_NOTICE)).count(), 1, "the stop once: {saved:?}");
    resume_turns(conversation_id);
}
