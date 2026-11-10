//! SME-414 scenario 2: a tool result the model never read — a compaction
//! covered it before its next request — named in the continuation, so the
//! model doesn't re-run the big call and keep the compaction loop going.

use super::compaction_progress::step;
use super::window_stop::set_context_window;
use super::*;

/// The section's pinned phrase (the wording may be polished; this may not).
const UNREAD_MARKER: &str = "summarized above before you read them";

/// Window 40,000; one step 12,380 in / 12,000 out: every projection clears
/// the 23,616 trigger, so the turn compacts after the tool result lands —
/// before the model reads it. The post-summary request's continuation must
/// name what it covered unread (red on main: it doesn't name it).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_continuation_names_results_compacted_before_they_were_read(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let requests = start_recording_mock_upstream(
        &pool,
        vec![step(12_380, 12_000), text_reply_body("The summary."), text_reply_body("Done.")],
    )
    .await;
    set_context_window(&pool, 40_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    run_turn(&pool, conversation.id, hello()).await.expect("the turn goes on");

    // Requests: the ask, the summary call, the post-summary request. The
    // covered history is gone from the third; only a naming section can
    // mention the result's tool by name there.
    let recorded = requests.lock().expect("the request log");
    let post_summary = recorded[2].to_string();
    assert!(post_summary.contains(UNREAD_MARKER), "the continuation says what was covered unread: {post_summary}");
    assert!(post_summary.contains("before you read them: todoread"), "naming inside the section: {post_summary}");
}

fn tool_use(id: &str, name: &str) -> anthropic::ContentBlock {
    anthropic::ContentBlock::ToolUse { id: id.to_string(), name: name.to_string(), input: serde_json::json!({}) }
}

fn tool_result(id: &str, content: &str) -> anthropic::ContentBlock {
    anthropic::ContentBlock::ToolResult { tool_use_id: id.to_string(), content: content.to_string(), is_error: None }
}

/// The unread tail: results after the model's last reply, named by the
/// calls they answer, in block order; an id no open call owns names nothing.
#[test]
fn test_unread_results_are_named_by_their_own_call() {
    let messages = vec![
        message_with_blocks(1, "user", vec![text_block("go")]),
        message_with_blocks(2, "assistant", vec![tool_use("toolu_01", "todoread"), tool_use("toolu_02", "read_file")]),
        message_with_blocks(3, "user", vec![tool_result("toolu_02", "the file"), tool_result("toolu_01", "[]"), tool_result("toolu_99", "orphan")]),
    ];
    assert_eq!(
        unread_tool_results(&messages),
        vec!["read_file".to_string(), "todoread".to_string()],
        "block order, matched by id, orphans unnamed"
    );
}

/// Once the model has replied again, its results are read: nothing unread.
#[test]
fn test_results_the_model_replied_to_are_not_unread() {
    let messages = vec![
        message_with_blocks(1, "assistant", vec![tool_use("toolu_01", "todoread")]),
        message_with_blocks(2, "user", vec![tool_result("toolu_01", "[]")]),
        message_with_blocks(3, "assistant", vec![text_block("Read them.")]),
    ];
    assert_eq!(unread_tool_results(&messages), Vec::<String>::new());
}

/// The section: capped at 8 names with the rest counted, before the
/// unanswered tail (which stays last), and absent when nothing is unread.
#[test]
fn test_the_unread_section_is_capped_and_sits_before_the_tail() {
    let names: Vec<String> = (1..=9).map(|i| format!("tool{i}")).collect();
    let prompt = continuation_prompt(&Typed::default(), &["answer me".to_string()], &[], &names);
    assert!(
        prompt.contains("summarized above before you read them: tool1, tool2, tool3, tool4, tool5, tool6, tool7, tool8 (and 1 more)"),
        "{prompt}"
    );
    assert!(!prompt.contains("tool9"), "capped at eight named: {prompt}");
    let section = prompt.find(UNREAD_MARKER).expect("the section");
    let tail = prompt.find("haven't answered yet").expect("the tail");
    assert!(section < tail, "the tail stays last: {prompt}");
    let bare = continuation_prompt(&Typed::default(), &[], &[], &[]);
    assert!(!bare.contains(UNREAD_MARKER), "nothing unread, no section: {bare}");
}
