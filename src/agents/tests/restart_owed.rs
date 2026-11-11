//! A notice a turn saved that no model turn answered before a restart is
//! answered after it, once (SME-450): a wake parked on a held provider
//! slot, a held `Deliver`, a notice saved just before the restart, and a
//! command's end recorded but never drained.

use super::*;

/// `content`'s text as a finished command's notice says it.
pub(super) fn command_notice(command_id: &str) -> String {
    crate::api::chat::mark_notice(&format!("Terminal command {command_id} finished: exit code 0."))
}

/// A command in `conversation_id` that started running (with a 300 s idle
/// interval) and, when `finished`, ended with exit code 0: its end is
/// recorded and its notice not yet saved, as `turn/tests/wake.rs` makes it.
pub(super) async fn command(pool: &PgPool, conversation_id: i64, command_id: &str, finished: bool) {
    let pod = db::create_sandbox_pod(pool, conversation_id).await.expect("create sandbox pod");
    let terminal = db::create_sandbox_terminal(pool, pod.id).await.expect("create sandbox terminal");
    db::create_terminal_command(pool, conversation_id, terminal.id, command_id, "echo hi", 300)
        .await
        .expect("create terminal command");
    if finished {
        db::mark_terminal_command_finished(pool, command_id, 0).await.expect("mark it finished");
    }
}

/// Saves `command_id`'s notice as its drain would, marking it notified.
pub(super) async fn drain_command(pool: &PgPool, conversation_id: i64, command_id: &str) {
    let content = [ContentBlock::Text { text: command_notice(command_id) }];
    db::save_command_notice(pool, conversation_id, command_id, &content)
        .await
        .expect("save the notice")
        .expect("saved");
}

/// How many of `id`'s messages hold `needle`, in any block.
pub(super) async fn messages_holding(pool: &PgPool, id: i64, needle: &str) -> usize {
    db::list_messages(pool, id)
        .await
        .expect("list messages")
        .iter()
        .filter(|m| serde_json::to_string(&m.blocks().expect("valid blocks")).expect("blocks as JSON").contains(needle))
        .count()
}

/// The requests whose last message holds `needle`.
pub(super) fn requests_ending_in(requests: &Mutex<Vec<serde_json::Value>>, needle: &str) -> usize {
    requests
        .lock()
        .expect("the request log")
        .iter()
        .filter(|r| r["messages"].as_array().and_then(|m| m.last()).is_some_and(|last| last.to_string().contains(needle)))
        .count()
}

/// The newest provider: the mock `setup` saved.
pub(super) async fn mock_provider(pool: &PgPool) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT id FROM inference_providers ORDER BY id DESC LIMIT 1")
        .fetch_one(pool)
        .await
        .expect("the mock provider")
}

/// Gives `provider` one request slot, in its row and in its pool, as a
/// one-slot server's provider has (strata), and takes that slot: what a
/// running turn's `TurnSlot` holds.
pub(super) async fn hold_the_one_slot(pool: &PgPool, provider: i64) -> crate::providers::limiter::Slot {
    let version = sqlx::query_scalar::<_, chrono::NaiveDateTime>(
        "UPDATE inference_providers SET max_concurrent_requests = 1 WHERE id = $1 RETURNING updated_at",
    )
    .bind(provider)
    .fetch_one(pool)
    .await
    .expect("one slot");
    crate::providers::limiter::set(provider, version, Some(1));
    crate::providers::limiter::current(provider).expect("the provider's pool").try_acquire().expect("its one slot")
}

/// Ends `task` as a process death ends it: dropped wherever it waits.
pub(super) async fn kill(task: tokio::task::JoinHandle<()>) {
    task.abort();
    let ended = tokio::time::timeout(Duration::from_secs(10), task).await.expect("the task ends");
    assert!(ended.as_ref().is_err_and(|e| e.is_cancelled()), "the task ran to its end: {ended:?}");
}

/// A parent for a test's child, stopped by the user, so the child's turn
/// ending without a report wakes no one (SME-236).
pub(super) async fn quiet_parent(pool: &PgPool) -> i64 {
    let parent = db::create_conversation(pool).await.expect("create conversation").id;
    crate::turn::stop_conversation(pool, parent).await;
    parent
}

/// T1, the ticket's reproduction: a wake that drained its notice and then
/// waited for its provider's one slot, held by another turn, is dropped by
/// the restart. Both conversations, a child (the incident's) and a root,
/// get their turn after it, once.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_waiting_for_a_held_slot_is_answered_after_a_restart(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Noted.")]).await;
    let provider = mock_provider(&pool).await;
    let parent = quiet_parent(&pool).await;
    let child = active_child(&pool, parent).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    command(&pool, child, "t1-child", true).await;
    command(&pool, root, "t1-root", true).await;
    let held = hold_the_one_slot(&pool, provider).await;

    let mut wakes = Vec::new();
    for id in [child, root] {
        let parked = crate::turn::test_hooks::watch_before_turn_slot(id);
        wakes.push(crate::turn::notify(&pool, id, vec![crate::turn::Notice::Wake]));
        tokio::time::timeout(Duration::from_secs(10), parked).await.expect("the wake reaches the slot").expect("the hook");
    }
    let pool_now = crate::providers::limiter::current(provider).expect("the pool");
    assert_eq!(pool_now.available_permits(), 0, "the one slot is held");

    // The mechanism, as traced: each notice is saved and marked, and
    // nothing a restart reads says it's owed an answer.
    for (id, command_id) in [(child, "t1-child"), (root, "t1-root")] {
        assert_eq!(messages_holding(&pool, id, &command_notice(command_id)).await, 1, "#{id}'s notice");
        assert!(db::unnotified_finished_terminal_commands(&pool, id).await.expect("query").is_empty(), "#{id}'s command");
        assert!(!db::wake_has_work(&pool, id).await.expect("wake_has_work"), "#{id} has work");
        assert!(!db::conversations_with_queued_messages(&pool).await.expect("queued").contains(&id), "#{id} is queued");
    }
    assert_eq!(requests.lock().expect("the request log").len(), 0, "a request was made");

    for wake in wakes {
        kill(wake).await;
    }
    drop(held);
    crate::providers::limiter::forget(provider);
    let checked = test_hooks::last_turns_checked(child);
    restart(&pool, &[parent, child, root]).await;

    let child_calls = requests_for(&requests, Some(child));
    assert!(!child_calls.is_empty(), "#{child} wasn't woken");
    let root_calls = requests_for(&requests, None);
    assert!(!root_calls.is_empty(), "#{root} wasn't woken");
    for (id, command_id, calls) in [(child, "t1-child", &child_calls), (root, "t1-root", &root_calls)] {
        let notice = command_notice(command_id);
        let last = calls[0]["messages"].as_array().expect("messages").last().expect("a message").to_string();
        assert!(last.contains(&notice), "#{id}'s request ends {last}");
        let said = transcript(&pool, id).await;
        let at: Vec<usize> = said.iter().enumerate().filter(|(_, (_, text))| text == &notice).map(|(i, _)| i).collect();
        assert_eq!(at.len(), 1, "#{id}'s notice, once: {said:?}");
        assert_eq!(said.get(at[0] + 1), Some(&("assistant".to_string(), "Noted.".to_string())), "#{id}: {said:?}");
    }
    eventually("the child's end-of-turn check ran", || async { test_hooks::last_turns_checked(child) > checked }).await;

    // SME-455: the child's answered turn ended plain and stuck, so it was
    // nudged — wait its spell out (its nudge-woken turn, then the notice
    // its parent reads) before the second restart proves nothing is owed.
    eventually("the child's nudge spell is over (the stopped parent only queues the notice)", || async {
        !crate::turn::turn_running(child)
            && requests_for(&requests, Some(child)).len() == 2
            && sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1 AND kind = 'notice'",
            )
            .bind(parent)
            .fetch_one(&pool)
            .await
            .unwrap_or(0)
                == 1
    })
    .await;
    // The chain is three turns: the two answers and the child's nudge-woken
    // turn. Its parent is the user-stopped one, so the second-end notice
    // only queues. After the spell, the second restart must wake no one.
    restart(&pool, &[parent, child, root]).await;
    assert_eq!(requests.lock().expect("the request log").len(), 3, "a second restart answered again");
    crate::providers::limiter::forget(provider);
    for id in [parent, child, root] {
        crate::turn::forget_conversation_lock(id);
    }
}

/// T3: the process dies right after a notice's save committed, before any
/// wait at all. One root per writer of the mark, each answered once.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_notice_saved_just_before_the_restart_is_answered(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Noted.")]).await;
    let ids: Vec<i64> = {
        let mut ids = Vec::new();
        for _ in 0..4 {
            ids.push(db::create_conversation(&pool).await.expect("create conversation").id);
        }
        ids
    };
    let (by_command, by_idle, by_message, by_answer) = (ids[0], ids[1], ids[2], ids[3]);

    command(&pool, by_command, "t3-ended", true).await;
    drain_command(&pool, by_command, "t3-ended").await;

    command(&pool, by_idle, "t3-idle", false).await;
    sqlx::query("UPDATE terminal_commands SET created_at = now() - make_interval(secs => 7501) WHERE command_id = 't3-idle'")
        .execute(&pool)
        .await
        .expect("an idle command");
    let idle = "T3 idle notice for t3-idle";
    let content = [ContentBlock::Text { text: idle.to_string() }];
    db::save_idle_notice(&pool, by_idle, "t3-idle", &content).await.expect("save it").expect("saved");

    let message = "T3 notice from another conversation";
    db::queue_notice(&pool, by_message, by_command, "smelt", message).await.expect("queue it");
    let row = db::undelivered_agent_messages(&pool, by_message).await.expect("rows").remove(0);
    let content = [ContentBlock::Text { text: message.to_string() }];
    db::deliver_agent_message(&pool, &row, &content).await.expect("deliver it").expect("delivered");

    let answer = "T3 the card's answer";
    db::create_message(&pool, by_answer, "user", &[ContentBlock::Text { text: "Ask me.".to_string() }]).await.expect("save");
    let call = ContentBlock::ToolUse { id: "toolu_t3".to_string(), name: "ask_user".to_string(), input: serde_json::json!({}) };
    db::create_message(&pool, by_answer, "assistant", &[call]).await.expect("save");
    sqlx::query("INSERT INTO pending_questions (conversation_id, tool_use_id, questions, answer) VALUES ($1, 'toolu_t3', '[]', '[]')")
        .bind(by_answer)
        .execute(&pool)
        .await
        .expect("an answered question");
    let result_for = |q: &db::StoredQuestion| ContentBlock::ToolResult {
        tool_use_id: q.tool_use_id.clone(),
        content: answer.to_string(),
        is_error: None,
    };
    db::create_message_taking_question(&pool, by_answer, "user", Vec::new(), false, result_for)
        .await
        .expect("take it")
        .expect("taken");

    restart(&pool, &ids).await;
    let notice = command_notice("t3-ended");
    for (id, needle) in [(by_command, notice.as_str()), (by_idle, idle), (by_message, message), (by_answer, answer)] {
        assert_eq!(requests_ending_in(&requests, needle), 1, "#{id} wasn't answered");
        assert_eq!(messages_holding(&pool, id, needle).await, 1, "#{id}'s notice, once");
    }
    for id in ids {
        crate::turn::forget_conversation_lock(id);
    }
}

/// What smelt tells a conversation when Docker restarts, as
/// `sandbox/claims.rs` says it.
pub(super) fn docker_restarted() -> String {
    crate::api::chat::mark_notice("Docker in sandbox pod 1 was restarted, so the containers it was running have stopped.")
}

/// T2b (D4): a `Deliver` to a conversation the user stopped is saved at
/// once, as before, and nothing is queued or called.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_deliver_to_a_stopped_conversation_is_saved_at_once(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    crate::turn::stop_conversation(&pool, root).await;
    let text = docker_restarted();
    let deliver = crate::turn::notify(&pool, root, vec![crate::turn::Notice::Deliver(text.clone())]);
    tokio::time::timeout(Duration::from_secs(10), deliver).await.expect("the notice is saved").expect("the task");

    let told = transcript(&pool, root).await.into_iter().filter(|(_, said)| said == &text).count();
    assert_eq!(told, 1, "the notice, once");
    let queued = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1")
        .bind(root)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(queued, 0, "a row was queued");
    assert_eq!(requests.lock().expect("the request log").len(), 0, "the model was called");
    crate::turn::forget_conversation_lock(root);
}

/// D9: a Stop that ends a `Deliver`'s wake while it's queued behind a
/// running turn still shows the notice at once, as before the queue and
/// as D4 does: saved once, its row delivered, and no model call.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_ending_a_queued_deliver_still_saves_it_at_once(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let text = docker_restarted();
    let turn = crate::turn::conversation_lock(root).lock_owned().await;
    let (reached, release) = crate::turn::test_hooks::pause_after_notice_admitted(root);
    let deliver = crate::turn::notify(&pool, root, vec![crate::turn::Notice::Deliver(text.clone())]);
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the notice is admitted").expect("the hook");
    // Stopped once let in. The yields usually let the wake reach its wait
    // for the held lock first, so the stop ends it there; landing after
    // the lock is free instead, the stop ends it mid-turn, before any
    // model call. Either way the notice is saved at once, as asserted.
    crate::turn::stop_conversation(&pool, root).await;
    release.send(()).expect("the wake waits at the hook");
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    drop(turn);
    tokio::time::timeout(Duration::from_secs(10), deliver).await.expect("the delivery ends").expect("the task");
    // No turn will answer it: a mark here would say a restart dropped one
    // (review 1, L1).
    assert!(!db::answer_owed(&pool, root).await.expect("answer_owed"), "the saved notice marked it owed");

    let told = transcript(&pool, root).await.into_iter().filter(|(_, said)| said == &text).count();
    assert_eq!(told, 1, "the notice, once");
    let undelivered = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1 AND delivered_at IS NULL",
    )
    .bind(root)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(undelivered, 0, "its row is still queued");
    assert_eq!(requests.lock().expect("the request log").len(), 0, "the model was called");
    crate::turn::forget_conversation_lock(root);
}

/// T5: a child owed an answer that SME-238 also tells about its cut-off
/// turn is answered once, with both in the one request.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_answer_owed_is_answered_once(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Carrying on.")]).await;
    let parent = quiet_parent(&pool).await;
    let child = active_child(&pool, parent).await;
    db::create_message(&pool, child, "user", &[ContentBlock::Text { text: "Work.".to_string() }]).await.expect("save");
    db::create_message(&pool, child, "assistant", &[ContentBlock::Text { text: "Done.".to_string() }]).await.expect("save");
    command(&pool, child, "t5", true).await;
    drain_command(&pool, child, "t5").await;
    db::message_agent(&pool, parent, db::Recipient::Child(child), "your parent", "One more thing.").await.expect("queue it");
    let row = db::undelivered_agent_messages(&pool, child).await.expect("rows").remove(0);
    let text = vec![ContentBlock::Text { text: "Message from your parent:\n\nOne more thing.".to_string() }];
    db::deliver_agent_message(&pool, &row, &text).await.expect("deliver it").expect("delivered");

    let checked = test_hooks::last_turns_checked(child);
    restart(&pool, &[parent, child]).await;
    let calls = requests_for(&requests, Some(child));
    assert!(!calls.is_empty(), "#{child} wasn't woken");
    let sent = calls[0]["messages"].to_string();
    assert!(sent.contains("Terminal command t5 finished") && sent.contains(RESTARTED_MID_TURN), "{sent}");
    eventually("the child's end-of-turn check ran", || async { test_hooks::last_turns_checked(child) > checked }).await;
    // SME-455: that answered turn ended plain and stuck, so the seat was
    // nudged — wait its spell out (nudge-woken turn, notice to the parent)
    // before the second restart proves nothing else is owed.
    eventually("the child's nudge spell is over (the stopped parent only queues the notice)", || async {
        !crate::turn::turn_running(child)
            && requests_for(&requests, Some(child)).len() == 2
            && sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1 AND kind = 'notice'",
            )
            .bind(parent)
            .fetch_one(&pool)
            .await
            .unwrap_or(0)
                == 1
    })
    .await;
    restart(&pool, &[parent, child]).await;
    assert_eq!(requests_for(&requests, Some(child)).len(), 2, "a second restart answered again");
    let said = transcript(&pool, child).await;
    assert_eq!(
        said.iter().filter(|(_, t)| t.contains(crate::agents::queue::REPORT_OWE_NUDGE)).count(),
        1,
        "the nudge, once: {said:?}"
    );
    assert_eq!(said.iter().filter(|(_, t)| t.contains(RESTARTED_MID_TURN)).count(), 1, "the cut-off notice wasn't answered again");
    for id in [parent, child] {
        crate::turn::forget_conversation_lock(id);
    }
}

/// T8: what isn't owed, or can't be answered, isn't woken: a root the
/// user stopped, a stopped and a failed child, each with a notice saved,
/// a root whose last message is the user's own send (D2), and one being
/// deleted.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_what_isnt_owed_isnt_woken_after_a_restart(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Noted.")]).await;
    let paused = db::create_conversation(&pool).await.expect("create conversation").id;
    command(&pool, paused, "t8-paused", true).await;
    drain_command(&pool, paused, "t8-paused").await;
    crate::turn::stop_conversation(&pool, paused).await;
    let parent = quiet_parent(&pool).await;
    let mut ids = vec![paused, parent];
    for (state, command_id) in [(db::AGENT_STOPPED, "t8-stopped"), (db::AGENT_FAILED, "t8-failed")] {
        let child = active_child(&pool, parent).await;
        command(&pool, child, command_id, true).await;
        drain_command(&pool, child, command_id).await;
        sqlx::query("UPDATE agents SET state = $2 WHERE conversation_id = $1")
            .bind(child)
            .bind(state)
            .execute(&pool)
            .await
            .expect("set the state");
        ids.push(child);
    }
    let sent = db::create_conversation(&pool).await.expect("create conversation").id;
    db::create_message(&pool, sent, "user", &[ContentBlock::Text { text: "Hello.".to_string() }]).await.expect("save");
    ids.push(sent);
    // Being deleted, owed an answer and with an end undrained (review 1,
    // N3); the D2 case through the real send is restart_owed_edges'.
    let deleting = db::create_conversation(&pool).await.expect("create conversation").id;
    command(&pool, deleting, "t8-deleting", true).await;
    sqlx::query("UPDATE conversations SET deleting_at = now(), answer_owed_at = now() WHERE id = $1")
        .bind(deleting)
        .execute(&pool)
        .await
        .expect("mark it deleting");
    ids.push(deleting);

    restart(&pool, &ids).await;
    assert_eq!(requests.lock().expect("the request log").len(), 0, "something was woken");
    for id in ids {
        crate::turn::forget_conversation_lock(id);
    }
}

/// T9 (SME-218): while a wake's turn runs, parked on the slot with its
/// answer owed, a second wake with nothing pending isn't counted.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_during_an_owed_turn_is_not_counted(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Noted.")]).await;
    let provider = mock_provider(&pool).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    command(&pool, root, "t9", true).await;
    let held = hold_the_one_slot(&pool, provider).await;
    let parked = crate::turn::test_hooks::watch_before_turn_slot(root);
    let first = crate::turn::notify(&pool, root, vec![crate::turn::Notice::Wake]);
    tokio::time::timeout(Duration::from_secs(10), parked).await.expect("the wake reaches the slot").expect("the hook");
    assert!(crate::turn::turn_running(root), "the first wake's turn runs");

    let mut events = crate::events::subscribe(root);
    let (admitted, _release) = crate::turn::test_hooks::pause_after_notice_admitted(root);
    let second = crate::turn::notify(&pool, root, vec![crate::turn::Notice::Wake]);
    tokio::select! {
        ended = second => ended.expect("the second wake"),
        _ = admitted => panic!("the second wake was let in"),
        _ = tokio::time::sleep(Duration::from_secs(10)) => panic!("the second wake didn't end within 10 s"),
    }
    let mut states = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let crate::events::ConversationEvent::TurnState { running, .. } = event {
            states.push(running);
        }
    }
    assert_eq!(states, Vec::<bool>::new(), "tabs were told a turn started");
    assert!(crate::turn::turn_running(root), "still the first wake's turn");

    kill(first).await;
    drop(held);
    crate::providers::limiter::forget(provider);
    assert_eq!(requests.lock().expect("the request log").len(), 0, "a request was made");
    crate::turn::forget_conversation_lock(root);
}

/// A mock upstream that refuses every request with a 500; returns how
/// many it refused.
async fn failing_upstream(pool: &PgPool) -> Arc<std::sync::atomic::AtomicUsize> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let refused = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = refused.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || {
            counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async { (axum::http::StatusCode::INTERNAL_SERVER_ERROR, "{\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"boom\"}}") }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    crate::providers::test_support::add_mock_provider(pool, addr).await;
    refused
}

/// T7: a wake's model call failed and its turn ended: a restart doesn't
/// try it again (D7).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failed_turn_owes_nothing_after_a_restart(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let refused = failing_upstream(&pool).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    command(&pool, root, "t7", true).await;
    let wake = crate::turn::notify(&pool, root, vec![crate::turn::Notice::Wake]);
    tokio::time::timeout(Duration::from_secs(60), wake).await.expect("the wake ends").expect("the wake");
    let tried = refused.load(std::sync::atomic::Ordering::SeqCst);
    assert!(tried > 0, "the wake never called the model");
    assert_eq!(messages_holding(&pool, root, &command_notice("t7")).await, 1, "the notice");

    restart(&pool, &[root]).await;
    assert_eq!(refused.load(std::sync::atomic::Ordering::SeqCst), tried, "the restart tried the failed turn again");
    crate::turn::forget_conversation_lock(root);
}

/// T6: the model's reply was saved, and the turn didn't end before the
/// restart: nothing is owed.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_saved_before_the_restart_owes_nothing(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Again.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    command(&pool, root, "t6", true).await;
    drain_command(&pool, root, "t6").await;
    assert!(db::answer_owed(&pool, root).await.expect("answer_owed"), "the drain's mark");
    db::save_reply_message(&pool, root, &[ContentBlock::Text { text: "Noted.".to_string() }]).await.expect("the reply");

    restart(&pool, &[root]).await;
    assert_eq!(requests.lock().expect("the request log").len(), 0, "the model was called on its own reply");
    crate::turn::forget_conversation_lock(root);
}

/// T2: a `Deliver` waiting for a running turn's lock when smelt restarts:
/// its text survives, and is answered once.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_deliver_waiting_for_the_turn_lock_survives_a_restart(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let text = docker_restarted();
    // Stands in for a running turn.
    let turn = crate::turn::conversation_lock(root).lock_owned().await;
    let (reached, release) = crate::turn::test_hooks::pause_after_notice_admitted(root);
    let deliver = crate::turn::notify(&pool, root, vec![crate::turn::Notice::Deliver(text.clone())]);
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the notice is admitted").expect("the hook");
    kill(deliver).await;
    drop(release);
    drop(turn);

    restart(&pool, &[root]).await;
    let told = transcript(&pool, root).await.into_iter().filter(|(_, said)| said == &text).count();
    assert_eq!(told, 1, "the notice, once");
    let calls = requests_for(&requests, None);
    assert_eq!(calls.len(), 1, "#{root} wasn't woken");
    let last = calls[0]["messages"].as_array().expect("messages").last().expect("a message").to_string();
    assert!(last.contains("Docker in sandbox pod 1 was restarted"), "{last}");
    let delivered = sqlx::query_scalar::<_, i64>(
        "SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1 AND delivered_at IS NOT NULL",
    )
    .bind(root)
    .fetch_one(&pool)
    .await
    .expect("count");
    assert_eq!(delivered, 1, "its row");
    crate::turn::forget_conversation_lock(root);
}

/// T4, the ticket's own inferred state: a command's end was recorded and
/// smelt restarted before any wake drained it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_end_recorded_before_the_restart_is_drained_after_it(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    command(&pool, root, "t4", true).await;

    restart(&pool, &[root]).await;
    assert_eq!(requests_ending_in(&requests, &command_notice("t4")), 1, "#{root} wasn't woken");
    assert!(db::unnotified_finished_terminal_commands(&pool, root).await.expect("query").is_empty(), "the command");
    crate::turn::forget_conversation_lock(root);
}
