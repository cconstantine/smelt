//! A persona's token ceiling (SME-493): a seat over its `smelt.compact_at`
//! compacts before its next model call, over SME-482's `compact_on_request`
//! path, once it's two steps clear of the last compaction, and not again
//! while compacting can't get it under (the no-loop rule, SME-414's lesson).
//! The mock's window is 200,000, so its own trigger never fires here.

use super::ask_user::tool_calls_body;
use super::on_request::dividers;
use super::overflow::{Answer, start_refusal_mock};
use super::*;

/// The ceiling these tests set.
const AT: u64 = 40_000;

/// The divider's reason at `AT`.
const REASON: &str = "it passed its persona's 40,000-token ceiling";

/// Sets `conversation_id` to a top-level persona whose snapshot holds
/// `settings`, applied, as `agents::top_level` would leave it.
pub(super) async fn set_persona_settings(pool: &PgPool, conversation_id: i64, settings: serde_json::Value) {
    sqlx::query(
        "INSERT INTO top_level_personas (conversation_id, persona_name, persona_path, persona_body, persona_settings, credentials, applied_at)
         VALUES ($1, 'developer', '/workspace/app/.agents/personas/developer.md', 'Develop.', $2, '{}', now())",
    )
    .bind(conversation_id)
    .bind(settings)
    .execute(pool)
    .await
    .expect("a top-level persona row");
}

/// Sets `conversation_id`'s persona ceiling to `at` tokens.
pub(super) async fn set_persona_ceiling(pool: &PgPool, conversation_id: i64, at: u64) {
    set_persona_settings(pool, conversation_id, serde_json::json!({"compact_at": at})).await;
}

/// A reply calling `todoread` as call `id`, measuring `input` tokens.
fn todoread_at(id: &str, input: i64) -> String {
    with_usage(&tool_calls_body(&[(id, "todoread", serde_json::json!({}))]), input, 0, 10)
}

/// A text reply measuring `input` tokens.
fn text_at(text: &str, input: i64) -> String {
    with_usage(&text_reply_body(text), input, 0, 10)
}

/// Whether a recorded request is a summary call: no tools offered.
fn is_summary_call(request: &serde_json::Value) -> bool {
    request["tools"].as_array().is_none_or(Vec::is_empty)
}

/// The recorded requests' summary calls, by index.
fn summary_calls(requests: &[serde_json::Value]) -> Vec<usize> {
    requests.iter().enumerate().filter(|(_, r)| is_summary_call(r)).map(|(i, _)| i).collect()
}

/// How many `compaction` model-call rows `conversation_id` has, once one
/// is recorded (they're written beside the call, so polled for a while).
async fn compaction_rows(pool: &PgPool, conversation_id: i64) -> i64 {
    let mut rows = 0;
    for _ in 0..100 {
        rows = sqlx::query_scalar("SELECT count(*) FROM model_call_usage WHERE conversation_id = $1 AND kind = 'compaction'")
            .bind(conversation_id)
            .fetch_one(pool)
            .await
            .expect("the recorded calls");
        if rows > 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    rows
}

/// Every mock provider's `mock-model` at a 200,000 window: an upsert, as a
/// test may start a second mock.
async fn set_window(pool: &PgPool) {
    sqlx::query(
        "INSERT INTO provider_models (provider_id, model, context_window) SELECT id, $1, 200000 FROM inference_providers
         ON CONFLICT (provider_id, model) DO UPDATE SET context_window = EXCLUDED.context_window",
    )
    .bind(crate::providers::test_support::MOCK_MODEL)
    .execute(&*pool)
    .await
    .expect("seed the window");
}

/// A conversation on a recording mock of `bodies`, at a 200,000 window.
async fn on_mock(pool: &PgPool, bodies: Vec<String>) -> (i64, Arc<std::sync::Mutex<Vec<serde_json::Value>>>) {
    db::test_support::start_ids_clear_of_other_runs(pool).await.expect("clear ids");
    let requests = start_recording_mock_upstream(pool, bodies).await;
    set_window(pool).await;
    let conversation = db::create_conversation(pool).await.expect("create conversation");
    (conversation.id, requests)
}

/// Two tool steps, the second landing over `AT`: the script T1, T5 and T6
/// share (Decided (Driver) 2026-11-12: two steps, so the ≥2-steps rule
/// lets the ceiling fire), then the summary and a reply.
fn over_at_the_second_step() -> Vec<String> {
    vec![todoread_at("toolu_1", 30_000), todoread_at("toolu_2", 50_000), text_reply_body("SUMMARY-1"), text_at("done", 9_000)]
}

/// The four requests of a ceiling compaction at the third call: the two
/// steps, the summary (before the 50k+ request would have gone), and the
/// call after it, from the compaction.
fn assert_compacted_at_the_third_call(recorded: &[serde_json::Value]) {
    assert_eq!(recorded.len(), 4, "two steps, the summary, the reply after: {recorded:#?}");
    assert_eq!(summary_calls(recorded), vec![2], "request 3 is the summary call, before the request over the ceiling");
    let after = recorded[3]["messages"].to_string();
    assert!(recorded[3]["messages"][0].to_string().contains(COMPACTION_PLACEHOLDER_PROMPT), "{after}");
    assert!(recorded[3]["messages"][1].to_string().contains("SUMMARY-1"), "{after}");
}

/// T1: a seat over its ceiling compacts once, before its next call.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_seat_over_its_ceiling_compacts_once_at_its_next_call(pool: PgPool) {
    let _guard = lock_turn_tests();
    let (id, requests) = on_mock(&pool, over_at_the_second_step()).await;
    set_persona_ceiling(&pool, id, AT).await;

    run_turn(&pool, id, hello()).await.expect("the turn ends Ok");

    assert_compacted_at_the_third_call(&requests.lock().expect("the request log").clone());
    assert_eq!(dividers(&pool, id).await, vec![("SUMMARY-1".to_string(), Some(REASON.to_string()))]);
    assert_eq!(compaction_rows(&pool, id).await, 1, "one compaction call recorded");
    let messages = db::list_messages(&pool, id).await.expect("messages");
    assert_eq!(last_reply(&messages).as_deref(), Some("done"));
}

/// T2: last turn ended over the ceiling (its usage stored), two steps in:
/// the next turn's first request is the summary.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_seat_over_its_ceiling_at_a_turns_start_compacts_before_its_first_call(pool: PgPool) {
    let _guard = lock_turn_tests();
    let bodies = vec![todoread_at("toolu_1", 30_000), text_at("one", 50_000), text_reply_body("SUMMARY-2"), text_at("two", 9_000)];
    let (id, requests) = on_mock(&pool, bodies).await;
    set_persona_ceiling(&pool, id, AT).await;
    run_turn(&pool, id, hello()).await.expect("turn 1");
    assert!(dividers(&pool, id).await.is_empty(), "turn 1 ends at 50,000 without another call");

    run_turn(&pool, id, hello()).await.expect("turn 2");

    let recorded = requests.lock().expect("the request log").clone();
    assert_eq!(recorded.len(), 4, "{recorded:#?}");
    assert_eq!(summary_calls(&recorded), vec![2], "turn 2's first request is the summary");
    assert_eq!(dividers(&pool, id).await, vec![("SUMMARY-2".to_string(), Some(REASON.to_string()))]);
    assert_eq!(last_reply(&db::list_messages(&pool, id).await.expect("messages")).as_deref(), Some("two"));
}

/// T3, the no-loop case: the reply after the compaction measures 45,000,
/// within 4,096 of the ceiling, so compacting can't get the seat under it.
/// Three more steps over it, and a second turn, compact no more.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_summary_that_leaves_the_seat_over_its_ceiling_doesnt_compact_again(pool: PgPool) {
    let _guard = lock_turn_tests();
    let bodies = vec![
        todoread_at("toolu_1", 30_000),
        todoread_at("toolu_2", 50_000),
        text_reply_body("SUMMARY-3"),
        todoread_at("toolu_3", 45_000),
        todoread_at("toolu_4", 46_000),
        todoread_at("toolu_5", 47_000),
        todoread_at("toolu_6", 48_000),
        text_at("done", 49_000),
        text_at("turn two", 50_000),
    ];
    let (id, requests) = on_mock(&pool, bodies).await;
    set_persona_ceiling(&pool, id, AT).await;

    run_turn(&pool, id, hello()).await.expect("turn 1");
    run_turn(&pool, id, hello()).await.expect("turn 2");

    let recorded = requests.lock().expect("the request log").clone();
    assert_eq!(summary_calls(&recorded), vec![2], "one summary across both turns: {recorded:#?}");
    assert_eq!(recorded.len(), 9);
    assert_eq!(dividers(&pool, id).await.len(), 1);
    assert_eq!(last_reply(&db::list_messages(&pool, id).await.expect("messages")).as_deref(), Some("turn two"));
}

/// T4: after T3's spent state, the endpoint's compaction leaves room (its
/// next reply measures 10,000), so the ceiling is armed again and fires
/// once the context grows past it, two steps on.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_compaction_that_leaves_room_rearms_the_ceiling(pool: PgPool) {
    let _guard = lock_turn_tests();
    let bodies = vec![
        todoread_at("toolu_1", 30_000),
        todoread_at("toolu_2", 50_000),
        text_reply_body("SUMMARY-A"),
        todoread_at("toolu_3", 45_000),
        text_at("done", 46_000),
        text_reply_body("SUMMARY-B"),
        todoread_at("toolu_4", 10_000),
        todoread_at("toolu_5", 30_000),
        todoread_at("toolu_6", 50_000),
        text_reply_body("SUMMARY-C"),
        text_at("turn two", 5_000),
    ];
    let (id, requests) = on_mock(&pool, bodies).await;
    set_persona_ceiling(&pool, id, AT).await;
    run_turn(&pool, id, hello()).await.expect("turn 1");

    crate::turn::compact_now(&pool, id, None).await.expect("the endpoint compacts");
    run_turn(&pool, id, hello()).await.expect("turn 2");

    let recorded = requests.lock().expect("the request log").clone();
    assert_eq!(summary_calls(&recorded), vec![2, 5, 9], "{recorded:#?}");
    let reasons: Vec<Option<String>> = dividers(&pool, id).await.into_iter().map(|(_, reason)| reason).collect();
    assert_eq!(reasons, vec![Some(REASON.to_string()), Some("on request".to_string()), Some(REASON.to_string())]);
}

/// T5: no persona, or a persona with no `compact_at`, means no ceiling:
/// T1's script runs with no compaction (green before and after: it
/// guards against a default creeping in).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_no_ceiling_means_no_ceiling_compaction(pool: PgPool) {
    let _guard = lock_turn_tests();
    for settings in [None, Some(serde_json::json!({"model": null, "credentials": []}))] {
        let (id, requests) = on_mock(&pool, over_at_the_second_step()).await;
        if let Some(settings) = settings.clone() {
            set_persona_settings(&pool, id, settings).await;
        }
        run_turn(&pool, id, hello()).await.expect("the turn ends Ok");
        let recorded = requests.lock().expect("the request log").clone();
        assert_eq!(recorded.len(), 3, "{settings:?}: the two steps and the reply: {recorded:#?}");
        assert!(summary_calls(&recorded).is_empty(), "{settings:?}");
        assert!(dividers(&pool, id).await.is_empty(), "{settings:?}");
    }
}

/// T6: a child seat's ceiling is its own snapshot's, from its `agents`
/// row; its parent's (500,000) is not inherited.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_child_seats_ceiling_comes_from_its_own_snapshot(pool: PgPool) {
    let _guard = lock_turn_tests();
    let mut bodies = over_at_the_second_step();
    // The child's turn ends plainly with no report, so its after-turn work
    // runs on (SME-455): its first such end nudges the child itself, whose
    // wake turn takes the next body, and that turn's plain end tells the
    // parent and wakes it, which takes the one after (round 1 L2).
    bodies.push(text_reply_body("the nudge's reply"));
    bodies.push(text_reply_body("the parent's reply"));
    let (root, requests) = on_mock(&pool, bodies).await;
    set_persona_ceiling(&pool, root, 500_000).await;
    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('c', $1) RETURNING id")
        .bind(root)
        .fetch_one(&pool)
        .await
        .expect("a child");
    sqlx::query(
        "INSERT INTO agents (conversation_id, root_id, depth, brief, state, credentials, persona_settings)
         VALUES ($1, $2, 1, 'b', 'active', '{}', $3)",
    )
    .bind(child)
    .bind(root)
    .bind(serde_json::json!({"compact_at": AT}))
    .execute(&pool)
    .await
    .expect("its agent row");

    run_turn(&pool, child, hello()).await.expect("the child's turn ends Ok");
    every_turn_is_over(&pool, &[child, root]).await;

    let recorded = requests.lock().expect("the request log").clone();
    assert_compacted_at_the_third_call(&recorded[..4.min(recorded.len())]);
    assert_eq!(dividers(&pool, child).await, vec![("SUMMARY-1".to_string(), Some(REASON.to_string()))]);
    assert!(dividers(&pool, root).await.is_empty(), "the parent's 500,000 ceiling never fired");
}

/// Waits, up to 10 s, until the parent of `ids` (the last) has been told
/// of the child's turn, every message queued between them is delivered,
/// and none of their turns is running: the after-turn wakes are over, so
/// the test leaves no turn on its pool (as `agents::tests::persona` does).
async fn every_turn_is_over(pool: &PgPool, ids: &[i64]) {
    let parent = *ids.last().expect("a parent");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let (told, undelivered): (i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE to_conversation_id = $2), count(*) FILTER (WHERE delivered_at IS NULL)
             FROM agent_messages WHERE to_conversation_id = ANY($1)",
        )
        .bind(ids)
        .bind(parent)
        .fetch_one(pool)
        .await
        .expect("the agent messages");
        if told > 0 && undelivered == 0 && ids.iter().all(|id| !crate::turn::turn_running(*id)) {
            return;
        }
        assert!(std::time::Instant::now() < deadline, "every turn is over: not within 10 s (told {told}, undelivered {undelivered})");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// T7: the ceiling's compaction is `compact`, so every safeguard holds:
/// the user's and the parent's words are quoted verbatim, the todo list is
/// pinned, and a summary cut off at its budget gets its one continuation
/// (SME-443), joined in the divider. A regression guard for D4: it passes
/// only because the path is `compact`.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_ceilings_compaction_keeps_the_safeguards(pool: PgPool) {
    let _guard = lock_turn_tests();
    let todos = serde_json::json!({"todos": [{"content": "KEEP-THE-TODO", "status": "pending"}]});
    let bodies = vec![
        with_usage(&tool_calls_body(&[("toolu_1", "todowrite", todos)]), 30_000, 0, 10),
        todoread_at("toolu_2", 50_000),
        text_reply_body("PART-ONE ").replace("end_turn", "max_tokens"),
        text_reply_body("PART-TWO"),
        text_at("done", 9_000),
    ];
    let (id, requests) = on_mock(&pool, bodies).await;
    set_persona_ceiling(&pool, id, AT).await;
    db::create_message(&pool, id, "user", &[text_block("Message from your parent:\n\nPARENT-SAYS-KEEP-THIS.")])
        .await
        .expect("the parent's message");
    db::create_message(&pool, id, "assistant", &[text_block("Noted.")]).await.expect("its answer");
    let ask = anthropic::AnthropicMessage { role: "user".to_string(), content: vec![text_block("USER-SAYS-KEEP-THIS.")] };

    run_turn(&pool, id, ask).await.expect("the turn ends Ok");

    let recorded = requests.lock().expect("the request log").clone();
    assert_eq!(summary_calls(&recorded), vec![2, 3], "the summary and its one continuation: {recorded:#?}");
    assert!(recorded[3].to_string().contains(crate::turn::compaction::SUMMARY_CONTINUE_PROMPT), "{}", recorded[3]);
    let divided = dividers(&pool, id).await;
    assert_eq!(divided.len(), 1, "{divided:?}");
    assert!(divided[0].0.contains("PART-ONE") && divided[0].0.contains("PART-TWO"), "joined: {divided:?}");
    assert_eq!(divided[0].1.as_deref(), Some(REASON));
    let continuation = recorded[4]["messages"].to_string();
    for kept in ["USER-SAYS-KEEP-THIS.", "PARENT-SAYS-KEEP-THIS.", "KEEP-THE-TODO (pending)"] {
        assert!(continuation.contains(kept), "{kept} in the continuation: {continuation}");
    }
}

/// T8: a ceiling summary that fails (HTTP 500) doesn't fail the turn: no
/// divider, no notice, and no second try this turn though two more steps
/// are over; the next turn tries once again.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failed_ceiling_summary_lets_the_turn_go_on_and_retries_next_turn(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let boom: Answer = Err((axum::http::StatusCode::INTERNAL_SERVER_ERROR, "MOCK-BOOM".to_string()));
    let answers: Vec<Answer> = vec![
        Ok(todoread_at("toolu_1", 30_000)),
        Ok(todoread_at("toolu_2", 50_000)),
        boom.clone(),
        Ok(todoread_at("toolu_3", 51_000)),
        Ok(todoread_at("toolu_4", 52_000)),
        Ok(text_at("turn one", 53_000)),
        boom,
        Ok(text_at("turn two", 54_000)),
    ];
    let mock = start_refusal_mock(&pool, answers).await;
    set_window(&pool).await;
    let id = db::create_conversation(&pool).await.expect("create conversation").id;
    set_persona_ceiling(&pool, id, AT).await;

    run_turn(&pool, id, hello()).await.expect("turn 1 carries on past the failure");
    let turn_one: Vec<serde_json::Value> = (0..mock.count()).map(|i| mock.request(i)).collect();
    assert_eq!(summary_calls(&turn_one), vec![2], "one attempt in turn 1: {turn_one:#?}");
    assert_eq!(turn_one.len(), 6);
    run_turn(&pool, id, hello()).await.expect("turn 2 carries on past the failure");

    let all: Vec<serde_json::Value> = (0..mock.count()).map(|i| mock.request(i)).collect();
    assert_eq!(summary_calls(&all), vec![2, 6], "one more attempt, at turn 2's start: {all:#?}");
    assert!(dividers(&pool, id).await.is_empty(), "nothing compacted");
    let messages = db::list_messages(&pool, id).await.expect("messages");
    assert_eq!(last_reply(&messages).as_deref(), Some("turn two"));
    let saved: String = messages.iter().flat_map(|m| m.blocks().expect("blocks")).map(|b| format!("{b:?}")).collect();
    assert!(!saved.contains("smelt notice"), "no notice saved: {saved}");
}
