use super::*;
use futures_util::StreamExt;
use std::time::Duration;

/// SME-71: what a conversation "needs the user" for, and when the server
/// says so. Each test names the outcome it walks; the runtime keeps the
/// attention through the busy spell, `TurnInFlight::drop` publishes it
/// once when the spell ends, and the kind is the maximum noted.
///
/// A reply that ends the turn (end_turn), built from `text_reply_body`.
fn done_body(text: &str) -> String {
    text_reply_body(text)
}

/// A reply that asks the user one question (the ask_user.rs shape).
fn ask_body() -> String {
    sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_ask","name":"ask_user","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"questions\":[{\"question\":\"Which one?\",\"header\":\"Which\",\"options\":[{\"label\":\"One\"},{\"label\":\"Two\"}]}]}"}}"#,
        ),
        ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ])
}

/// The `NeedsUser` events this conversation got while `recording` lived,
/// in order, with the generation each carried.
fn needs_user_of(recording: &events::AppRecording, conversation_id: i64) -> Vec<(events::Attention, u64)> {
    recording
        .events()
        .into_iter()
        .filter_map(|event| match event {
            events::AppEvent::NeedsUser { conversation_id: id, attention, generation } if id == conversation_id => {
                Some((attention, generation))
            }
            _ => None,
        })
        .collect()
}

/// Waits (up to 10 s) for the busy spell that ends now to publish its
/// `NeedsUser`, polling so a turn spawned by `start_turn` finishes on
/// this current-thread runtime. Panics with what was seen if none comes.
async fn wait_needs_user(recording: &events::AppRecording, conversation_id: i64) -> (events::Attention, u64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(first) = needs_user_of(recording, conversation_id).into_iter().next() {
            return first;
        }
        assert!(std::time::Instant::now() < deadline, "no NeedsUser within 10 s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits (up to 10 s) for `conversation_id` to look idle again.
async fn wait_turn_done(conversation_id: i64) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while turn_running(conversation_id) {
        assert!(std::time::Instant::now() < deadline, "the turn never ended");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Waits (up to 10 s) for `conversation_id`'s busy spell to end and says
/// what, if anything, it published: the nothing-published tests' judge.
async fn published_after_done(recording: &events::AppRecording, conversation_id: i64) -> Vec<(events::Attention, u64)> {
    wait_turn_done(conversation_id).await;
    // The last drop's publishes happen after the count goes to 0, so
    // give them this runtime's next turns.
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    needs_user_of(recording, conversation_id)
}

/// The plan's first case, for each of the four callers: a turn that
/// saved a reply ends the busy spell with one `NeedsUser(Finished)`,
/// after `TurnsChanged`, carrying the spell's generation — and once.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_finished_turn_publishes_needs_user_finished_once_after_turns_changed(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_mock_upstream(&pool, vec![done_body("Done.")]).await;
    // The one stream a chat tab reads, alive before the busy spell so
    // every kind of it lands on one merged view (SME-71 review 1).
    let relay = conversation_relay(conversation.id, events::subscribe(conversation.id), events::subscribe_app());
    futures_util::pin_mut!(relay);
    let recording = events::AppRecording::start();

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    let (attention, generation) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Finished);
    // Once: nothing more for a while after the first.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = needs_user_of(&recording, conversation.id);
    assert_eq!(seen.len(), 1, "one NeedsUser per busy spell, saw {seen:?}");
    assert_eq!(generation, turn_generation(conversation.id), "the spell's generation");
    // The ordering the plan pins. The recording is this thread's own
    // publish list, so this is the publish order itself: both
    // `TurnsChanged`s — the busy spell's, then the idle one — come
    // before the `NeedsUser` on the app-wide bus. The plan's mutation —
    // publishing it inside the critical section, before the idle one —
    // puts the `NeedsUser` second and lands this red. (A first/last
    // match alone was the review's Medium: it matched the busy spell's
    // `TurnsChanged`, which precedes any `NeedsUser` by construction.)
    let app_events = recording.events();
    let needs_at = app_events
        .iter()
        .position(|e| matches!(e, events::AppEvent::NeedsUser { conversation_id, .. } if *conversation_id == conversation.id))
        .expect("the NeedsUser");
    let turns_before = app_events[..needs_at]
        .iter()
        .filter(|e| matches!(e, events::AppEvent::TurnsChanged))
        .count();
    assert!(turns_before >= 2, "the NeedsUser follows both TurnsChanged — busy, then idle — saw {app_events:?}");
    // The same order as a tab sees it, on the one merged
    // `conversation_relay` stream. The relay merges two channels with
    // `tokio::select!` (`api/chat.rs`), so what survives the merge is
    // each channel's own FIFO order — not publish order across
    // channels. `NeedsUser` and `TurnsChanged` ride the app channel
    // together, so on the merged stream the `NeedsUser` follows the
    // busy and idle `TurnsChanged` in their publish order (the
    // mutation — publishing it inside the critical section, before the
    // idle one — lands this red with one `TurnsChanged` before it).
    let mut relayed: Vec<events::ConversationEvent> = Vec::new();
    loop {
        let complete = relayed
            .iter()
            .any(|e| matches!(e, events::ConversationEvent::NeedsUser { conversation_id, .. } if *conversation_id == conversation.id))
            && relayed
                .iter()
                .any(|e| matches!(e, events::ConversationEvent::TurnState { running: false, .. }));
        if complete {
            break;
        }
        match tokio::time::timeout(Duration::from_secs(2), relay.next()).await {
            Ok(Some(Ok(event))) => relayed.push(event),
            _ => break,
        }
    }
    let needs_at = relayed
        .iter()
        .position(|e| matches!(e, events::ConversationEvent::NeedsUser { conversation_id, .. } if *conversation_id == conversation.id))
        .expect("the relayed NeedsUser");
    let turns_before = relayed[..needs_at]
        .iter()
        .filter(|e| matches!(e, events::ConversationEvent::TurnsChanged {}))
        .count();
    assert!(turns_before >= 2, "the relayed NeedsUser follows the busy and idle TurnsChanged, saw {relayed:?}");
    // The conversation channel's idle `TurnState` is judged for arrival
    // only, never for order: the first run of this assertion saw it land
    // on the merged stream after the `NeedsUser` — published later, but
    // the select drained the app channel's backlog first — and a select
    // merge promises no cross-channel order. A tab learns "the turn
    // ended" from the `TurnsChanged` above, the same channel the
    // `NeedsUser` rides.
    let idle_seen = relayed
        .iter()
        .any(|e| matches!(e, events::ConversationEvent::TurnState { running: false, .. }));
    assert!(idle_seen, "the tab saw the turn end on its stream, saw {relayed:?}");
    events::forget(conversation.id);
}

/// The answer's own turn, replying, ends Finished (the plan's list names
/// `answer_question` as one of the four callers).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_answer_turn_that_replies_publishes_finished(pool: PgPool) {
    use crate::questions::QuestionAnswer;
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    db::create_pending_question(
        &pool,
        conversation.id,
        "toolu_ask",
        &[crate::questions::Question {
            question: "Which one?".to_string(),
            header: "Which".to_string(),
            options: vec![crate::questions::QuestionOption { label: "One".to_string(), description: None }],
            multi_select: false,
        }],
    )
    .await
    .expect("pending question");
    start_mock_upstream(&pool, vec![done_body("One it is.")]).await;
    let recording = events::AppRecording::start();

    answer_question(
        pool.clone(),
        conversation.id,
        "toolu_ask".to_string(),
        vec![QuestionAnswer { selected: vec!["One".to_string()], other: None }],
    )
    .await
    .expect("answer");
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Finished);
}

/// A delivered notice whose turn replies: Finished, for `deliver_notice`.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_notice_turn_that_replies_publishes_finished(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_mock_upstream(&pool, vec![done_body("Command noted.")]).await;
    let recording = events::AppRecording::start();

    deliver_notice(&pool, conversation.id, "Terminal command cmd-1 finished: exit code 0.".to_string()).await;
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Finished);
}

/// A wake with a pending command notice, replying: Finished — the real
/// end of the command the user waited on.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_turn_that_replies_publishes_finished(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-1").await;
    start_mock_upstream(&pool, vec![done_body("Noted.")]).await;
    let recording = events::AppRecording::start();

    wake_conversation(&pool, conversation.id).await.expect("wake");
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Finished);
}

/// The ticket's reason to exist: an `ask_user` pauses the conversation,
/// and the spell ends `Question`.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_asked_question_publishes_needs_user_question(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_mock_upstream(&pool, vec![ask_body()]).await;
    let recording = events::AppRecording::start();

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Question);
    // Exactly one, and it's the Question: the turn's Ok end saves the
    // ask_user reply too. Honest reach (review 1): this count cannot pin
    // the wait-aware rule — if the Ok arm did note a Finished, `max`
    // coalesces it under the Question and the spell still publishes this
    // one event — so this test never goes red on that mutation. The
    // red-capable pin of the ruling is the ask_parent test below: no
    // Question is noted there, and a Finished would show plain.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = needs_user_of(&recording, conversation.id);
    assert_eq!(seen.len(), 1, "one Question, nothing alongside, saw {seen:?}");
    assert_eq!(seen[0].0, events::Attention::Question);
}

/// The ruling's first path: a child's turn ending on `ask_parent` waits
/// on its parent, not the user — the busy spell ends owing nothing.
/// (Red on the pre-ruling code: a Finished arrives for the saved
/// tool_use reply within seconds.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turn_ending_on_ask_parent_publishes_nothing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let parent = db::create_conversation(&pool).await.expect("parent");
    let child = active_child(&pool, parent.id).await;
    start_mock_upstream(&pool, vec![ask_parent_body()]).await;
    let recording = events::AppRecording::start();

    start_turn(pool.clone(), child, "go".to_string()).await.expect("send");
    // The wait is real before the spell ends: the row exists, parent-row.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let row = db::get_pending_question(&pool, child).await.expect("pending question");
        if row.is_some() {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the ask_parent wait never landed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        published_after_done(&recording, child).await.is_empty(),
        "a child waiting on its parent never notifies"
    );
}

/// A child of `parent` made directly, active and started, as the agents
/// tests make one: `ask_parent` needs the agent row, not a real start.
async fn active_child(pool: &PgPool, parent: i64) -> i64 {
    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('c', $1) RETURNING id")
        .bind(parent)
        .fetch_one(pool)
        .await
        .expect("a child");
    sqlx::query("INSERT INTO agents (conversation_id, root_id, depth, brief, state, started_at) VALUES ($1, $2, 1, 'b', 'active', now())")
        .bind(child)
        .bind(parent)
        .execute(pool)
        .await
        .expect("its agent row");
    child
}

/// A reply that calls `ask_parent` and ends the turn on it.
fn ask_parent_body() -> String {
    sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_askp","name":"ask_parent","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"question\":\"Which reading?\"}"}}"#,
        ),
        ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ])
}

/// A reply cut off at its budget gets the notice, and the spell ends
/// CutOff even though the turn itself ended Ok (the maximum at work:
/// CutOff outranks Finished).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cut_off_reply_publishes_cutoff(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let body = text_reply_body("half a reply").replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    start_mock_upstream(&pool, vec![body]).await;
    let recording = events::AppRecording::start();

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::CutOff);
}

/// A refused reply, same shape: Refused.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refused_reply_publishes_refused(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let body = text_reply_body("I can't help with that").replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"refusal""#);
    start_mock_upstream(&pool, vec![body]).await;
    let recording = events::AppRecording::start();

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Refused);
}

/// A user's failed turn — its failure notice saved, so it ends
/// `TurnFailure::Noted` — still means Failed. (The plan's mutation:
/// matching only `Failed` misses this one.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_user_turn_failing_with_its_notice_saved_publishes_failed(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_mock_upstream_failing_n_times(&pool, 100, None).await;
    let recording = events::AppRecording::start();

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Failed);
    let seen = needs_user_of(&recording, conversation.id);
    assert_eq!(seen.len(), 1, "one NeedsUser, saw {seen:?}");
}

/// A notice's turn failing (`deliver_notice`'s `NotificationDeliveryFailed`
/// case): Failed.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failing_notice_turn_publishes_failed(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_mock_upstream_failing_n_times(&pool, 100, None).await;
    let recording = events::AppRecording::start();

    deliver_notice(&pool, conversation.id, "Terminal command cmd-1 finished: exit code 0.".to_string()).await;
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Failed);
}

/// A wake's turn failing: Failed.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failing_wake_publishes_failed(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-1").await;
    start_mock_upstream_failing_n_times(&pool, 100, None).await;
    let recording = events::AppRecording::start();

    wake_conversation(&pool, conversation.id).await.expect_err("the failing call fails the wake");
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Failed);
}

/// `MAX_TURNS`: driven by `run_turn_bounded`'s own bound, a tool-looping
/// reply with `max_turns: 1` fails the turn — and Failed is what the
/// spell ends with.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_exceeding_max_turns_publishes_failed(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    // Every request asks for a tool call, so the loop never reaches a
    // final reply; one step is all the bound allows.
    let body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        (
            "content_block_start",
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_x","name":"nonexistent_tool","input":{}}}"#,
        ),
        (
            "content_block_delta",
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
        ),
        ("content_block_stop", r#"{"type":"content_block_stop","index":0}"#),
        (
            "message_delta",
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
        ),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    start_mock_upstream(&pool, vec![body]).await;
    let recording = events::AppRecording::start();

    let (in_flight, generation) = TurnInFlight::start_by_user(conversation.id);
    let result = run_turn_bounded(&pool, in_flight, Some(hello()), 1, Some(generation), true).await;
    assert!(result.is_err(), "one allowed step can't finish a tool-looping turn");
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Failed);
}

/// The maximum, not the last word: a Failed early in a busy spell is
/// what ends it, whatever replies come after in the same spell.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_failed_beats_a_later_finished_in_one_busy_spell(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let recording = events::AppRecording::start();

    // Two turns, one spell: the first notes Failed (a failing run's end),
    // the second notes Finished after it (a reply saved). With a plain
    // overwrite the spell would end Finished; the maximum keeps Failed.
    let first = TurnInFlight::start(conversation.id);
    note_attention(conversation.id, events::Attention::Failed);
    let second = TurnInFlight::start(conversation.id);
    note_attention(conversation.id, events::Attention::Finished);
    drop(first);
    assert!(needs_user_of(&recording, conversation.id).is_empty(), "nothing while a turn is still in flight");
    drop(second);
    let seen = needs_user_of(&recording, conversation.id);
    assert_eq!(seen, vec![(events::Attention::Failed, turn_generation(conversation.id))]);
}

/// A Stop mid-turn publishes nothing: the user is there.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_mid_turn_publishes_nothing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_partial_then_hanging_mock_upstream(&pool, "streaming…").await;
    let recording = events::AppRecording::start();

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    // Let the turn reach its model call, then stop it there.
    tokio::time::sleep(Duration::from_millis(300)).await;
    stop_turn_now(conversation.id);
    assert!(published_after_done(&recording, conversation.id).await.is_empty(), "a stop ends the spell with nothing owed");
}

/// A Stop after a Question in the same busy spell: the debt goes with
/// it (the plan's mutation: without the clear on `Err(Stopped)` this
/// publishes Question).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_after_a_question_publishes_nothing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_partial_then_hanging_mock_upstream(&pool, "streaming…").await;
    let recording = events::AppRecording::start();

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    tokio::time::sleep(Duration::from_millis(300)).await;
    // A question waits now (as `ask` would have noted it); then the Stop.
    note_attention(conversation.id, events::Attention::Question);
    stop_turn_now(conversation.id);
    assert!(published_after_done(&recording, conversation.id).await.is_empty(), "a Stop clears the spell's Question");
}

/// A Stop catching a queued message (the SME-170 path): the message is
/// saved for the model, and nothing is announced to the user.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_catching_a_queued_message_publishes_nothing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    // Hold the turn lock so the send queues behind it.
    let held = conversation_lock(conversation.id).clone().lock_owned().await;
    let recording = events::AppRecording::start();

    start_turn(pool.clone(), conversation.id, "queued".to_string()).await.expect("send queued");
    // Only the stop is ready when the queued turn reaches its select.
    stop_turn_now(conversation.id);
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    drop(held);
    assert!(published_after_done(&recording, conversation.id).await.is_empty(), "a queued stop ends with nothing owed");
}

/// A notice saved while a question waits runs no turn the user waits
/// for: nothing is announced (the reply is held for the answer).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_notice_while_a_question_waits_publishes_nothing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    db::create_pending_question(
        &pool,
        conversation.id,
        "toolu_ask",
        &[crate::questions::Question {
            question: "Which one?".to_string(),
            header: "Which".to_string(),
            options: vec![crate::questions::QuestionOption { label: "One".to_string(), description: None }],
            multi_select: false,
        }],
    )
    .await
    .expect("pending question");
    start_mock_upstream(&pool, vec![done_body("unused — the turn stops at the question")]).await;
    let recording = events::AppRecording::start();

    deliver_notice(&pool, conversation.id, "Terminal command cmd-1 finished: exit code 0.".to_string()).await;
    assert!(
        published_after_done(&recording, conversation.id).await.is_empty(),
        "a notice held for a waiting question announces nothing"
    );
}

/// A wake with nothing to do is never counted (SME-218), so it notes and
/// publishes nothing — no `TurnsChanged` either.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_no_op_wake_publishes_nothing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let recording = events::AppRecording::start();

    let result = wake_conversation(&pool, conversation.id).await.expect("nothing to do");
    assert!(result.is_empty());
    for _ in 0..3 {
        tokio::task::yield_now().await;
    }
    assert!(recording.events().is_empty(), "a wake with nothing to do announces nothing, saw {:?}", recording.events());
}

/// A turn's state is still freed once its published outcome is all it
/// held against a delete: the attention taken by the last drop doesn't
/// pin the entry.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_runtime_entry_is_freed_after_a_delete_and_a_published_outcome(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_mock_upstream(&pool, vec![done_body("Done.")]).await;

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    wait_turn_done(conversation.id).await;

    db::delete_conversation(&pool, conversation.id).await.expect("delete");
    forget_conversation_lock(conversation.id);
    assert!(!has_turn_state(conversation.id), "the entry goes with the conversation");
    events::forget(conversation.id);
}

/// `turn_generation`: 0 with no entry, and the counter the events carry.
#[test]
fn test_turn_generation_is_zero_with_no_entry_and_follows_the_takers() {
    let id = 9_071_888_001;
    assert_eq!(turn_generation(id), 0, "no entry, no generation");
    assert_eq!(new_turn_generation(id), 1, "a user's turn takes 1");
    assert_eq!(turn_generation(id), 1);
    let held = conversation_lock(id);
    assert_eq!(take_turn_generation(id, &held), Some(2), "a turn holding the lock takes the next");
    assert_eq!(turn_generation(id), 2);
    forget_turn_generation(id);
    assert_eq!(turn_generation(id), 0, "a turn that found its conversation deleted leaves no entry behind");
}

/// The SME-73 pair, first half: a turn that overflows once, compacts and
/// answers ends Finished — the recovery's refused call notes no Failed.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turn_that_overflows_once_publishes_finished_not_failed(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    super::overflow::start_refusal_mock(
        &pool,
        vec![
            super::overflow::too_long(120_000, 131_072),
            Ok(text_reply_body("Summary: it was going.")),
            Ok(text_reply_body("Done now.")),
        ],
    )
    .await;
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    db::create_message(&pool, conversation.id, "user", &[crate::anthropic::ContentBlock::Text { text: "earlier".to_string() }])
        .await
        .expect("seed");
    db::create_message(&pool, conversation.id, "assistant", &[crate::anthropic::ContentBlock::Text { text: "reply".to_string() }])
        .await
        .expect("seed");
    let recording = events::AppRecording::start();

    run_turn(&pool, conversation.id, hello()).await.expect("the refusal recovers through a compaction");
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Finished, "a recovered overflow is a normal ending");
    let seen = needs_user_of(&recording, conversation.id);
    assert_eq!(seen.len(), 1, "one Finished for the spell, no Failed alongside: {seen:?}");
}

/// The SME-73 pair, second half: a turn refused a second time fails —
/// Failed is what the user needs to be told.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turn_refused_twice_publishes_failed(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    super::overflow::start_refusal_mock(&pool, vec![super::overflow::too_long(150_000, 131_072), super::overflow::too_long(150_000, 131_072)])
        .await;
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    db::create_message(&pool, conversation.id, "user", &[crate::anthropic::ContentBlock::Text { text: "earlier".to_string() }])
        .await
        .expect("seed");
    db::upsert_conversation_usage(
        &pool,
        conversation.id,
        &crate::anthropic::TokenUsage {
            input_tokens: 190_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    )
    .await
    .expect("seed usage");
    let recording = events::AppRecording::start();

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("the summary refused twice fails the turn");
    assert!(error.to_string().contains("Couldn't compact the conversation"), "{error}");
    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Failed, "the second refusal ends the spell Failed");
    let seen = needs_user_of(&recording, conversation.id);
    assert_eq!(seen.len(), 1, "one Failed for the spell: {seen:?}");
}

// --- An idle-notice turn notes no Finished (SME-370) ---

/// The ticket's reason to exist: a wake whose drain saved an idle notice is
/// smelt telling itself about a command that keeps running, not an answer
/// the user was waiting on — the busy spell ends owing nothing, and the
/// repeated pings never reach the desktop. (Prediction, red on the unfixed
/// code: the `is_empty` fails naming one event, `[(Finished, …)]`. The
/// actual red is recorded beside it in What shipped.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_idle_notice_wake_publishes_nothing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    untouched_running_command(&pool, conversation.id, "cmd-idle", 601).await;
    start_mock_upstream(&pool, vec![done_body("Still running.")]).await;
    let recording = events::AppRecording::start();

    wake_conversation(&pool, conversation.id).await.expect("wake");

    let seen = published_after_done(&recording, conversation.id).await;
    assert!(seen.is_empty(), "an idle-notice turn notifies nothing, saw {seen:?}");
}

/// The mixed-drain call (SME-370's Trade-offs): a turn whose drain carried a
/// finished command's notice *and* an idle notice — the command ended during
/// that same turn — still notes nothing. The miss is toward quiet, and the
/// command's user got its output in the transcript either way. (Prediction,
/// red before the fix: one Finished event.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_with_a_finished_command_and_an_idle_notice_publishes_nothing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    finished_and_idle_commands(&pool, conversation.id).await;
    start_mock_upstream(&pool, vec![done_body("Both noted.")]).await;
    let recording = events::AppRecording::start();

    wake_conversation(&pool, conversation.id).await.expect("wake");

    let seen = published_after_done(&recording, conversation.id).await;
    assert!(
        seen.is_empty(),
        "an idle notice in the drain takes the Finished with it, saw {seen:?}"
    );
}

/// What an idle-notice turn still notifies: a question it asked still
/// reaches the user. The guard is the Finished arm's alone — `ask` notes the
/// Question at ask time, whatever else the turn saved. (Cannot go red
/// pre-fix, as predicted: Question already outranks Finished. Honest reach
/// as in the `ask_user` test above — it bites if the guard is over-plumbed,
/// e.g. moved into `note_attention`, where the ask's note would vanish
/// with it.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_question_asked_on_an_idle_notice_turn_publishes_question(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    untouched_running_command(&pool, conversation.id, "cmd-idle", 601).await;
    start_mock_upstream(&pool, vec![ask_body()]).await;
    let recording = events::AppRecording::start();

    wake_conversation(&pool, conversation.id).await.expect("the waking turn asks");

    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Question);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = needs_user_of(&recording, conversation.id);
    assert_eq!(seen.len(), 1, "one Question, nothing alongside, saw {seen:?}");
    assert_eq!(seen[0].0, events::Attention::Question);
}

/// And a failing idle-notice turn still notifies: the error arm notes Failed
/// whatever the turn saved. (Same honest reach as the question test — cannot
/// go red pre-fix — and it bites if the guard is over-plumbed into skipping
/// every note for such a turn.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failing_idle_notice_wake_publishes_failed(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    untouched_running_command(&pool, conversation.id, "cmd-idle", 601).await;
    start_mock_upstream_failing_n_times(&pool, 100, None).await;
    let recording = events::AppRecording::start();

    wake_conversation(&pool, conversation.id).await.expect_err("the failing call fails the wake");

    let (attention, _) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Failed);
}
