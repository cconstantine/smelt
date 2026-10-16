use super::*;

// --- ask_user (SME-34) ---

/// A reply that calls each of `calls` (id, tool name, input) and stops for
/// their results.
fn tool_calls_body(calls: &[(&str, &str, serde_json::Value)]) -> String {
    let mut events: Vec<(String, String)> = vec![("message_start".into(), r#"{"type":"message_start"}"#.into())];
    for (index, (id, name, input)) in calls.iter().enumerate() {
        events.push((
            "content_block_start".into(),
            serde_json::json!({"type": "content_block_start", "index": index,
                "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}})
            .to_string(),
        ));
        events.push((
            "content_block_delta".into(),
            serde_json::json!({"type": "content_block_delta", "index": index,
                "delta": {"type": "input_json_delta", "partial_json": input.to_string()}})
            .to_string(),
        ));
        events.push((
            "content_block_stop".into(),
            serde_json::json!({"type": "content_block_stop", "index": index}).to_string(),
        ));
    }
    events.push(("message_delta".into(), r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#.into()));
    events.push(("message_stop".into(), r#"{"type":"message_stop"}"#.into()));
    let borrowed: Vec<(&str, &str)> = events.iter().map(|(e, d)| (e.as_str(), d.as_str())).collect();
    sse_body(&borrowed)
}

fn ask_input() -> serde_json::Value {
    serde_json::json!({"questions": [
        {"question": "Delete the build directory?", "header": "Delete",
         "options": [{"label": "Yes (Recommended)"}, {"label": "No"}]}
    ]})
}

fn ask_body() -> String {
    tool_calls_body(&[("toolu_ask", crate::questions::ASK_USER, ask_input())])
}

fn notice(text: &str) -> anthropic::AnthropicMessage {
    anthropic::AnthropicMessage {
        role: "user".to_string(),
        content: vec![anthropic::ContentBlock::Text { text: text.to_string() }],
    }
}

/// Each saved message's blocks, in order.
async fn saved_blocks(pool: &PgPool, conversation_id: i64) -> Vec<(String, Vec<anthropic::ContentBlock>)> {
    db::list_messages(pool, conversation_id)
        .await
        .expect("messages")
        .into_iter()
        .map(|m| {
            let blocks = m.blocks().expect("blocks");
            (m.role, blocks)
        })
        .collect()
}

/// Waits (up to 10 s) until the recording upstream has had `count`
/// requests.
async fn wait_for_requests(requests: &Arc<std::sync::Mutex<Vec<serde_json::Value>>>, count: usize) {
    for _ in 0..200 {
        if requests.lock().expect("requests").len() >= count {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("expected {count} requests, got {}", requests.lock().expect("requests").len());
}

/// The user message right after the assistant message that called
/// `tool_use_id`, in a request body.
fn message_after_call(request: &serde_json::Value, tool_use_id: &str) -> serde_json::Value {
    let messages = request["messages"].as_array().expect("messages");
    let call = messages
        .iter()
        .position(|m| m["content"].as_array().is_some_and(|c| c.iter().any(|b| b["id"] == tool_use_id)))
        .expect("the call is in the request");
    messages[call + 1].clone()
}

#[sqlx::test]
async fn test_ask_user_ends_the_turn_and_waits(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_300).await.expect("conversation");
    let calls = start_mock_upstream(&pool, vec![ask_body(), text_reply_body("unreached")]).await;
    let mut rx = events::subscribe(conversation.id);

    run_turn(&pool, conversation.id, hello()).await.expect("the turn ends cleanly");

    assert_eq!(calls.load(Ordering::SeqCst), 1, "no model call after the question");
    let saved = saved_blocks(&pool, conversation.id).await;
    assert_eq!(saved.len(), 2, "the user's message and the call, no result: {saved:?}");
    let pending = pending_question(&pool, conversation.id).await.expect("read").expect("a question waits");
    assert_eq!(pending.tool_use_id, "toolu_ask");
    assert_eq!(pending.questions[0].header, "Delete");
    let seen = drain_events(&mut rx).await;
    assert!(
        seen.iter().any(|e| matches!(e, events::ConversationEvent::QuestionUpdate { question: Some(q) } if q.tool_use_id == "toolu_ask")),
        "every tab hears of the question: {seen:?}"
    );
}

#[sqlx::test]
async fn test_ask_user_with_another_tool_saves_its_result_and_waits(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_301).await.expect("conversation");
    let body = tool_calls_body(&[
        ("toolu_read", "todoread", serde_json::json!({})),
        ("toolu_ask", crate::questions::ASK_USER, ask_input()),
    ]);
    let calls = start_mock_upstream(&pool, vec![body, text_reply_body("unreached")]).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn ends cleanly");

    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let saved = saved_blocks(&pool, conversation.id).await;
    assert_eq!(saved.len(), 3, "{saved:?}");
    let results: Vec<&String> = saved[2]
        .1
        .iter()
        .filter_map(|b| match b {
            anthropic::ContentBlock::ToolResult { tool_use_id, .. } => Some(tool_use_id),
            _ => None,
        })
        .collect();
    assert_eq!(results, vec!["toolu_read"], "only the other tool's result is saved");
    assert!(pending_question(&pool, conversation.id).await.expect("read").is_some());
}

#[sqlx::test]
async fn test_a_bad_or_second_ask_user_is_an_error_result(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_302).await.expect("conversation");
    let body = tool_calls_body(&[
        ("toolu_ask", crate::questions::ASK_USER, ask_input()),
        ("toolu_ask2", crate::questions::ASK_USER, ask_input()),
        ("toolu_bad", crate::questions::ASK_USER, serde_json::json!({"questions": []})),
    ]);
    start_mock_upstream(&pool, vec![body, text_reply_body("unreached")]).await;

    run_turn(&pool, conversation.id, hello()).await.expect("the turn ends cleanly");

    let saved = saved_blocks(&pool, conversation.id).await;
    let errors: Vec<(String, String)> = saved
        .last()
        .expect("a results message")
        .1
        .iter()
        .filter_map(|b| match b {
            anthropic::ContentBlock::ToolResult { tool_use_id, content, is_error: Some(true) } => {
                Some((tool_use_id.clone(), content.clone()))
            }
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 2, "{saved:?}");
    assert_eq!(errors[0].0, "toolu_ask2");
    assert!(errors[0].1.contains("one"), "{errors:?}");
    assert_eq!(errors[1].0, "toolu_bad");
    assert_eq!(
        pending_question(&pool, conversation.id).await.expect("read").expect("waits").tool_use_id,
        "toolu_ask"
    );
}

#[sqlx::test]
async fn test_a_notice_while_a_question_waits_is_saved_without_calling_the_model(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_303).await.expect("conversation");
    let calls = start_mock_upstream(&pool, vec![ask_body(), text_reply_body("unreached")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("asks");

    run_turn(&pool, conversation.id, notice("Terminal command c1 finished: exit code 0."))
        .await
        .expect("the notice's turn ends cleanly");

    assert_eq!(calls.load(Ordering::SeqCst), 1, "the notice doesn't reach the model yet");
    let saved = saved_blocks(&pool, conversation.id).await;
    assert_eq!(saved.last().expect("saved").1, notice("Terminal command c1 finished: exit code 0.").content);
    assert!(pending_question(&pool, conversation.id).await.expect("read").is_some(), "still waiting");
}

#[sqlx::test]
async fn test_answering_sends_the_result_right_after_the_call(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_304).await.expect("conversation");
    let requests = start_recording_mock_upstream(&pool, vec![ask_body(), text_reply_body("Deleting.")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("asks");
    run_turn(&pool, conversation.id, notice("a notice")).await.expect("notice saved");

    let answers = vec![crate::questions::QuestionAnswer { selected: vec!["Yes (Recommended)".to_string()], other: None }];
    answer_question(pool.clone(), conversation.id, "toolu_ask".to_string(), answers)
        .await
        .expect("the answer is taken");

    wait_for_requests(&requests, 2).await;
    let request = requests.lock().expect("requests")[1].clone();
    let after = message_after_call(&request, "toolu_ask");
    assert_eq!(after["role"], "user");
    assert_eq!(after["content"][0]["type"], "tool_result", "{after}");
    assert_eq!(after["content"][0]["tool_use_id"], "toolu_ask");
    assert_eq!(after["content"][0]["content"], "The user answered:\n1. Delete: Yes (Recommended)");
    assert_eq!(after["content"][1]["text"], "a notice", "the notice follows the result: {after}");
    assert_eq!(db::get_pending_question(&pool, conversation.id).await.expect("read"), None);
}

#[sqlx::test]
async fn test_a_message_instead_of_an_answer_dismisses_the_question(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_305).await.expect("conversation");
    let requests = start_recording_mock_upstream(&pool, vec![ask_body(), text_reply_body("OK.")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("asks");

    start_turn(pool.clone(), conversation.id, "keep it, and do X instead".to_string())
        .await
        .expect("the message is sent");

    wait_for_requests(&requests, 2).await;
    let request = requests.lock().expect("requests")[1].clone();
    let after = message_after_call(&request, "toolu_ask");
    assert_eq!(after["content"][0]["tool_use_id"], "toolu_ask", "{after}");
    assert_eq!(after["content"][0]["content"], crate::questions::NOT_ANSWERED);
    assert_eq!(after["content"][1]["text"], "keep it, and do X instead");
    assert_eq!(db::get_pending_question(&pool, conversation.id).await.expect("read"), None);
}

#[sqlx::test]
async fn test_a_question_takes_one_valid_answer(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_306).await.expect("conversation");
    let requests = start_recording_mock_upstream(&pool, vec![ask_body(), text_reply_body("OK.")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("asks");
    let answer = |label: &str| vec![crate::questions::QuestionAnswer { selected: vec![label.to_string()], other: None }];

    let err = answer_question(pool.clone(), conversation.id, "toolu_ask".to_string(), answer("Maybe"))
        .await
        .expect_err("a choice that wasn't offered");
    assert!(err.to_string().contains("Maybe"), "{err}");
    assert!(pending_question(&pool, conversation.id).await.expect("read").is_some(), "still waiting");

    answer_question(pool.clone(), conversation.id, "toolu_ask".to_string(), answer("No"))
        .await
        .expect("the first answer");
    let err = answer_question(pool.clone(), conversation.id, "toolu_ask".to_string(), answer("Yes (Recommended)"))
        .await
        .expect_err("a second answer");
    assert!(err.to_string().contains(ALREADY_ANSWERED), "{err}");

    wait_for_requests(&requests, 2).await;
    let request = requests.lock().expect("requests")[1].clone();
    assert_eq!(message_after_call(&request, "toolu_ask")["content"][0]["content"], "The user answered:\n1. Delete: No");
}

/// Code review 1 (high): an answer recorded while a turn that already
/// saved its own message is running (a notice's turn, paused here right
/// after its save) is taken by that turn before it calls the model, not
/// left behind with the call answered "unfinished".
#[sqlx::test]
async fn test_an_answer_landing_mid_turn_is_taken_before_the_model_call(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_307).await.expect("conversation");
    let requests = start_recording_mock_upstream(&pool, vec![ask_body(), text_reply_body("Deleting.")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("asks");

    let (reached, release) = test_hooks::pause_after_message_saved(conversation.id);
    let notice_turn = {
        let pool = pool.clone();
        tokio::spawn(async move { run_turn(&pool, conversation.id, notice("a notice")).await })
    };
    reached.await.expect("the notice's turn saved its message");
    let answers = vec![crate::questions::QuestionAnswer { selected: vec!["No".to_string()], other: None }];
    answer_question(pool.clone(), conversation.id, "toolu_ask".to_string(), answers)
        .await
        .expect("the answer is recorded");
    release.send(()).expect("let the notice's turn go on");
    notice_turn.await.expect("join").expect("the notice's turn");

    wait_for_requests(&requests, 2).await;
    let request = requests.lock().expect("requests")[1].clone();
    let after = message_after_call(&request, "toolu_ask");
    assert_eq!(after["content"][0]["content"], "The user answered:\n1. Delete: No", "{after}");
    assert_eq!(db::get_pending_question(&pool, conversation.id).await.expect("read"), None);
}

/// Code review 2: the question is recorded before the reply's other
/// tools run, so a Stop (or restart) while they run leaves it on its card
/// rather than losing it.
#[sqlx::test]
async fn test_the_question_is_recorded_before_the_replys_other_tools_run(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_308).await.expect("conversation");
    let body = tool_calls_body(&[
        ("toolu_read", "todoread", serde_json::json!({})),
        ("toolu_ask", crate::questions::ASK_USER, ask_input()),
    ]);
    start_mock_upstream(&pool, vec![body, text_reply_body("unreached")]).await;
    let mut rx = events::subscribe(conversation.id);

    run_turn(&pool, conversation.id, hello()).await.expect("the turn ends cleanly");

    let order: Vec<&str> = drain_events(&mut rx)
        .await
        .iter()
        .filter_map(|e| match e {
            events::ConversationEvent::QuestionUpdate { question: Some(_) } => Some("question"),
            events::ConversationEvent::MessagesAppended { messages }
                if messages.iter().any(|m| m.content.contains("toolu_read") && m.role == "user") =>
            {
                Some("results")
            }
            _ => None,
        })
        .collect();
    assert_eq!(order, vec!["question", "results"]);
}

// --- A message a Stop catches queued (SME-170) ---

/// Holds the turn lock while `queue` starts a turn that queues behind it,
/// waits until that turn is in flight, stops it, then releases the lock.
/// Returns every event the conversation published until it went idle.
/// The interleaving is forced, not timed: the queued turn publishes
/// `TurnState { running: true }` just before it makes its stop receiver,
/// with no await in between, so the Stop reaches it while it waits for
/// the lock, and it takes the stop before the lock is free (code review
/// 1: releasing the lock straight after the stop let the turn run and
/// save its message the normal way about half the time). It asserts the
/// message wasn't saved by the turn running.
async fn stop_while_queued(conversation_id: i64, queue: impl std::future::Future<Output = ()>) -> Vec<events::ConversationEvent> {
    let mut rx = events::subscribe(conversation_id);
    // Only the run path reaches this hook. Its release is dropped, so a
    // run-path save doesn't wait on it: it only tells the test it happened.
    let (mut ran, _) = test_hooks::pause_after_message_saved(conversation_id);
    let lock = conversation_lock(conversation_id);
    let running = lock.lock().await;
    queue.await;
    let mut seen = Vec::new();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let event = rx.recv().await.expect("event");
            let started = matches!(event, events::ConversationEvent::TurnState { running: true, .. });
            seen.push(event);
            if started {
                return;
            }
        }
    })
    .await
    .expect("the queued turn is in flight");
    stop_turn_now(conversation_id);
    // The stop woke the queued turn. Yielding lets it run while the lock is
    // still held, so its `select!` finds only the stop ready (the test
    // runtime is current-thread). Dropping the lock first would make both
    // arms ready at once, and the `select!` would pick one at random.
    tokio::task::yield_now().await;
    drop(running);
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let event = rx.recv().await.expect("event");
            let idle = matches!(event, events::ConversationEvent::TurnState { running: false, .. });
            seen.push(event);
            if idle {
                return;
            }
        }
    })
    .await
    .expect("the stopped turn ends");
    assert!(ran.try_recv().is_err(), "the message was saved by the turn running, not by the stop while it was queued");
    seen
}

/// The saved message holding `text`, as blocks.
async fn saved_message_with_text(pool: &PgPool, conversation_id: i64, text: &str) -> Vec<anthropic::ContentBlock> {
    saved_blocks(pool, conversation_id)
        .await
        .into_iter()
        .map(|(_, blocks)| blocks)
        .find(|blocks| blocks.iter().any(|b| matches!(b, anthropic::ContentBlock::Text { text: t } if t == text)))
        .expect("the message is saved")
}

fn tool_result(tool_use_id: &str, content: &str) -> anthropic::ContentBlock {
    anthropic::ContentBlock::ToolResult { tool_use_id: tool_use_id.to_string(), content: content.to_string(), is_error: None }
}

/// The user writes instead of answering, and a Stop catches the message
/// queued behind another turn: the message still dismisses the question,
/// as it does when it runs, and the model later gets one result for the
/// call, the user's text right after it.
#[sqlx::test]
async fn test_a_written_answer_caught_queued_by_a_stop_dismisses_the_question(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_312).await.expect("conversation");
    let requests = start_recording_mock_upstream(&pool, vec![ask_body(), text_reply_body("OK.")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("asks");

    let seen = stop_while_queued(conversation.id, async {
        start_turn(pool.clone(), conversation.id, "keep it, do X instead".to_string())
            .await
            .expect("the message is sent");
    })
    .await;

    assert_eq!(db::get_pending_question(&pool, conversation.id).await.expect("read"), None, "the question is dismissed");
    assert_eq!(
        saved_message_with_text(&pool, conversation.id, "keep it, do X instead").await,
        vec![tool_result("toolu_ask", crate::questions::NOT_ANSWERED), text_block("keep it, do X instead")]
    );
    assert!(
        seen.iter().any(|e| matches!(e, events::ConversationEvent::QuestionUpdate { question: None })),
        "every tab's card goes: {seen:?}"
    );

    start_turn(pool.clone(), conversation.id, "go on".to_string()).await.expect("the next message");
    wait_for_requests(&requests, 2).await;
    let request = requests.lock().expect("requests")[1].clone();
    let after = message_after_call(&request, "toolu_ask");
    assert_eq!(after["content"][0]["tool_use_id"], "toolu_ask", "{after}");
    assert_eq!(after["content"][0]["content"], crate::questions::NOT_ANSWERED);
    assert_eq!(after["content"][1]["text"], "keep it, do X instead", "{after}");
    let results = request["messages"]
        .as_array()
        .expect("messages")
        .iter()
        .flat_map(|m| m["content"].as_array().cloned().unwrap_or_default())
        .filter(|b| b["type"] == "tool_result" && b["tool_use_id"] == "toolu_ask")
        .count();
    assert_eq!(results, 1, "one result for the call: {request}");
}

/// A notice a Stop catches queued isn't the user's reply: the question
/// still waits, and the notice is saved as it is.
#[sqlx::test]
async fn test_a_notice_caught_queued_by_a_stop_leaves_the_question_waiting(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_310).await.expect("conversation");
    start_mock_upstream(&pool, vec![ask_body(), text_reply_body("unreached")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("asks");

    let text = "Terminal command c1 finished: exit code 0.";
    let queued = std::sync::Mutex::new(None);
    stop_while_queued(conversation.id, async {
        let pool = pool.clone();
        *queued.lock().expect("queued") =
            Some(tokio::spawn(async move { run_turn(&pool, conversation.id, notice(text)).await }));
    })
    .await;
    let result = queued.lock().expect("queued").take().expect("spawned").await.expect("join");
    assert!(matches!(result, Err(TurnFailure::Stopped)), "{result:?}");

    assert_eq!(saved_message_with_text(&pool, conversation.id, text).await, notice(text).content);
    assert!(pending_question(&pool, conversation.id).await.expect("read").is_some(), "still waiting");
    resume_turns(conversation.id);
}

/// An answer recorded from the card before the Stop goes first in the
/// stopped message, as it would in a turn that ran.
#[sqlx::test]
async fn test_an_answer_recorded_before_the_stop_goes_with_the_stopped_message(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation_with_id(&pool, 9_100_000_311).await.expect("conversation");
    start_mock_upstream(&pool, vec![ask_body(), text_reply_body("unreached")]).await;
    run_turn(&pool, conversation.id, hello()).await.expect("asks");
    let answers = vec![crate::questions::QuestionAnswer { selected: vec!["Yes (Recommended)".to_string()], other: None }];
    assert!(
        db::answer_pending_question(&pool, conversation.id, "toolu_ask", &answers).await.expect("answer"),
        "the answer is recorded"
    );

    stop_while_queued(conversation.id, async {
        start_turn(pool.clone(), conversation.id, "and then X".to_string())
            .await
            .expect("the message is sent");
    })
    .await;

    assert_eq!(
        saved_message_with_text(&pool, conversation.id, "and then X").await,
        vec![tool_result("toolu_ask", "The user answered:\n1. Delete: Yes (Recommended)"), text_block("and then X")]
    );
    assert_eq!(db::get_pending_question(&pool, conversation.id).await.expect("read"), None);
    resume_turns(conversation.id);
}
