//! Compacting on demand (SME-482): the model's `compact_context` tool, at
//! the end of its tool pass, at most once a turn.

use super::ask_user::tool_calls_body;
use super::*;

const COMPACT: &str = "compact_context";

/// Every `CompactionSummary` saved in the conversation, as (summary, reason).
pub(super) async fn dividers(pool: &PgPool, conversation_id: i64) -> Vec<(String, Option<String>)> {
    db::list_messages(pool, conversation_id)
        .await
        .expect("messages")
        .iter()
        .flat_map(|m| m.blocks().expect("blocks"))
        .filter_map(|b| match b {
            anthropic::ContentBlock::CompactionSummary { summary, refused, .. } => Some((summary, refused)),
            _ => None,
        })
        .collect()
}

/// The saved result of call `id`: (content, is_error).
async fn result_of(pool: &PgPool, conversation_id: i64, id: &str) -> Option<(String, Option<bool>)> {
    db::list_messages(pool, conversation_id)
        .await
        .expect("messages")
        .iter()
        .flat_map(|m| m.blocks().expect("blocks"))
        .find_map(|b| match b {
            anthropic::ContentBlock::ToolResult { tool_use_id, content, is_error } if tool_use_id == id => {
                Some((content, is_error))
            }
            _ => None,
        })
}

/// T1: the model asks twice in one turn; the first compacts at the end of
/// its pass, with the note in the summary prompt, and the second is
/// refused, so the turn pays for one summary.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_model_compacts_at_most_once_per_turn(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let requests = start_recording_mock_upstream(
        &pool,
        vec![
            tool_calls_body(&[("toolu_c1", COMPACT, serde_json::json!({"note": "keep SHA abc123"}))]),
            text_reply_body("SUMMARY-1"),
            tool_calls_body(&[("toolu_c2", COMPACT, serde_json::json!({}))]),
            text_reply_body("done"),
        ],
    )
    .await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("the turn ends Ok");

    let recorded = requests.lock().expect("the request log").clone();
    assert_eq!(recorded.len(), 4, "the reply, one summary, the reply after it, the last: {recorded:#?}");
    let divided = dividers(&pool, conversation.id).await;
    assert_eq!(divided.len(), 1, "one compaction: {divided:?}");
    assert_eq!(divided[0], ("SUMMARY-1".to_string(), Some("the model asked for it".to_string())));
    assert!(recorded[1]["tools"].as_array().is_none_or(Vec::is_empty), "request 2 is the summary call");
    assert!(recorded[1].to_string().contains("keep SHA abc123"), "the note reaches the summariser: {}", recorded[1]);
    let after = recorded[2]["messages"].to_string();
    assert!(
        recorded[2]["messages"][0].to_string().contains(COMPACTION_PLACEHOLDER_PROMPT),
        "the request after starts from the compaction: {after}"
    );
    assert!(recorded[2]["messages"][1].to_string().contains("SUMMARY-1"), "{after}");
    assert!(!after.contains("toolu_c1"), "the compact call is covered by the summary: {after}");
    let (refusal, is_error) = result_of(&pool, conversation.id, "toolu_c2").await.expect("the second call's result");
    assert_eq!(is_error, Some(true), "{refusal}");
    assert!(refusal.contains("once this turn"), "{refusal}");
    let messages = db::list_messages(&pool, conversation.id).await.expect("messages");
    assert_eq!(last_reply(&messages).as_deref(), Some("done"));
}

/// The text of request `request`'s message `index`, as sent.
fn sent_text(request: &serde_json::Value, index: usize) -> String {
    request["messages"][index].to_string()
}

/// T2: the compaction waits for the whole pass. The summary covers the
/// todowrite result beside it, the continuation pins the list and quotes
/// the user, names todowrite (not compact_context) as covered unread, and
/// a message queued during the pass lands after the continuation.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_model_compacts_after_its_whole_tool_pass(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let todos = serde_json::json!({"todos": [{"content": "a", "status": "pending"}]});
    let requests = start_recording_mock_upstream(
        &pool,
        vec![
            tool_calls_body(&[("toolu_t", "todowrite", todos), ("toolu_c", COMPACT, serde_json::json!({}))]),
            text_reply_body("SUMMARY-2"),
            text_reply_body("after"),
        ],
    )
    .await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let id = conversation.id;
    let (reached, release) = test_hooks::pause_after_reply_saved(id);
    let turn_pool = pool.clone();
    let turn = tokio::spawn(async move { run_turn(&turn_pool, id, hello()).await.map(|_| ()) });
    tokio::time::timeout(std::time::Duration::from_secs(10), reached)
        .await
        .expect("the reply is saved within 10 s")
        .expect("the hook fires");
    let mut conn = pool.acquire().await.expect("a connection");
    db::queue_agent_message(&mut conn, id, None, "your parent", db::KIND_MESSAGE, "NOTICE-DURING-PASS")
        .await
        .expect("queue a message");
    drop(conn);
    release.send(()).expect("release the turn");
    tokio::time::timeout(std::time::Duration::from_secs(10), turn)
        .await
        .expect("the turn ends within 10 s")
        .expect("the turn task")
        .expect("the turn ends Ok");

    let recorded = requests.lock().expect("the request log").clone();
    assert_eq!(recorded.len(), 3, "the reply, the summary, the reply after: {recorded:#?}");
    let summary_prompt = recorded[1].to_string();
    assert!(summary_prompt.contains(r#"[tool result: [{\"content\":\"a\""#), "the summary covers todowrite's result: {summary_prompt}");
    assert!(!summary_prompt.contains("NOTICE-DURING-PASS"), "the message isn't summarised: {summary_prompt}");
    let continuation = sent_text(&recorded[2], 2);
    assert!(continuation.contains("- a (pending)"), "the todo list is pinned: {continuation}");
    assert!(continuation.contains("hello"), "the user's words are quoted: {continuation}");
    assert!(continuation.contains("before you read them: todowrite"), "todowrite is named unread: {continuation}");
    assert!(!continuation.contains(COMPACT), "compact_context isn't named unread: {continuation}");
    let after = recorded[2]["messages"].to_string();
    let (resumed, notice) = (
        after.find("Continue based on the summary above").expect("the continuation"),
        after.find("NOTICE-DURING-PASS").expect("the message is sent"),
    );
    assert!(resumed < notice, "the message lands after the continuation: {after}");
}

/// T3: a compact call beside a question is refused, naming it: covering
/// the question's call would orphan its answer.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_compact_call_beside_a_question_is_refused(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let ask = serde_json::json!({"questions": [{"question": "Go on?", "header": "Go", "options": [{"label": "Yes"}, {"label": "No"}]}]});
    let requests = start_recording_mock_upstream(
        &pool,
        vec![tool_calls_body(&[("toolu_q", crate::questions::ASK_USER, ask), ("toolu_c", COMPACT, serde_json::json!({}))])],
    )
    .await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("the turn ends on the question");

    assert!(db::get_pending_question(&pool, conversation.id).await.expect("the question row").is_some(), "the question is recorded");
    let (refusal, is_error) = result_of(&pool, conversation.id, "toolu_c").await.expect("the compact call's result");
    assert_eq!(is_error, Some(true), "{refusal}");
    assert!(refusal.contains("can't run in the same reply as a question"), "{refusal}");
    assert!(dividers(&pool, conversation.id).await.is_empty(), "no compaction");
    assert_eq!(requests.lock().expect("the request log").len(), 1, "no summary request");
}

/// T4: a pass that ends the turn (notify_user) still compacts before the
/// turn returns.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_pass_that_ends_the_turn_still_compacts(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let requests = start_recording_mock_upstream(
        &pool,
        vec![
            tool_calls_body(&[
                ("toolu_c", COMPACT, serde_json::json!({})),
                ("toolu_n", anthropic::tools::NOTIFY_USER, serde_json::json!({"message": "all done"})),
            ]),
            text_reply_body("SUMMARY-4"),
        ],
    )
    .await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("the turn ends Ok");

    assert_eq!(requests.lock().expect("the request log").len(), 2, "the reply and the summary");
    assert_eq!(dividers(&pool, conversation.id).await, vec![("SUMMARY-4".to_string(), Some("the model asked for it".to_string()))]);
    let (notified, is_error) = result_of(&pool, conversation.id, "toolu_n").await.expect("notify_user's result");
    assert_eq!(is_error, None, "the notification is kept: {notified}");
}

/// T5: SME-443 holds on the model path: an empty summary, or one cut off
/// twice, fails the turn and leaves the history as it was.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_empty_summary_leaves_the_history_and_the_turn_carries_on(pool: PgPool) {
    a_refused_summary_is_a_notice(&pool, vec![text_reply_body("")], "its summary came back empty").await;
}

/// T5, cut off: the summary and its one continuation both stop at the budget.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_summary_cut_off_twice_leaves_the_history_and_the_turn_carries_on(pool: PgPool) {
    let cut = text_reply_body("part").replace("end_turn", "max_tokens");
    a_refused_summary_is_a_notice(&pool, vec![cut.clone(), cut], "cut off again").await;
}

/// T5's body (SME-482 round 1 L1): the model asks, the summary is refused
/// for `why`. A compaction the model chose is optional: the turn doesn't
/// fail. A notice names the failure, the history is never replaced (no
/// divider), and the model is called again and answers.
async fn a_refused_summary_is_a_notice(pool: &PgPool, summaries: Vec<String>, why: &str) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(pool).await.expect("clear ids");
    let mut bodies = vec![tool_calls_body(&[("toolu_c", COMPACT, serde_json::json!({}))])];
    bodies.extend(summaries);
    bodies.push(text_reply_body("carried on"));
    let requests = start_recording_mock_upstream(pool, bodies).await;
    let conversation = db::create_conversation(pool).await.expect("create conversation");

    run_turn(pool, conversation.id, hello()).await.expect("the turn carries on");

    assert!(dividers(pool, conversation.id).await.is_empty(), "no divider: the history stays");
    let messages = db::list_messages(pool, conversation.id).await.expect("messages");
    assert_eq!(last_reply(&messages).as_deref(), Some("carried on"));
    let last = requests.lock().expect("the request log").last().cloned().expect("a request").to_string();
    assert!(last.contains("compact_context") && last.contains(why), "the model is told why: {last}");
    assert!(last.contains("toolu_c"), "the history it carries on from is the uncompacted one: {last}");
}
