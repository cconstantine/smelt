//! SME-386 F10: a turn can't keep compacting for nothing. Where compaction
//! can't leave room to work in, it's skipped until the request really nears
//! the window, across wakes too; a request that can't fit even compacted
//! ends the turn and says why; and a compaction that frees real room goes
//! on as often as the turn needs it.

use super::ask_user::tool_calls_body;
use super::overflow::{Answer, compaction_summaries, ollama_refused, start_refusal_mock, too_long};
use super::window_stop::set_context_window;
use super::*;

/// A step that calls `todoread` (cheap, database only), measuring `input`
/// tokens in and `output` out.
pub(super) fn step(input: i64, output: i64) -> String {
    with_usage(&tool_calls_body(&[("toolu_01", "todoread", serde_json::json!({}))]), input, 0, output)
}

/// What the stepping mock saw.
#[derive(Default)]
pub(super) struct Seen {
    /// Requests that weren't summaries, in order.
    steps: std::sync::Mutex<Vec<serde_json::Value>>,
}

impl Seen {
    pub(super) fn count(&self) -> usize {
        self.steps.lock().expect("the request log").len()
    }

    pub(super) fn max_tokens(&self, i: usize) -> u64 {
        self.steps.lock().expect("the request log")[i]["max_tokens"].as_u64().expect("max_tokens")
    }
}

/// A mock answering each model step with the next of `steps`, then "Done.",
/// and each summary request with a summary, in whatever order the turn asks:
/// whether a compaction runs between two steps is what's under test.
pub(super) async fn stepping_mock(pool: &PgPool, steps: Vec<String>) -> Arc<Seen> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let seen = Arc::new(Seen::default());
    let log = seen.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |body: String| {
            let log = log.clone();
            let steps = steps.clone();
            async move {
                let parsed: serde_json::Value = serde_json::from_str(&body).expect("a JSON request body");
                let reply = if parsed["system"].to_string().contains("You are compacting") {
                    text_reply_body("The summary.")
                } else {
                    let mut log = log.steps.lock().expect("the request log");
                    log.push(parsed);
                    steps.get(log.len() - 1).cloned().unwrap_or_else(|| text_reply_body("Done."))
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

pub(super) async fn summaries(pool: &PgPool, id: i64) -> usize {
    compaction_summaries(pool, id).await.len()
}

/// The finding's own shape: each compacted request is over the trigger, so
/// compacting can't help. It fits the window, though, so the turn goes on,
/// without paying for a summary at every step (M3).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_request_over_the_trigger_that_still_fits_stops_compacting_and_carries_on(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    // The trigger at 28,000 is 11,616.
    let seen = stepping_mock(&pool, vec![step(12_380, 100); 12]).await;
    set_context_window(&pool, 28_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("the request still fits");

    assert_eq!(summaries(&pool, conversation.id).await, 2, "two results in a row show it can't help");
    assert_eq!(seen.count(), 13, "every step ran, and the final reply");
}

/// Review M1: what's judged is the room left, not the size of the last step.
/// A 2,100-token step used to read as freeing 2,100 tokens, and compact every
/// step with 616 tokens of room under the trigger.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_compactions_leaving_little_room_stop_however_big_the_step(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let seen = stepping_mock(&pool, vec![step(11_000, 2_100); 12]).await;
    set_context_window(&pool, 28_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("the request still fits");

    assert_eq!(summaries(&pool, conversation.id).await, 2);
    assert_eq!(seen.count(), 13);
}

/// Skipped until the request nears the window: then compaction frees what
/// the turn has built up, so it runs, and each request keeps inside the
/// window with the reply it asks for, which the provider's limit needs.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_skipped_compaction_runs_near_the_window_and_every_request_fits(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let seen = stepping_mock(
        &pool,
        // The limit at 28,000 is 21,856: the window less the buffer, less
        // room for a short reply. Step 4 is under it and step 5 over.
        vec![step(12_380, 100), step(12_380, 100), step(12_380, 100), step(20_000, 100), step(22_000, 100), step(12_380, 100)],
    )
    .await;
    set_context_window(&pool, 28_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("the turn gets there");

    assert_eq!(summaries(&pool, conversation.id).await, 3, "two to learn it can't help, one at the limit");
    assert_eq!(seen.count(), 7);
    // The request after step 4 goes out at about 20,100 tokens.
    let reply = seen.max_tokens(4);
    assert!(reply + 20_100 + u64::from(COMPACTION_SAFETY_BUFFER) <= 28_000, "{reply} reply tokens");
    assert!(reply >= 2_048 - 200, "room for a short reply: {reply}");
}

/// A compaction that does leave room to work in clears what was learned, as
/// when the user removes an MCP server's tools.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_compaction_that_leaves_room_goes_back_to_the_usual_trigger(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let steps = vec![step(12_380, 100), step(12_380, 100), step(12_380, 100), step(22_000, 100), step(6_000, 100), step(12_000, 100)];
    let seen = stepping_mock(&pool, steps).await;
    set_context_window(&pool, 28_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("the turn gets there");

    // Two to learn, one at the limit, and at 12,100, over the usual trigger
    // again once the 6,000 has shown there's room, a fourth.
    assert_eq!(summaries(&pool, conversation.id).await, 4);
    assert_eq!(seen.count(), 7);
}

/// A long ask whose steps read big outputs (F11's nine steps, 11 compactions
/// at 40,000): each compaction leaves real room, so the turn is getting
/// somewhere however often it compacts.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_compactions_that_leave_real_room_go_on_as_often_as_needed(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    stepping_mock(&pool, vec![step(15_000, 9_000); 8]).await;
    set_context_window(&pool, 40_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("every compaction left real room");

    assert_eq!(summaries(&pool, conversation.id).await, 8);
}

/// One tight result isn't a pattern (L1: a compaction's quotes of unanswered
/// messages inflate only the request right after it).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_one_tight_compaction_in_a_row_learns_nothing(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    // Over the 23,616 trigger after each step. After a compaction: tight
    // (23,000), then roomy (15,000), round and round.
    let round = [step(23_000, 9_000), step(15_000, 9_000)];
    stepping_mock(&pool, round.iter().cycle().take(8).cloned().collect()).await;
    set_context_window(&pool, 40_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("the turn ends");

    assert_eq!(summaries(&pool, conversation.id).await, 8, "it never stopped compacting");
}

/// A request that can't fit even compacted ends the turn, saying why. The
/// limit at 20,000 is 13,856: each step's 14,000 reaches it, 12,000 leaves under
/// 2,048 below it, and an estimate of about 11,000 leaves the model room to be
/// called, so what's measured decides this, not the floor.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_compacted_request_that_cant_fit_ends_the_turn_saying_why(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    stepping_mock(&pool, vec![step(12_000, 2_000); 12]).await;
    set_context_window(&pool, 20_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("it can't fit");

    let error = error.to_string();
    assert_eq!(summaries(&pool, conversation.id).await, 3, "{error}");
    assert!(error.contains("(20,000 tokens) is too small for this conversation"), "{error}");
    assert!(error.contains("about 12,000 tokens"), "it names what a compacted request measured: {error}");
}

/// H1: what a turn learned holds for the wakes after it: a command's finish
/// doesn't pay for a summary that can't help.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_remembers_that_compacting_cant_help(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let seen = stepping_mock(&pool, vec![step(12_380, 100); 6]).await;
    set_context_window(&pool, 28_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    run_turn(&pool, conversation.id, hello()).await.expect("the request still fits");
    assert_eq!(summaries(&pool, conversation.id).await, 2);
    let before = seen.count();

    unnotified_finished_command(&pool, conversation.id, "sme-386-h1").await;
    wake_conversation(&pool, conversation.id).await.expect("the wake's request still fits");

    assert_eq!(seen.count(), before + 1, "the wake called the model");
    assert_eq!(summaries(&pool, conversation.id).await, 2, "and paid for no summary");
}

/// H1's own shape, where the turn did fail: a wake finds it can't fit before
/// paying for a summary or a model step, again and again.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_wakes_after_a_turn_that_couldnt_fit_call_nothing(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let seen = stepping_mock(&pool, vec![step(12_000, 2_000); 30]).await;
    set_context_window(&pool, 20_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    run_turn(&pool, conversation.id, hello()).await.expect_err("it can't fit");
    let (calls, summarized) = (seen.count(), summaries(&pool, conversation.id).await);

    let pod = db::create_sandbox_pod(&pool, conversation.id).await.expect("create sandbox pod");
    let terminal = db::create_sandbox_terminal(&pool, pod.id).await.expect("create sandbox terminal");
    for round in 0..3 {
        let command = format!("sme-386-h1-{round}");
        db::create_terminal_command(&pool, conversation.id, terminal.id, &command, "echo hi", 300).await.expect("create terminal command");
        db::mark_terminal_command_finished(&pool, &command, 0).await.expect("mark terminal command finished");
        wake_conversation(&pool, conversation.id).await.expect_err("it still can't fit");
    }

    assert_eq!(seen.count(), calls, "no model step");
    assert_eq!(summaries(&pool, conversation.id).await, summarized, "no summary");
}

/// The user writing tries again, whatever a wake would have done, and a
/// turn that fails again pays for one summary rather than the three that
/// established it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_users_send_tries_again(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let seen = stepping_mock(&pool, vec![step(12_000, 2_000); 30]).await;
    set_context_window(&pool, 20_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    run_turn(&pool, conversation.id, hello()).await.expect_err("it can't fit");
    let (calls, summarized) = (seen.count(), summaries(&pool, conversation.id).await);
    let mut rx = events::subscribe(conversation.id);

    start_turn(pool.clone(), conversation.id, "Try again.".to_string()).await.expect("send");
    wait_until_idle(&mut rx).await;

    assert!(seen.count() > calls, "the model was called");
    assert_eq!(summaries(&pool, conversation.id).await, summarized + 1);
    let reason = last_failure_reason(&pool, conversation.id).await.expect("the conversation says why");
    assert!(reason.contains("is too small for this conversation"), "{reason}");
}

/// A larger window on the provider's page takes effect at once, without the
/// user writing: what was learned was learned of the old one.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_new_window_forgets_what_was_learned(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let seen = stepping_mock(&pool, vec![step(12_000, 2_000); 4]).await;
    set_context_window(&pool, 20_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    run_turn(&pool, conversation.id, hello()).await.expect_err("it can't fit");
    let calls = seen.count();
    sqlx::query("UPDATE provider_models SET context_window = 200000").execute(&pool).await.expect("a larger window");

    unnotified_finished_command(&pool, conversation.id, "sme-386-l5").await;
    wake_conversation(&pool, conversation.id).await.expect("it fits now");

    assert_eq!(seen.count(), calls + 1);
}

/// A compaction answering a refusal is judged like a proactive one.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_compaction_answering_a_refusal_counts_too(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let summary = || -> Answer { Ok(text_reply_body("The summary.")) };
    // Refused, summarized, retried: a tight result. Over the trigger, so a
    // second compaction: tight again. Then none, however many steps.
    let answers = vec![
        too_long(30_000, 40_000),
        summary(),
        Ok(step(23_000, 700)),
        summary(),
        Ok(step(23_000, 700)),
        Ok(step(23_000, 700)),
        Ok(step(23_000, 700)),
        Ok(text_reply_body("Done.")),
    ];
    start_refusal_mock(&pool, answers).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("the request still fits");

    assert_eq!(summaries(&pool, conversation.id).await, 2);
}

/// M2: a refusal naming no window has the turn assume one, and a healthy turn
/// is never failed against that guess, nor shown it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_guessed_window_never_fails_a_turn(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let summary = || -> Answer { Ok(text_reply_body("The summary.")) };
    // The refused request is about 16,500 tokens plus the prompt, so the
    // guess is under 22,000 and its limit under 16,000.
    let mut answers = vec![ollama_refused(), summary()];
    answers.extend(std::iter::repeat_with(|| Ok(step(16_000, 100))).take(6));
    answers.push(Ok(text_reply_body("Done.")));
    let mock = start_refusal_mock(&pool, answers).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    db::create_message(&pool, conversation.id, "user", &[text_block(&"x".repeat(66_000))]).await.expect("a paste");
    db::create_message(&pool, conversation.id, "assistant", &[text_block("Read it.")]).await.expect("a reply");

    let saved = run_turn(&pool, conversation.id, hello()).await.expect("nothing was refused after the first");

    assert_eq!(last_reply(&saved).as_deref(), Some("Done."));
    assert_eq!(summaries(&pool, conversation.id).await, 1, "and it asked for no more summaries against a guess");
    assert_eq!(mock.count(), 9);
}

/// L2: a server reporting no usage that refuses every step, naming nothing:
/// no measure can stop this, so the refusals themselves do.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_refusals_at_every_step_end_the_turn_without_naming_a_guess(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let tool = || tool_calls_body(&[("toolu_01", "todoread", serde_json::json!({}))]);
    let mut answers = Vec::new();
    for _ in 0..8 {
        answers.extend([ollama_refused(), Ok(text_reply_body("The summary.")), Ok(tool())]);
    }
    answers.push(Ok(text_reply_body("Done.")));
    let mock = start_refusal_mock(&pool, answers).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("it keeps being refused");

    let error = error.to_string();
    assert_eq!(summaries(&pool, conversation.id).await, 3, "{error}");
    assert!(error.contains("kept refusing"), "{error}");
    assert!(!error.contains(" tokens)"), "a guess is never named: {error}");

    // And a wake doesn't start it over.
    let calls = mock.count();
    unnotified_finished_command(&pool, conversation.id, "sme-386-l2").await;
    wake_conversation(&pool, conversation.id).await.expect_err("still");
    assert_eq!(mock.count(), calls);
}

/// A refusal now and then, many steps apart, is the window doing its job.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_step_the_server_took_starts_the_refusal_count_again(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let tool = || tool_calls_body(&[("toolu_01", "todoread", serde_json::json!({}))]);
    let mut answers = Vec::new();
    for _ in 0..5 {
        answers.extend([ollama_refused(), Ok(text_reply_body("The summary.")), Ok(tool()), Ok(tool())]);
    }
    answers.push(Ok(text_reply_body("Done.")));
    start_refusal_mock(&pool, answers).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("the server takes a step between");

    assert_eq!(summaries(&pool, conversation.id).await, 5);
}

/// L1: a compaction's quotes of what hasn't been answered have a total cap,
/// or 14 reports waiting would leave the compacted request 14,000 tokens big.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_what_a_continuation_quotes_of_unanswered_messages_has_a_total_cap(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    stepping_mock(&pool, vec![with_usage(&text_reply_body("Started them."), 30_000, 0, 100)]).await;
    set_context_window(&pool, 40_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    run_turn(&pool, conversation.id, hello()).await.expect("the first turn");
    for report in 0..14 {
        let text = format!("Report from agent #{report}: finding-{report} {}", "x".repeat(5_000));
        db::create_message(&pool, conversation.id, "user", &[text_block(&text)]).await.expect("a report");
    }

    run_turn(&pool, conversation.id, hello()).await.expect("the turn compacts and answers");

    let messages = db::list_messages(&pool, conversation.id).await.expect("messages");
    let continuation = messages
        .iter()
        .flat_map(|m| m.blocks().unwrap_or_default())
        .filter_map(|block| match block {
            anthropic::ContentBlock::CompactionPlaceholder { text } if text != COMPACTION_PLACEHOLDER_PROMPT => Some(text),
            _ => None,
        })
        .next_back()
        .expect("a continuation");
    // `typed_budget(40_000)` is 5,000 characters, for each of the two kinds.
    assert!(continuation.chars().count() <= 11_000, "{} chars", continuation.chars().count());
    assert!(continuation.contains("hello"), "the newest is the one to answer");
    assert!(continuation.contains("left out here"), "it says what it left out: {}", &continuation[continuation.len() - 300..]);
}

/// How many failure notices `id` has.
async fn failure_notices(pool: &PgPool, id: i64) -> usize {
    let messages = db::list_messages(pool, id).await.expect("messages");
    messages
        .iter()
        .flat_map(|m| m.blocks().unwrap_or_default())
        .filter(|block| matches!(block, anthropic::ContentBlock::Text { text } if crate::api::chat::parse_failure_notice(text).is_some()))
        .count()
}

/// L4: F10's "nothing tells the user why", on a root's wake: the banner is
/// live only, so the conversation says it, and once, however many wakes.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_root_whose_wakes_cant_fit_says_why_once(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    stepping_mock(&pool, vec![step(12_000, 2_000); 30]).await;
    set_context_window(&pool, 20_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    // A wake's turn, with no user's turn to save the reason for it.
    run_turn(&pool, conversation.id, hello()).await.expect_err("it can't fit");
    let reason = last_failure_reason(&pool, conversation.id).await.expect("the conversation says why");
    assert!(reason.contains("is too small for this conversation"), "{reason}");

    let pod = db::create_sandbox_pod(&pool, conversation.id).await.expect("create sandbox pod");
    let terminal = db::create_sandbox_terminal(&pool, pod.id).await.expect("create sandbox terminal");
    for round in 0..3 {
        let command = format!("sme-386-l4-{round}");
        db::create_terminal_command(&pool, conversation.id, terminal.id, &command, "echo hi", 300).await.expect("create terminal command");
        db::mark_terminal_command_finished(&pool, &command, 0).await.expect("mark terminal command finished");
        wake_conversation(&pool, conversation.id).await.expect_err("it still can't fit");
    }

    assert_eq!(failure_notices(&pool, conversation.id).await, 1);
}

/// What the user writes gets its own answer, the same words or not.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_each_of_the_users_failed_sends_says_why(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    stepping_mock(&pool, vec![step(12_000, 2_000); 30]).await;
    set_context_window(&pool, 20_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let mut rx = events::subscribe(conversation.id);
    for send in 1..=2 {
        start_turn(pool.clone(), conversation.id, "Try.".to_string()).await.expect("send");
        wait_until_idle(&mut rx).await;
        assert_eq!(failure_notices(&pool, conversation.id).await, send);
    }
}

/// A root's wake failing for any other reason is as it was: the banner.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_roots_wake_failing_otherwise_saves_no_notice(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    start_refusal_mock(&pool, vec![Err((axum::http::StatusCode::BAD_REQUEST, "MOCK-FAILURE".to_string()))]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    unnotified_finished_command(&pool, conversation.id, "sme-386-l4-other").await;

    wake_conversation(&pool, conversation.id).await.expect_err("the model call fails");

    assert_eq!(failure_notices(&pool, conversation.id).await, 0);
}

/// The same reason twice, with no model reply between, which only a failing
/// call gives: the user's send still gets its answer. Each is its own turn,
/// and `already_said` would otherwise see the first.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_users_send_failing_like_the_last_still_says_why(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    start_refusal_mock(&pool, vec![Err((axum::http::StatusCode::BAD_REQUEST, "MOCK-FAILURE".to_string()))]).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let mut rx = events::subscribe(conversation.id);
    for send in 1..=2 {
        start_turn(pool.clone(), conversation.id, "Try.".to_string()).await.expect("send");
        wait_until_idle(&mut rx).await;
        assert_eq!(failure_notices(&pool, conversation.id).await, send);
    }
}
