//! SME-386 round 2: replies held to the room left, against a mock that holds
//! them to `max_tokens` as a real server does. Each planned step wants so
//! many tokens, and one that doesn't get them stops at `max_tokens` with its
//! call dropped, whatever the test's other numbers say.

use super::ask_user::tool_calls_body;
use super::attention::published_after_done;
use super::overflow::{Answer, compaction_summaries, ollama_refused, start_refusal_mock, too_long};
use super::window_stop::set_context_window;
use super::*;

/// One model step: the input it measures and the tokens it wants to write.
#[derive(Clone, Copy)]
pub(super) struct Plan {
    pub(super) input: i64,
    pub(super) wants: i64,
}

pub(super) fn plan(input: i64, wants: i64) -> Plan {
    Plan { input, wants }
}

#[derive(Default)]
pub(super) struct Honest {
    steps: std::sync::Mutex<Vec<serde_json::Value>>,
    summaries: AtomicUsize,
}

impl Honest {
    pub(super) fn count(&self) -> usize {
        self.steps.lock().expect("the request log").len()
    }

    pub(super) fn max_tokens(&self, i: usize) -> u64 {
        self.steps.lock().expect("the request log")[i]["max_tokens"].as_u64().expect("max_tokens")
    }

    pub(super) fn request(&self, i: usize) -> String {
        self.steps.lock().expect("the request log")[i]["messages"].to_string()
    }

    pub(super) fn summary_calls(&self) -> usize {
        self.summaries.load(Ordering::SeqCst)
    }
}

/// A step that fails, its stream ending in an error.
const FAILS: i64 = -1;
/// A step cut off in the middle of a call to `write_file`.
const CUT_IN_WRITE_FILE: i64 = -2;
const OVERLOADED: &str = "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n";

fn cut_in_write_file(input: i64, output: i64) -> String {
    let body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        ("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_w","name":"write_file","input":{}}}"#),
        ("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"a.txt\",\"cont"}}"#),
        ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    with_usage(&body, input, 0, output)
}

/// Answers each model step from `plans`, in order, then "Done.", and each
/// summary request with a summary.
pub(super) async fn honest_mock(pool: &PgPool, plans: Vec<Plan>) -> Arc<Honest> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let seen = Arc::new(Honest::default());
    let log = seen.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |body: String| {
            let log = log.clone();
            let plans = plans.clone();
            async move {
                let parsed: serde_json::Value = serde_json::from_str(&body).expect("a JSON request body");
                let reply = if parsed["system"].to_string().contains("You are compacting") {
                    log.summaries.fetch_add(1, Ordering::SeqCst);
                    text_reply_body("The summary.")
                } else {
                    let max_tokens = parsed["max_tokens"].as_i64().expect("max_tokens");
                    let mut steps = log.steps.lock().expect("the request log");
                    steps.push(parsed);
                    match plans.get(steps.len() - 1) {
                        Some(Plan { wants: FAILS, .. }) => OVERLOADED.to_string(),
                        Some(Plan { input, wants: CUT_IN_WRITE_FILE }) => cut_in_write_file(*input, max_tokens),
                        Some(Plan { input, wants }) if *wants > max_tokens => with_usage(
                            &text_reply_body("Half a thought").replace("end_turn", "max_tokens"),
                            *input,
                            0,
                            max_tokens,
                        ),
                        Some(Plan { input, wants }) => {
                            with_usage(&tool_calls_body(&[("toolu_01", "todoread", serde_json::json!({}))]), *input, 0, *wants)
                        }
                        None => text_reply_body("Done."),
                    }
                };
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], reply)
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    crate::providers::test_support::add_mock_provider(pool, addr).await;
    seen
}

/// Into the state a window of 32,768 with 13,600 of fixed content reaches:
/// the trigger is 16,384, two compactions learn that compacting can't help.
fn learn_its_tight() -> Vec<Plan> {
    vec![plan(16_500, 100), plan(13_600, 3_000), plan(13_600, 100)]
}

async fn cut_off_notices(pool: &PgPool, id: i64) -> usize {
    let messages = db::list_messages(pool, id).await.expect("messages");
    messages
        .iter()
        .flat_map(|m| m.blocks().unwrap_or_default())
        .filter(|block| matches!(block, anthropic::ContentBlock::Text { text } if crate::api::chat::parse_cut_off_notice(text).is_some()))
        .count()
}

/// Round 2 M1: a reply the room cut off, when a compaction would free the
/// room, doesn't end a healthy turn: a child would stop there with no report.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_the_room_cut_off_compacts_and_goes_on(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mut plans = learn_its_tight();
    // At about 24,100 the room is 4,5xx, and this wants 6,000.
    plans.extend([plan(24_000, 100), plan(24_000, 6_000), plan(13_600, 6_000)]);
    let seen = honest_mock(&pool, plans).await;
    set_context_window(&pool, 32_768, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn goes on");

    assert_eq!(last_reply(&saved).as_deref(), Some("Done."), "the turn got to its end");
    assert_eq!(seen.count(), 7, "every planned step ran, and the final reply");
    assert_eq!(compaction_summaries(&pool, conversation.id).await.len(), 3, "two to learn it, one for the room");
    assert_eq!(cut_off_notices(&pool, conversation.id).await, 1, "the model and the user are told it was cut");
    assert!(seen.request(5).contains(crate::api::chat::CUT_OFF_NOTICE_START), "the retry carries the notice: {}", seen.request(5));
}

/// The case M1 is for: a big `write_file` cut in the middle. Its call is
/// dropped, the retry says which, and the model can write it again.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_write_file_the_room_cut_off_can_be_written_again(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mut plans = learn_its_tight();
    plans.extend([plan(24_000, 100), plan(24_000, CUT_IN_WRITE_FILE), plan(13_600, 6_000)]);
    let seen = honest_mock(&pool, plans).await;
    set_context_window(&pool, 32_768, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the turn goes on");

    assert_eq!(last_reply(&saved).as_deref(), Some("Done."));
    assert_eq!(seen.count(), 7);
    let retry = seen.request(5);
    assert!(retry.contains("in the middle of a call to `write_file`") && retry.contains("dropped and not run"), "{retry}");
}

/// It carries on, so nobody is called over to it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_cut_off_on_the_way_owes_the_user_nothing(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mut plans = learn_its_tight();
    plans.extend([plan(24_000, 100), plan(24_000, 6_000), plan(13_600, 100)]);
    honest_mock(&pool, plans).await;
    set_context_window(&pool, 32_768, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let recording = events::AppRecording::start();

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");

    assert_eq!(published_after_done(&recording, conversation.id).await, vec![], "nothing notifies");
    assert_eq!(cut_off_notices(&pool, conversation.id).await, 1);
}

/// Can't loop: a reply cut off again straight after that compaction ends the
/// turn as any cut-off reply does, and tells the user.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_cut_off_again_after_its_compaction_ends_the_turn(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mut plans = learn_its_tight();
    plans.extend([plan(24_000, 100), plan(24_000, 6_000), plan(13_600, 1_000_000), plan(13_600, 100)]);
    let seen = honest_mock(&pool, plans).await;
    set_context_window(&pool, 32_768, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("ends as a cut-off reply does");

    assert_eq!(seen.count(), 6, "no call after the second cut");
    assert_eq!(compaction_summaries(&pool, conversation.id).await.len(), 3);
    assert_eq!(cut_off_notices(&pool, conversation.id).await, 2);
}

/// Nothing to free: the request has barely grown since the last compaction,
/// so another would buy no room, and the turn doesn't pay for one.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_cut_off_with_nothing_to_free_ends_the_turn(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mut plans = learn_its_tight();
    plans.extend([plan(13_700, 1_000_000), plan(13_600, 100)]);
    let seen = honest_mock(&pool, plans).await;
    set_context_window(&pool, 32_768, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("ends as a cut-off reply does");

    assert_eq!(seen.count(), 4);
    assert_eq!(seen.summary_calls(), 2, "no third summary");
    assert_eq!(cut_off_notices(&pool, conversation.id).await, 1);
}

/// A reply cut off for any other reason is as it was: a cap the user set
/// isn't helped by more room.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_cut_off_at_its_output_cap_ends_the_turn(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    // Everything before it wants less than the cap of 3,500.
    let plans = vec![plan(16_500, 100), plan(13_600, 3_000), plan(13_600, 100), plan(19_000, 100), plan(19_000, 3_600), plan(13_600, 100)];
    let seen = honest_mock(&pool, plans).await;
    set_context_window(&pool, 32_768, Some(3_500)).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("ends as a cut-off reply does");

    assert_eq!(seen.count(), 5);
    assert_eq!(seen.summary_calls(), 2);
}

/// The window at which, with this conversation's fixed content, a compacted
/// request leaves `room` tokens for a reply.
async fn window_leaving(pool: &PgPool, id: i64, room: u64) -> i32 {
    let model = crate::providers::resolve_turn_model(pool, id).await.expect("a model");
    let (request, _) = build_request(pool, id, &model, None, &[], false).await.expect("a request");
    (estimate_request_tokens(&request) + u64::from(COMPACTION_SAFETY_BUFFER) + room) as i32
}

/// Round 2 M2: a request with no useful room isn't sent for one token of
/// reply. The user hears the window is too small, not "cut off at 1 tokens".
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_compacted_request_with_no_room_says_so_instead_of_calling(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let seen = honest_mock(&pool, vec![plan(20_000, 10), plan(20_000, 10), plan(20_000, 10), plan(20_000, 3_000)]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let window = window_leaving(&pool, conversation.id, 100).await;
    set_context_window(&pool, window, None).await;
    let mut rx = events::subscribe(conversation.id);

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    wait_until_idle(&mut rx).await;

    assert_eq!(seen.count(), 3, "no call with no room");
    assert_eq!(seen.summary_calls(), 3);
    assert_eq!(cut_off_notices(&pool, conversation.id).await, 0);
    let reason = last_failure_reason(&pool, conversation.id).await.expect("the conversation says why");
    assert!(reason.contains("is too small for this conversation"), "{reason}");
}

/// Enough room to send, too little for the reply, on the result that shows
/// the conversation doesn't fit: that's said now, not at a next step a
/// cut-off reply never reaches.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cut_off_reply_that_shows_it_doesnt_fit_says_why(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let seen = honest_mock(&pool, vec![plan(20_000, 10), plan(20_000, 10), plan(20_000, 10), plan(20_000, 3_000)]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let window = window_leaving(&pool, conversation.id, 1_500).await;
    set_context_window(&pool, window, None).await;
    let mut rx = events::subscribe(conversation.id);

    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    wait_until_idle(&mut rx).await;

    assert_eq!(seen.count(), 4);
    assert_eq!(cut_off_notices(&pool, conversation.id).await, 1, "what happened to the reply is still said");
    let reason = last_failure_reason(&pool, conversation.id).await.expect("the conversation says why");
    assert!(reason.contains("is too small for this conversation") && reason.contains("about 20,000 tokens"), "{reason}");
}

/// A window this small with a cap under the floor is the cap's doing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_small_output_cap_isnt_a_lack_of_room(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mut plans = learn_its_tight();
    plans.extend([plan(13_600, 100), plan(13_600, 100)]);
    plans.iter_mut().for_each(|p| p.wants = p.wants.min(500));
    plans[0].input = 16_500;
    plans[1].input = 16_500;
    let seen = honest_mock(&pool, plans).await;
    set_context_window(&pool, 32_768, Some(512)).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("a small cap is no failure");

    assert_eq!(seen.count(), 6);
}

/// Round 2 M4: a recovery whose retry is refused ends the turn, and still
/// counts its compaction, or every wake pays for another summary.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_retry_refused_again_counts_its_compaction(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mut answers = Vec::new();
    for _ in 0..8 {
        answers.extend([ollama_refused(), Ok(text_reply_body("The summary.")), ollama_refused()]);
    }
    let mock = start_refusal_mock(&pool, answers).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    run_turn(&pool, conversation.id, hello()).await.expect_err("refused twice");
    assert_eq!(compaction_summaries(&pool, conversation.id).await.len(), 1);

    let pod = db::create_sandbox_pod(&pool, conversation.id).await.expect("create sandbox pod");
    let terminal = db::create_sandbox_terminal(&pool, pod.id).await.expect("create sandbox terminal");
    for round in 0..5 {
        let command = format!("sme-386-m4-{round}");
        db::create_terminal_command(&pool, conversation.id, terminal.id, &command, "echo hi", 300).await.expect("create terminal command");
        db::mark_terminal_command_finished(&pool, &command, 0).await.expect("mark terminal command finished");
        wake_conversation(&pool, conversation.id).await.expect_err("still refused");
    }

    assert_eq!(compaction_summaries(&pool, conversation.id).await.len(), 3, "the rule of three holds for failed retries");
    // The turn and two wakes compact; the third wake is refused and stops
    // there; the last two call nothing.
    assert_eq!(mock.count(), 3 * 3 + 1);
}

/// Round 2 L1: a retry after a refusal keeps to the room as the request
/// before it did.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_retry_after_a_refusal_keeps_to_the_room(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let tool = |input, output| -> Answer { Ok(with_usage(&tool_calls_body(&[("toolu_01", "todoread", serde_json::json!({}))]), input, 0, output)) };
    let summary = || -> Answer { Ok(text_reply_body("The summary.")) };
    // The trigger at 40,000 is 23,616; 21,000 leaves too little room under it.
    let answers = vec![
        tool(24_000, 100),
        summary(),
        tool(21_000, 3_000),
        summary(),
        tool(21_000, 100),
        tool(30_000, 100),
        // Naming the same window and a small input: the budget's the problem.
        too_long(5_000, 40_000),
        Ok(text_reply_body("Done.")),
    ];
    let mock = start_refusal_mock(&pool, answers).await;
    set_context_window(&pool, 40_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("the retry is taken");

    assert_eq!(mock.count(), 8);
    // The retry goes out at about 30,100 of 40,000.
    let retried = mock.request(7)["max_tokens"].as_u64().expect("max_tokens");
    assert!(retried + 30_100 + u64::from(COMPACTION_SAFETY_BUFFER) <= 40_000, "{retried} reply tokens");
}

/// Review M-A: a compaction clears the usage row until a reply lands, so a
/// request can look over the floor with nothing compacted yet. The paste
/// below would be capped by the compaction: the turn compacts and goes on,
/// rather than saying compacting wouldn't help.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_big_paste_after_a_failed_call_compacts_rather_than_refusing(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mut plans = learn_its_tight();
    plans.extend([plan(27_000, 100), plan(0, FAILS), plan(13_600, 100)]);
    let seen = honest_mock(&pool, plans).await;
    set_context_window(&pool, 32_768, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    run_turn(&pool, conversation.id, hello()).await.expect_err("the call fails, just after a compaction");
    let (calls, summarized) = (seen.count(), seen.summary_calls());
    assert_eq!(summarized, 3);
    let mut rx = events::subscribe(conversation.id);

    start_turn(pool.clone(), conversation.id, format!("Look at this. {}", "x".repeat(80_000))).await.expect("send");
    wait_until_idle(&mut rx).await;

    assert_eq!(last_failure_reason(&pool, conversation.id).await, None);
    assert_eq!(seen.summary_calls(), summarized + 1, "it compacted");
    assert_eq!(seen.count(), calls + 2, "the planned step, and the final reply");
}

/// Review M-A: an estimate above real usage. What a reply measured after a
/// compaction is what's believed, and the request gets the room that shows.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_measured_size_outweighs_a_high_estimate(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    // The window leaves the estimate about 100 tokens, and each reply measures
    // 3,000. Two results learn it's tight; the 9,000 has it compact again.
    let plans = vec![plan(3_000, 10), plan(3_000, 10), plan(3_000, 10), plan(9_000, 10), plan(3_000, 10)];
    let seen = honest_mock(&pool, plans).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let window = window_leaving(&pool, conversation.id, 100).await;
    set_context_window(&pool, window, None).await;

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("what was measured says it fits");

    assert_eq!(last_reply(&saved).as_deref(), Some("Done."));
    assert_eq!(seen.count(), 6);
    let reply = seen.max_tokens(4);
    assert!(u64::from(MIN_USEFUL_REPLY) <= reply, "{reply} reply tokens");
    assert!(reply + 3_000 + u64::from(COMPACTION_SAFETY_BUFFER) <= window as u64, "{reply} reply tokens, in {window}");
}

/// Review M-A: however the floor fired, the user's next send reaches the
/// server, whose answer is what can move things.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_send_after_the_floor_fired_reaches_the_server(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let seen = honest_mock(&pool, vec![plan(20_000, 10), plan(20_000, 10), plan(20_000, 10), plan(2_000, 10)]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let window = window_leaving(&pool, conversation.id, 100).await;
    set_context_window(&pool, window, None).await;
    let mut rx = events::subscribe(conversation.id);
    start_turn(pool.clone(), conversation.id, "hi".to_string()).await.expect("send");
    wait_until_idle(&mut rx).await;
    assert_eq!(seen.count(), 3, "the floor fired");

    start_turn(pool.clone(), conversation.id, "Again.".to_string()).await.expect("send");
    wait_until_idle(&mut rx).await;

    assert_eq!(seen.count(), 5, "the planned step, and the final reply");
    let last = db::list_messages(&pool, conversation.id).await.expect("messages");
    assert_eq!(last_reply(&last).as_deref(), Some("Done."));
}

/// Review M-B: a refusal naming the window, in the state where compacting
/// can't help, isn't "too small for smelt's system prompt and tools": they
/// leave room for a short reply, and the compaction goes ahead.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_refusal_naming_the_window_compacts_when_a_short_reply_fits(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let tool = |input| -> Answer { Ok(with_usage(&tool_calls_body(&[("toolu_01", "todoread", serde_json::json!({}))]), input, 0, 100)) };
    let summary = || -> Answer { Ok(text_reply_body("The summary.")) };
    let answers = vec![
        tool(12_380),
        summary(),
        tool(12_380),
        summary(),
        tool(12_380),
        too_long(20_000, 28_000),
        summary(),
        Ok(text_reply_body("Done.")),
    ];
    let mock = start_refusal_mock(&pool, answers).await;
    set_context_window(&pool, 28_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("the compaction answers the refusal");

    assert_eq!(last_reply(&saved).as_deref(), Some("Done."));
    assert_eq!(mock.count(), 8);
}

/// Review L-2: `recover`'s retry has the floor too. A longer summary than the
/// one before has left no useful room, and no call goes out for one token.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_retry_after_a_refusal_has_the_floor_too(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let tool = || -> Answer { Ok(with_usage(&tool_calls_body(&[("toolu_01", "todoread", serde_json::json!({}))]), 20_000, 0, 10)) };
    let summary = || -> Answer { Ok(text_reply_body("The summary.")) };
    // The refusal names a window it doesn't lower, so the turn's own stands.
    let answers = vec![
        tool(),
        summary(),
        tool(),
        summary(),
        tool(),
        summary(),
        too_long(20_000, 900_000),
        Ok(text_reply_body(&"long ".repeat(4_000))),
        Ok(text_reply_body("Should never be asked.")),
    ];
    let mock = start_refusal_mock(&pool, answers).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let window = window_leaving(&pool, conversation.id, 1_600).await;
    set_context_window(&pool, window, None).await;

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("no room after the second summary");

    assert_eq!(mock.count(), 8, "no retry with no room");
    let error = error.to_string();
    assert!(error.contains("is too small for this conversation") && error.contains("about 20,000 tokens"), "{error}");
}

/// Once: the guard outlasts the turn. The retry's own cut ends the turn at the
/// cap, so `room_retried` is still set. The wake's request, at about 21,600,
/// has room for less than the cap, so the room cuts it, with plenty a
/// compaction would free, and it still doesn't get a second go until a reply
/// has finished.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_later_wake_doesnt_retry_until_a_reply_finishes(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mut plans = learn_its_tight();
    plans.extend([plan(24_000, 100), plan(24_000, 7_000), plan(13_600, 1_000_000), plan(13_600, 1_000_000)]);
    let seen = honest_mock(&pool, plans).await;
    set_context_window(&pool, 32_768, Some(8_000)).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    run_turn(&pool, conversation.id, hello()).await.expect("ends as a cut-off reply does");
    let (calls, summarized) = (seen.count(), seen.summary_calls());

    unnotified_finished_command(&pool, conversation.id, "sme-386-guard").await;
    wake_conversation(&pool, conversation.id).await.expect("ends as a cut-off reply does");

    assert_eq!(seen.count(), calls + 1, "one call, cut off");
    assert_eq!(seen.summary_calls(), summarized, "and no compaction for it");
    assert_eq!(cut_off_notices(&pool, conversation.id).await, 3);
}
