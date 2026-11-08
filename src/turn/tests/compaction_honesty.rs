//! SME-393: the honesty of the compaction hand-off.
//!
//! A compacted seat believed in commits it never made — imaginary SHAs, an
//! imaginary HEAD — and destroyed its real uncommitted work "restoring"
//! them (conversation 2294639560; SME-439's 2294639622 completed a summary
//! cut off mid-sentence the same way, 16 s after the cut). Neither saved
//! summary lied; the seat confabulated on top of it, in the one exchange
//! where a past-tense narration meets a continuation that said the summary
//! "says which requests are already done".
//!
//! These tests pin the two prompts of that exchange: the summarization
//! request must bind itself to tool-result evidence (a Done/Planned split,
//! (a) in the ticket's plan), and the continuation must call the summary a
//! narration to verify, not a record to trust (b). A fake model cannot
//! re-fabricate, so what is mechanical here is the contract texts, not
//! model compliance — that limit is the plan's, stated not papered.

use super::*;

/// The summarization request's `system` must bind the summariser to
/// evidence: a **Done** part holding only what a tool result completes,
/// everything started or intended under **Planned**, words never taken as
/// evidence, and the state before an `[earlier conversation omitted]` cut
/// unknown rather than done (SME-393's (a)). The prompt's exact wording is
/// free; this contract is what the ticket decided.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_compaction_prompt_separates_done_from_planned(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool)
        .await
        .expect("create conversation");
    db::create_message(
        &pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text {
            text: "earlier message".to_string(),
        }],
    )
    .await
    .expect("seed earlier message");
    // Usage past the ceiling, so the next turn compacts first.
    db::upsert_conversation_usage(
        &pool,
        conversation.id,
        &anthropic::TokenUsage {
            input_tokens: 190_000,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
    )
    .await
    .expect("seed usage");
    let requests = start_recording_mock_upstream(&pool, vec![
        text_reply_body("Summary: nothing live."),
        text_reply_body("Hi again"),
    ])
    .await;

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("run_turn should succeed");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 2, "a compaction call, then the real turn");
    let system = requests[0]["system"]
        .as_str()
        .expect("the summarization request carries its system prompt");
    assert!(system.contains("Done"), "the prompt names a Done part: {system}");
    assert!(system.contains("Planned"), "…and a Planned part beside it: {system}");
    assert!(
        system.contains("words are not evidence"),
        "…and says what counts as evidence: {system}"
    );
    assert!(
        system.contains("[earlier conversation omitted]"),
        "…and names the cut whose state is unknown, not done: {system}"
    );
    assert!(
        !system.contains("Write the summary as plain prose."),
        "the one-prose-part instruction is gone, so the two labelled parts are permitted: {system}"
    );
}
