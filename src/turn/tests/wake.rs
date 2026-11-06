use super::*;

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_wake_conversation_is_a_noop_when_nothing_is_pending(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    let counter = start_mock_upstream(&pool, vec!["unused".to_string()]).await;

    let result = wake_conversation(&pool, conversation.id)
        .await
        .expect("wake_conversation should succeed even with nothing pending");
    assert!(
        result.is_empty(),
        "expected no persisted messages, got {result:?}"
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        0,
        "nothing pending should mean no API call at all"
    );

    let messages = db::list_messages(&pool, conversation.id)
        .await
        .expect("list messages");
    assert!(
        messages.is_empty(),
        "no message should be persisted when nothing is pending"
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_wake_conversation_drains_a_pending_command_and_completes_a_turn(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-1").await;

    let body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Noted."}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_mock_upstream(&pool, vec![body]).await;

    let messages = wake_conversation(&pool, conversation.id)
        .await
        .expect("wake_conversation should succeed");

    assert_eq!(
        messages.len(),
        2,
        "expected the notification plus the assistant's reply, got {messages:?}"
    );
    assert_eq!(messages[0].role, "user");
    assert_eq!(
        messages[0].blocks().expect("valid blocks"),
        vec![anthropic::ContentBlock::Text {
            text: format!("{}Terminal command cmd-1 finished: exit code 0.", crate::api::chat::NOTICE_MARK)
        }]
    );
    assert_eq!(messages[1].role, "assistant");

    let remaining = db::unnotified_finished_terminal_commands(&pool, conversation.id)
        .await
        .expect("query unnotified commands");
    assert!(
        remaining.is_empty(),
        "the command should now be marked notified"
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_wake_conversation_second_call_is_a_noop_once_the_first_drained_everything(
    pool: PgPool,
) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-1").await;

    let body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"Noted."}}"#,
        ),
        (
            "content_block_stop",
            r#"{"type":"content_block_stop","index":0}"#,
        ),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    let counter = start_mock_upstream(&pool, vec![body]).await;

    let first = wake_conversation(&pool, conversation.id)
        .await
        .expect("first wake should succeed");
    assert_eq!(first.len(), 2);
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "first wake should make exactly one API call"
    );

    // Simulates a second, near-simultaneous exit event's own detached
    // wake_conversation call — nothing should be left to drain, so this
    // must not persist another message or make another API call.
    let second = wake_conversation(&pool, conversation.id)
        .await
        .expect("second wake should succeed");
    assert!(
        second.is_empty(),
        "second wake should find nothing left to drain, got {second:?}"
    );
    assert_eq!(
        counter.load(Ordering::SeqCst),
        1,
        "second wake should not make another API call"
    );
}

/// SME-72: with no model to run on, a wake-up still saves the finished
/// command's notice (the model sees it once there is one) and says why
/// it couldn't deliver it; a wake with nothing pending doesn't take the
/// default or say anything.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_with_no_model_still_saves_the_notice(pool: PgPool) {
    let _guard = lock_turn_tests();
    // An id no other test's database hands out: what this test hears on
    // its conversation's channel mustn't be another test's (SME-135).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-1").await;
    let mut rx = events::subscribe(conversation.id);

    let error = wake_conversation(&pool, conversation.id)
        .await
        .expect_err("no model to deliver to");

    assert_eq!(error.message(), crate::providers::NO_MODEL_CONFIGURED);
    let saved = db::list_messages(&pool, conversation.id).await.expect("list messages");
    assert_eq!(saved.len(), 1, "the notice is saved: {saved:?}");
    let failure = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if let events::ConversationEvent::NotificationDeliveryFailed { detail } =
                rx.recv().await.expect("event channel should not close")
            {
                return detail;
            }
        }
    })
    .await
    .expect("the failure is published");
    assert_eq!(failure, crate::providers::NO_MODEL_CONFIGURED);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_with_nothing_pending_leaves_the_model_alone(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    // The mock first: it moves conversations already there onto it.
    start_mock_upstream(&pool, vec!["unused".to_string()]).await;
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");

    wake_conversation(&pool, conversation.id).await.expect("nothing to do");

    let model = db::get_conversation_model(&pool, conversation.id)
        .await
        .expect("read")
        .expect("exists");
    assert_eq!(model.provider_id, None, "a wake with nothing to say doesn't take the default");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_wake_conversation_publishes_notification_delivery_failed_on_error(pool: PgPool) {
    let _guard = lock_turn_tests();
    // An id no other test's database hands out: what this test hears on
    // its conversation's channel mustn't be another test's (SME-135).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-1").await;

    // `fail_count` is irrelevant when `success_body` is `None` — every
    // request fails regardless (see the helper's doc comment).
    start_mock_upstream_failing_n_times(&pool, 0, None).await;

    let mut rx = events::subscribe(conversation.id);

    let result = wake_conversation(&pool, conversation.id).await;
    assert!(
        result.is_err(),
        "expected wake_conversation to surface the underlying failure, got {result:?}"
    );

    // The turn publishes other events too (its state, the drained
    // notice, reply resets); wait for this one.
    let event = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            match rx.recv().await.expect("event channel should not close") {
                event @ events::ConversationEvent::NotificationDeliveryFailed { .. } => return event,
                _ => continue,
            }
        }
    })
    .await
    .expect("should not time out waiting for the event");
    assert!(
        matches!(
            event,
            events::ConversationEvent::NotificationDeliveryFailed { .. }
        ),
        "expected NotificationDeliveryFailed, got {event:?}"
    );

    // The notification text itself is durably persisted regardless —
    // the drain step commits before the API call that failed.
    let messages = db::list_messages(&pool, conversation.id)
        .await
        .expect("list messages");
    assert_eq!(
        messages.len(),
        1,
        "the notification message should still be persisted, got {messages:?}"
    );
    assert_eq!(messages[0].role, "user");
}

// --- Idle notices (SME-79) ---

/// SME-79: a wake drains idle notices too — a running command left untouched
/// past its notice period is told to the model as its own user message, says
/// plainly that nothing stopped it and what it can do about it, and is marked
/// so its clock restarts from this notice. The command keeps running.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_wake_drains_an_idle_notice_and_the_command_keeps_running(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    untouched_running_command(&pool, conversation.id, "cmd-idle", 601).await;

    let counter = start_mock_upstream(&pool, vec![text_reply_body("Noted.")]).await;

    let messages = wake_conversation(&pool, conversation.id)
        .await
        .expect("wake_conversation should succeed");
    assert_eq!(messages.len(), 2, "the notice plus the reply: {messages:?}");
    assert_eq!(messages[0].role, "user");
    let anthropic::ContentBlock::Text { text } =
        messages[0].blocks().expect("valid blocks")[0].clone()
    else {
        panic!("the notice is a text block: {:?}", messages[0]);
    };
    // SME-357: the saved idle notice carries the mark before its words.
    assert!(text.starts_with(crate::api::chat::NOTICE_MARK), "marked: {text}");
    // The plan's sentence, in parts: which command, how long it has run,
    // how long since anything touched it, what it has printed, that nothing
    // stopped it, and what the model can do.
    for wanted in [
        "Terminal command cmd-idle",
        "sleep 100",
        "running for 10 minutes",
        "hasn't been touched in 10 minutes",
        "0 lines on stdout, 0 on stderr",
        "Nothing stopped it and smelt won't stop it",
        "read_terminal_output",
        "send_signal",
        "set_idle_notice",
    ] {
        assert!(text.contains(wanted), "{wanted:?} missing from: {text}");
    }
    // SME-370 review 1/2: the notice's fixed text, pinned as this test's own
    // literals — never through `IDLE_NOTICE_FRAGMENT`, an assertion
    // interpolating the const it guards can never go red. Two pins: the words,
    // and the delimiter the transcript splits on *with its spaces*. The short
    // line (`pages/chat/transcript/text.rs`) cuts the stored sentence at
    // `" is still running and hasn't been touched in "` — a leading and a
    // trailing space and all — so a space is part of the seam: a re-word OR a
    // whitespace-only re-flow of the `format!` (a lost or doubled space) breaks
    // that cut in silence, which the words-alone pin misses. Pin the delimiter
    // spaces and all, and both drifts go red here in lockstep with the const.
    assert!(
        text.contains("is still running and hasn't been touched in"),
        "the idle notice's fixed clause drifted: update IDLE_NOTICE_FRAGMENT \
         (src/turn/notify.rs) and the transcript copy \
         (src/frontend/pages/chat/transcript/text.rs) together — {text}"
    );
    // The exact delimiter `text.rs` splits on, spaces and all — this is what
    // catches a whitespace-only re-flow the words-alone pin sails past.
    assert!(
        text.contains(" is still running and hasn't been touched in "),
        "the delimiter's spaces drifted — the transcript splits on the phrase \
         with a leading and a trailing space, so a re-flow of the `format!`'s \
         whitespace must match them; update IDLE_NOTICE_FRAGMENT and the copy \
         (src/frontend/pages/chat/transcript/text.rs) together — {text}"
    );
    assert_eq!(messages[1].role, "assistant");

    let command = db::get_terminal_command(&pool, "cmd-idle")
        .await
        .expect("query")
        .expect("the command exists");
    assert_eq!(command.status, "running", "smelt did not stop it — SME-79's whole point");
    assert!(command.idle_notice_at.is_some(), "the notice is marked sent");
    assert!(command.notified_at.is_none(), "an idle notice is not a finish notice");

    // A second wake finds nothing, because this notice restarted the command's
    // clock — not because notices stop after one. Left alone again it earns
    // the next one (see `db::tests::commands::test_idle_notices_repeat_until_
    // the_command_is_touched`).
    let second = wake_conversation(&pool, conversation.id).await.expect("second wake");
    assert!(second.is_empty(), "the clock restarted, nothing is due yet: {second:?}");
    assert_eq!(counter.load(Ordering::SeqCst), 1, "one API call, for the notice");
}

/// SME-79: the wake-up is for *untouched* commands. A running command
/// nobody has left long is no reason to wake anyone.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_running_command_not_due_is_left_alone(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    untouched_running_command(&pool, conversation.id, "cmd-fresh", 0).await;

    let counter = start_mock_upstream(&pool, vec!["unused".to_string()]).await;
    let result = wake_conversation(&pool, conversation.id).await.expect("wake");
    assert!(result.is_empty(), "nothing is due: {result:?}");
    assert_eq!(counter.load(Ordering::SeqCst), 0, "no API call for a command not due");
}

/// SME-79: the idle notice's duration, and every notice sentence's one
/// honest clock — largest unit that fits, plus a second unit when there's
/// a remainder worth naming, always rounded down.
#[test]
fn test_format_duration_names_one_or_two_units() {
    assert_eq!(format_duration(0), "0 seconds");
    assert_eq!(format_duration(-5), "0 seconds", "never negative, never overstated");
    assert_eq!(format_duration(45), "45 seconds");
    assert_eq!(format_duration(59), "59 seconds");
    assert_eq!(format_duration(60), "1 minute");
    assert_eq!(format_duration(300), "5 minutes", "a notice fires on exactly its interval");
    assert_eq!(format_duration(359), "5 minutes 59 seconds", "the two units that fit");
    assert_eq!(format_duration(3_600), "1 hour");
    assert_eq!(format_duration(7_500), "2 hours 5 minutes");
    assert_eq!(format_duration(86_400), "1 day");
    assert_eq!(format_duration(90_000), "1 day 1 hour");
}

/// SME-79 review 1: the paused-conversation path the deferral rides on,
/// exercised for the idle notice. While the user has stopped the
/// conversation, the wake is a no-op: the notice is neither saved nor
/// marked, so nothing is lost or duplicated. The next message's turn
/// drains it, and its numbers are the ones the command has earned by
/// then — recomputed at the drain, not frozen at the wake that didn't
/// happen. ("untouched for 3 hours" reads oddly; it's honest.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stopped_conversation_holds_the_idle_notice_for_the_next_message(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    untouched_running_command(&pool, conversation.id, "cmd-paused", 601).await;

    let counter = start_mock_upstream(&pool, vec![text_reply_body("Noted.")]).await;

    stop_turn_now(conversation.id);
    assert!(is_paused(conversation.id), "a stop pauses the conversation");

    wake_conversation(&pool, conversation.id)
        .await
        .expect("a paused wake does nothing, successfully");
    assert_eq!(counter.load(Ordering::SeqCst), 0, "the paused conversation is not woken for the notice");
    let saved = db::list_messages(&pool, conversation.id).await.expect("list messages");
    assert!(saved.is_empty(), "nothing is saved while the conversation is stopped: {saved:?}");
    let command = db::get_terminal_command(&pool, "cmd-paused")
        .await
        .expect("query")
        .expect("the command row");
    assert!(command.idle_notice_at.is_none(), "the notice isn't marked — it's still owed, not lost");

    // The stop itself stretches the clock. When the user comes back, the
    // notice is written from the numbers of that moment.
    sqlx::query("UPDATE terminal_commands SET created_at = now() - make_interval(secs => 7501) WHERE command_id = $1")
        .bind("cmd-paused")
        .execute(&pool)
        .await
        .expect("backdate through the stop");
    resume_turns(conversation.id);
    assert!(!is_paused(conversation.id));

    run_turn(&pool, conversation.id, hello()).await.expect("the user's next turn runs");

    let saved = db::list_messages(&pool, conversation.id).await.expect("list messages");
    let notices: Vec<&String> = saved
        .iter()
        .filter(|m| m.content.contains("still running"))
        .map(|m| &m.content)
        .collect();
    assert_eq!(notices.len(), 1, "the drain saved the one notice: {saved:?}");
    // 7501 s, not the 601 s it was at the wake that did nothing.
    assert!(notices[0].contains("running for 2 hours 5 minutes"), "recomputed at the drain: {}", notices[0]);
    assert!(notices[0].contains("hasn't been touched in 2 hours 5 minutes"), "{}", notices[0]);
    assert_eq!(counter.load(Ordering::SeqCst), 1, "the user's turn is the only model call");
}

// --- A wake with nothing to do isn't counted (SME-218) ---

/// Every `TurnState` `rx` has had so far, as `running` values in order.
fn turn_states_heard(rx: &mut tokio::sync::broadcast::Receiver<events::ConversationEvent>) -> Vec<bool> {
    let mut states = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let events::ConversationEvent::TurnState { running, .. } = event {
            states.push(running);
        }
    }
    states
}

/// A question the model asked, still waiting for the user.
async fn waiting_question(pool: &PgPool, conversation_id: i64) {
    let questions = vec![crate::questions::Question {
        question: "Delete the build directory?".to_string(),
        header: "Delete".to_string(),
        options: vec![],
        multi_select: false,
    }];
    db::create_pending_question(pool, conversation_id, "toolu_ask", &questions)
        .await
        .expect("create the question");
}

/// SME-218: a wake that finds nothing pending isn't counted as a turn,
/// so no tab is told one started (it would flash "Working…" and clear
/// the error it shows), and the kept error stays.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_with_nothing_pending_tells_no_tab_a_turn_started(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    let counter = start_mock_upstream(&pool, vec!["unused".to_string()]).await;
    remember_turn_error(conversation_id, Some("the last turn failed".to_string()));
    let mut rx = events::subscribe(conversation_id);

    let woke = wake_conversation(&pool, conversation_id).await.expect("nothing to do");

    assert!(woke.is_empty(), "the wake had nothing to do: {woke:?}");
    assert_eq!(turn_states_heard(&mut rx), Vec::<bool>::new(), "tabs were told a turn started");
    assert_eq!(last_turn_error(conversation_id).as_deref(), Some("the last turn failed"));
    assert_eq!(counter.load(Ordering::SeqCst), 0, "the wake called the model");
}

/// SME-218: a question still waiting for the user is nothing a wake can
/// act on (its turn would stop at the question), so it isn't counted.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_with_only_an_unanswered_question_tells_no_tab(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    let counter = start_mock_upstream(&pool, vec!["unused".to_string()]).await;
    waiting_question(&pool, conversation_id).await;
    let mut rx = events::subscribe(conversation_id);

    let woke = wake_conversation(&pool, conversation_id).await.expect("nothing to do");

    assert!(woke.is_empty(), "the wake had nothing to do: {woke:?}");
    assert_eq!(turn_states_heard(&mut rx), Vec::<bool>::new(), "tabs were told a turn started");
    assert_eq!(counter.load(Ordering::SeqCst), 0, "the wake called the model");
    assert!(
        db::get_pending_question(&pool, conversation_id).await.expect("read").is_some(),
        "the question still waits"
    );
}

/// SME-218's guard for the answered-question clause: an answer no turn
/// took (the server restarted between the answer and its turn) is taken
/// by the next wake, which sends it to the model.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_takes_an_answer_no_turn_took(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    let counter =
        start_mock_upstream(&pool, vec![super::ask_user::ask_body(), text_reply_body("Deleting.")]).await;
    run_turn(&pool, conversation_id, hello()).await.expect("asks");
    let answers = vec![crate::questions::QuestionAnswer { selected: vec!["Yes (Recommended)".to_string()], other: None }];
    assert!(
        db::answer_pending_question(&pool, conversation_id, "toolu_ask", &answers).await.expect("answer"),
        "the answer is recorded, with no turn spawned for it"
    );

    let woke = wake_conversation(&pool, conversation_id).await.expect("the wake runs");

    assert_eq!(woke.len(), 2, "the answer and the reply: {woke:?}");
    assert!(
        matches!(
            &woke[0].blocks().expect("valid blocks")[0],
            anthropic::ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "toolu_ask"
        ),
        "the answer went first: {:?}",
        woke[0]
    );
    assert_eq!(counter.load(Ordering::SeqCst), 2, "the wake sent the answer to the model");
    assert_eq!(db::get_pending_question(&pool, conversation_id).await.expect("read"), None);
}

/// SME-218's guard for `'lost'` in the shared fragment: a command whose
/// outcome is unknown is told to the model by a wake, as a finished one is.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_drains_a_lost_command(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    untouched_running_command(&pool, conversation_id, "cmd-lost", 0).await;
    db::mark_terminal_command_lost(&pool, "cmd-lost").await.expect("mark lost");
    let counter = start_mock_upstream(&pool, vec![text_reply_body("Noted.")]).await;

    let woke = wake_conversation(&pool, conversation_id).await.expect("the wake runs");

    assert_eq!(woke.len(), 2, "the notice and the reply: {woke:?}");
    assert!(woke[0].content.contains("cmd-lost's outcome is unknown"), "{:?}", woke[0]);
    assert!(
        woke[0].content.contains(&format!("{}{}", crate::api::chat::NOTICE_MARK, "Terminal command cmd-lost")),
        "SME-357: the unknown-outcome notice carries the mark: {:?}",
        woke[0]
    );
    assert_eq!(counter.load(Ordering::SeqCst), 1, "the wake called the model");
}

/// SME-409: the sighting state — a question the conversation waits on, and
/// a finished command, an idle-due running command and a queued agent
/// message all owed at once. A wake must stop at the question *before*
/// consuming any of them: a saved notice is invisible to every re-wake
/// rule (`wake_has_work` counts only *un*notified ends, due idles,
/// answered-untaken questions and undelivered rows), so consuming one into
/// a turn that stops before its model call freezes it until the question
/// resolves — the ticket's notice saved with no turn telling it. All three
/// stay queued and marked un-notified; then the answer, recorded with no
/// turn, is told by the next wake *together with* all three notices, in
/// one model call.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_while_a_question_waits_leaves_every_notice_queued(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation_id = db::create_conversation(&pool).await.expect("create conversation").id;
    waiting_question(&pool, conversation_id).await;
    // The finished command (`cmd-1`) and the idle-due running one
    // (`cmd-idle`) on one live pod — the pair this file's fixture builds,
    // since the two single-purpose ones can't share a conversation.
    finished_and_idle_commands(&pool, conversation_id).await;
    // And a queued agent message, owed like the other two.
    let mut conn = pool.acquire().await.expect("a connection");
    db::queue_agent_message(
        &mut conn,
        conversation_id,
        None,
        "agent #9",
        db::KIND_MESSAGE,
        "Reply body.",
    )
    .await
    .expect("queue the message");

    let counter = start_mock_upstream(&pool, vec![text_reply_body("Noted.")]).await;

    // The blocked wake: it stops at the question, having consumed nothing.
    let woke = wake_conversation(&pool, conversation_id)
        .await
        .expect("a wake that stops at the question succeeds");
    let saved = db::list_messages(&pool, conversation_id).await.expect("list messages");
    assert!(saved.is_empty(), "nothing may be saved into a turn that stops at the question: {saved:?}");
    assert!(woke.is_empty(), "the blocked wake persisted nothing: {woke:?}");
    let remaining = db::unnotified_finished_terminal_commands(&pool, conversation_id)
        .await
        .expect("query unnotified commands");
    assert!(
        remaining.iter().any(|c| c.command_id == "cmd-1"),
        "cmd-1's finish notice is still owed, not consumed: {remaining:?}"
    );
    let idle = db::get_terminal_command(&pool, "cmd-idle").await.expect("query").expect("the command row");
    assert!(idle.idle_notice_at.is_none(), "cmd-idle's idle notice is still owed: {idle:?}");
    let queued = db::undelivered_agent_messages(&pool, conversation_id).await.expect("query queued rows");
    assert_eq!(queued.len(), 1, "the agent message is still queued: {queued:?}");
    assert_eq!(counter.load(Ordering::SeqCst), 0, "the blocked wake called the model");
    assert!(
        db::get_pending_question(&pool, conversation_id).await.expect("read").is_some(),
        "the question still waits"
    );

    // The question resolves — the answer recorded with no turn spawned —
    // and the next wake tells it *and* everything that stayed queued.
    let answers = vec![crate::questions::QuestionAnswer {
        selected: vec!["Yes (Recommended)".to_string()],
        other: None,
    }];
    assert!(
        db::answer_pending_question(&pool, conversation_id, "toolu_ask", &answers).await.expect("answer"),
        "the answer is recorded, with no turn spawned for it"
    );

    let woke = wake_conversation(&pool, conversation_id).await.expect("the resolving wake runs");
    assert_eq!(woke.len(), 5, "the answer, the three notices and the reply: {woke:?}");
    assert!(
        matches!(
            &woke[0].blocks().expect("valid blocks")[0],
            anthropic::ContentBlock::ToolResult { tool_use_id, .. } if tool_use_id == "toolu_ask"
        ),
        "the answer went first: {:?}",
        woke[0]
    );
    assert!(woke[1].content.contains("Terminal command cmd-1 finished: exit code 0."), "{:?}", woke[1]);
    assert!(woke[2].content.contains("Terminal command cmd-idle"), "{:?}", woke[2]);
    assert!(woke[3].content.contains("Message from agent #9:"), "{:?}", woke[3]);
    assert_eq!(woke[4].role, "assistant", "the one reply: {:?}", woke[4]);
    assert_eq!(counter.load(Ordering::SeqCst), 1, "one model call told the answer and all three notices");
    assert!(
        db::unnotified_finished_terminal_commands(&pool, conversation_id).await.expect("query").is_empty(),
        "cmd-1's notice is told and marked"
    );
    let idle = db::get_terminal_command(&pool, "cmd-idle").await.expect("query").expect("the command row");
    assert!(idle.idle_notice_at.is_some(), "cmd-idle's idle notice is told and marked");
    assert!(
        db::undelivered_agent_messages(&pool, conversation_id).await.expect("query").is_empty(),
        "the agent message is delivered"
    );
    assert!(db::get_pending_question(&pool, conversation_id).await.expect("read").is_none(), "the question is taken");
}
