//! The order queued turns take the turn lock in (SME-202): a turn takes
//! its place in the lock's queue when it's admitted, and a Stop that
//! catches it queued keeps that place, so queued messages are saved, and
//! queued turns run, in the order they were admitted. `#[sqlx::test]`
//! runs on a current-thread runtime, so a task the code under test spawns
//! isn't polled until the test awaits: each test names the order the
//! tasks first run in, and the test hooks pin the rest.

use super::*;

/// Lets every ready task run until it waits.
async fn let_tasks_run() {
    for _ in 0..5 {
        tokio::task::yield_now().await;
    }
}

/// Waits (up to 5 s) for a test hook to say it's reached.
async fn reached(hook: tokio::sync::oneshot::Receiver<()>, what: &str) {
    tokio::time::timeout(std::time::Duration::from_secs(5), hook)
        .await
        .unwrap_or_else(|_| panic!("{what} within 5 s"))
        .expect("hook");
}

/// Where each of `texts` is among `conversation_id`'s saved messages, and
/// that each is saved once.
async fn saved_positions(pool: &PgPool, conversation_id: i64, texts: &[&str]) -> Vec<usize> {
    let saved = db::list_messages(pool, conversation_id).await.expect("list");
    texts
        .iter()
        .map(|text| {
            let quoted = format!("\"{text}\"");
            let at: Vec<usize> = saved
                .iter()
                .enumerate()
                .filter(|(_, m)| m.content.contains(&quoted))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(at.len(), 1, "{text:?} is saved once: {saved:?}");
            at[0]
        })
        .collect()
}

/// T1, the ticket's case: two sends, a Stop catching both queued, the
/// Stop landing before the first send's task has first run (it's paused
/// right after it's counted, and the second's task has already reached
/// its `select!`). The first is saved first, and it, not the second,
/// dismisses the question waiting. Before SME-202 the second, which
/// polled the lock first, was saved first.
#[sqlx::test]
async fn test_two_sends_a_stop_catches_queued_are_saved_in_order(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    let requests =
        start_recording_mock_upstream(&pool, vec![super::ask_user::ask_body(), text_reply_body("unreached")]).await;
    run_turn(&pool, conversation_id, hello()).await.expect("asks");
    let mut rx = events::subscribe(conversation_id);
    let held = conversation_lock(conversation_id).lock_owned().await;
    let (counted, release_one) = test_hooks::pause_after_turn_counted(conversation_id);

    start_turn(pool.clone(), conversation_id, "first message".to_string()).await.expect("the first send");
    reached(counted, "the first send's task runs").await;
    start_turn(pool.clone(), conversation_id, "second message".to_string()).await.expect("the second send");
    let_tasks_run().await;
    stop_turn_now(conversation_id);
    let_tasks_run().await;
    release_one.send(()).expect("the first send waits at the hook");
    let_tasks_run().await;
    drop(held);
    wait_until_idle(&mut rx).await;

    assert_eq!(requests.lock().expect("the request log").len(), 1, "a stopped send called the model");
    let at = saved_positions(&pool, conversation_id, &["first message", "second message"]).await;
    assert!(at[0] < at[1], "the first send is saved first: {at:?}");
    let saved = db::list_messages(&pool, conversation_id).await.expect("list");
    assert_eq!(saved.iter().filter(|m| m.content.contains(STOP_NOTICE)).count(), 1, "the stop once: {saved:?}");
    assert!(
        saved[at[0]].content.contains(crate::questions::NOT_ANSWERED),
        "the first send dismisses the question: {saved:?}"
    );
    assert!(!saved[at[1]].content.contains(crate::questions::NOT_ANSWERED), "{saved:?}");
    resume_turns(conversation_id);
}

/// T1b, the ticket's forced wake order: both sends' tasks have reached
/// their `select!` in send order, and the Stop's wake runs the second's
/// stop arm to the lock while the first's is held in its own. The first
/// is still saved first. Before SME-202 a stopped turn dropped its place
/// and queued again, so the second went ahead of the first.
#[sqlx::test]
async fn test_a_stop_arm_running_out_of_order_keeps_send_order(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("unreached")]).await;
    let mut rx = events::subscribe(conversation_id);
    let held = conversation_lock(conversation_id).lock_owned().await;

    start_turn(pool.clone(), conversation_id, "first message".to_string()).await.expect("the first send");
    start_turn(pool.clone(), conversation_id, "second message".to_string()).await.expect("the second send");
    let_tasks_run().await;
    assert_eq!(turns_in_flight(conversation_id), 2);
    let (one_stopped, release_one) = test_hooks::pause_stopped_while_queued(conversation_id, "first message");
    let (two_stopped, release_two) = test_hooks::pause_stopped_while_queued(conversation_id, "second message");
    stop_turn_now(conversation_id);
    reached(one_stopped, "the first send's stop arm").await;
    reached(two_stopped, "the second send's stop arm").await;
    // The second goes on to the lock first.
    release_two.send(()).expect("the second send waits at the hook");
    let_tasks_run().await;
    release_one.send(()).expect("the first send waits at the hook");
    let_tasks_run().await;
    drop(held);
    wait_until_idle(&mut rx).await;

    assert!(requests.lock().expect("the request log").is_empty(), "a stopped send called the model");
    let at = saved_positions(&pool, conversation_id, &["first message", "second message"]).await;
    assert!(at[0] < at[1], "the first send is saved first: {at:?}");
    resume_turns(conversation_id);
}

/// T2, with no Stop: a delivered notice admitted before a send runs
/// before it, though the send's task reaches the lock first. Before
/// SME-202 turns joined the lock's queue when their task first ran.
#[sqlx::test]
async fn test_turns_admitted_back_to_back_queue_in_admission_order(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    let requests =
        start_recording_mock_upstream(&pool, vec![text_reply_body("Noted."), text_reply_body("Hi!")]).await;
    let mut rx = events::subscribe(conversation_id);
    let held = conversation_lock(conversation_id).lock_owned().await;
    let (admitted, release) = test_hooks::pause_after_notice_admitted(conversation_id);
    let notice = "Docker in your sandbox was restarted.";

    let delivery = tokio::spawn({
        let pool = pool.clone();
        async move { deliver_notice(&pool, conversation_id, notice.to_string()).await }
    });
    reached(admitted, "the notice is admitted").await;
    start_turn(pool.clone(), conversation_id, "B".to_string()).await.expect("the send");
    let_tasks_run().await;
    release.send(()).expect("the notice waits at the hook");
    let_tasks_run().await;
    drop(held);
    wait_until_idle(&mut rx).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), delivery)
        .await
        .expect("the delivery ends within 10 s")
        .expect("join");

    let at = saved_positions(&pool, conversation_id, &[notice, "B"]).await;
    assert!(at[0] < at[1], "the notice, admitted first, is saved first: {at:?}");
    let requests = requests.lock().expect("the request log").clone();
    assert_eq!(requests.len(), 2, "{requests:?}");
    let last_text = |request: &serde_json::Value| {
        let messages = request["messages"].as_array().expect("messages");
        messages.last().expect("a message")["content"].to_string()
    };
    assert!(last_text(&requests[0]).contains(notice), "the first call is the notice's: {}", requests[0]);
    assert!(last_text(&requests[1]).contains("\"B\""), "the second call is the send's: {}", requests[1]);
}

/// T3, a guard: a turn's place in the queue ends with the turn, whether
/// it's dropped before it runs or its task is aborted while it waits.
/// (a) A user's turn admitted with the lock free holds it from its
/// admission; dropping it frees the lock, and leaves no turn state.
/// (b) A send whose task is aborted before it first reaches the lock
/// lets a later send run.
#[sqlx::test]
async fn test_a_turn_dropped_before_it_runs_frees_its_place(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    let requests = start_recording_mock_upstream(&pool, vec![text_reply_body("Hi!")]).await;

    let (turn, _) = TurnInFlight::start_by_user(conversation_id);
    assert!(conversation_lock(conversation_id).try_lock().is_err(), "the admitted turn holds the lock");
    drop(turn);
    forget_turn_generation(conversation_id);
    assert!(!has_turn_state(conversation_id), "the dropped turn left turn state");

    let (counted, _release) = test_hooks::pause_after_turn_counted(conversation_id);
    let (in_flight, generation) = TurnInFlight::start_by_user(conversation_id);
    let aborted = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn_bounded(&pool, in_flight, Some(hello()), MAX_TURNS, Some(generation), true).await }
    });
    reached(counted, "the first send's task runs").await;
    aborted.abort();
    assert!(aborted.await.expect_err("aborted").is_cancelled());
    // Subscribed after the abort, so only the later send's end is seen.
    let mut rx = events::subscribe(conversation_id);
    start_turn(pool.clone(), conversation_id, "later".to_string()).await.expect("the later send");
    wait_until_idle(&mut rx).await;

    assert_eq!(requests.lock().expect("the request log").len(), 1, "the later send called the model");
    saved_positions(&pool, conversation_id, &["later"]).await;
}
