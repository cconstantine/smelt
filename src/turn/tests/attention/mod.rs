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

/// A reply that calls `notify_user` with `message` (SME-377).
fn notify_body(message: &str) -> String {
    super::ask_user::tool_calls_body(&[("toolu_notify", "notify_user", serde_json::json!({ "message": message }))])
}

/// T3 (SME-377): a turn whose reply calls `notify_user` ends there — one
/// model request, not a second after the result — and the busy spell
/// ends with one `NeedsUser(Notified)`, after `TurnsChanged`, carrying
/// the spell's generation, and `get_attention` answers with the model's
/// message. (SME-71's ordering checks, kept from the Finished test this
/// replaced.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_notify_user_turn_ends_and_publishes_needs_user_notified_once_after_turns_changed(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    // A second body, so a turn that doesn't end on the call asks again
    // and ends on the plain reply, rather than calling notify_user forever.
    let requests = start_mock_upstream(&pool, vec![notify_body("Built and tested"), done_body("And again.")]).await;
    // The one stream a chat tab reads, alive before the busy spell so
    // every kind of it lands on one merged view (SME-71 review 1).
    let relay = conversation_relay(conversation.id, events::subscribe(conversation.id), events::subscribe_app());
    futures_util::pin_mut!(relay);
    let recording = events::AppRecording::start();

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    let (attention, generation) = wait_needs_user(&recording, conversation.id).await;
    assert_eq!(attention, events::Attention::Notified);
    // Once: nothing more for a while after the first.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = needs_user_of(&recording, conversation.id);
    assert_eq!(seen.len(), 1, "one NeedsUser per busy spell, saw {seen:?}");
    assert_eq!(generation, turn_generation(conversation.id), "the spell's generation");
    assert_eq!(requests.load(Ordering::SeqCst), 1, "notify_user ends the turn: no model call after its result");
    let details = crate::api::notifications::attention_details(&pool, conversation.id, generation, events::Attention::Notified)
        .await
        .expect("get_attention");
    assert_eq!(details.and_then(|d| d.message), Some("Built and tested".to_string()), "the model's message");
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

/// T2 (SME-377), the user's complaint: a turn that only replies notes
/// nothing by itself. Here the user's send; the three tests below are
/// the other callers that reached the automatic Finished.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_send_turn_that_only_replies_publishes_nothing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_mock_upstream(&pool, vec![done_body("Done.")]).await;
    let recording = events::AppRecording::start();

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    let seen = published_after_done(&recording, conversation.id).await;
    assert!(seen.is_empty(), "a plain reply notifies nothing, saw {seen:?}");
}

/// The answer's own turn, replying: nothing (T2).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_answer_turn_that_only_replies_publishes_nothing(pool: PgPool) {
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
    let seen = published_after_done(&recording, conversation.id).await;
    assert!(seen.is_empty(), "an answer's plain reply notifies nothing, saw {seen:?}");
}

/// A delivered notice whose turn replies: nothing, for `deliver_notice` (T2).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_notice_turn_that_only_replies_publishes_nothing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    start_mock_upstream(&pool, vec![done_body("Command noted.")]).await;
    let recording = events::AppRecording::start();

    deliver_notice(&pool, conversation.id, "Terminal command cmd-1 finished: exit code 0.".to_string()).await;
    let seen = published_after_done(&recording, conversation.id).await;
    assert!(seen.is_empty(), "a notice's plain reply notifies nothing, saw {seen:?}");
}

/// A wake with a pending command notice, replying: nothing (T2) — the
/// ticket's case, a model waking on its command's end. Only its own
/// `notify_user` would tell the user.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_turn_that_only_replies_publishes_nothing(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    unnotified_finished_command(&pool, conversation.id, "cmd-1").await;
    start_mock_upstream(&pool, vec![done_body("Noted.")]).await;
    let recording = events::AppRecording::start();

    wake_conversation(&pool, conversation.id).await.expect("wake");
    let seen = published_after_done(&recording, conversation.id).await;
    assert!(seen.is_empty(), "a wake's plain reply notifies nothing, saw {seen:?}");
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
    // Exactly one, and it's the Question, noted at `ask`.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let seen = needs_user_of(&recording, conversation.id);
    assert_eq!(seen.len(), 1, "one Question, nothing alongside, saw {seen:?}");
    assert_eq!(seen[0].0, events::Attention::Question);
}

/// The ruling's first path: a child's turn ending on `ask_parent` waits
/// on its parent, not the user — the busy spell ends owing nothing.
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

    // Review round 1, L3: the delete and its forget land while a spell is
    // still running, before the model's notify_user notes its message
    // (`note_notified`, what the tool's run does). The spell's end must not
    // keep that message on the forgotten entry, or nothing ever frees it.
    // The call is made directly: a real turn started after the forget
    // takes a turn generation, which holds the entry with or without the
    // fix, so it couldn't show this.
    let raced = db::create_conversation(&pool).await.expect("conversation");
    let spell = TurnInFlight::start(raced.id);
    db::delete_conversation(&pool, raced.id).await.expect("delete");
    forget_conversation_lock(raced.id);
    note_notified(raced.id, "Done".to_string());
    drop(spell);
    assert!(!has_turn_state(raced.id), "a notify racing the delete strands no entry");
    events::forget(raced.id);
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


mod failures;
mod idle_notice;
mod notify_user;
