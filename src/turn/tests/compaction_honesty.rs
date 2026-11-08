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

/// (b): the continuation calls its summary a narration to verify, not a
/// record to trust — and the SME-367 typed-quotes clause keeps don't-redo
/// without the invitation to trust. One constant pair feeds the HTTP and
/// the Claude CLI kinds alike.
#[test]
fn test_continuation_says_verify_before_acting() {
    let bare = continuation_prompt(&Typed::default(), &[]);
    assert!(bare.starts_with("Continue based on the summary above."), "the nudge stays: {bare}");
    assert!(bare.contains("narration"), "the summary is named a narration: {bare}");
    assert!(bare.contains("verify"), "…and verified, not believed: {bare}");
    assert!(
        bare.contains("overwrite, discard or reset"),
        "…always before anything that would overwrite, discard or reset work: {bare}"
    );
    let typed = Typed { quotes: vec!["use the compose stack".to_string()], omitted: 1 };
    let with_quotes = continuation_prompt(&typed, &[]);
    assert!(
        !with_quotes.contains("the summary says which requests are already done"),
        "the trust-the-summary clause is gone (SME-393): {with_quotes}"
    );
    assert!(
        with_quotes.contains("without checking them first"),
        "SME-367's don't-redo stays, behind a check: {with_quotes}"
    );
    assert!(
        with_quotes.contains("use the compose stack"),
        "the quotes are still word for word (SME-367): {with_quotes}"
    );
    assert!(
        with_quotes.contains("(1 more they wrote in between is left out here"),
        "the omitted count survives: {with_quotes}"
    );
}

/// The 2294639560 shape, honestly scoped: a typed parent ask, tool results
/// for the edits and the test run, and — as in the incident, where `git
/// commit` never ran at all — no completing commit result anywhere. The
/// summariser's reply is scripted as a 9560-style claim, because a fake
/// model cannot re-fabricate. What this proves mechanically: the transcript
/// handed the summariser evidences no commit and holds no SHA (fixture
/// sanity), the claim still saves as it always did — no code-side claim
/// check was adopted (Decided (Driver) Q1: it would have fired on neither
/// incident) — and the continuation beside it is the verify-before-acting
/// one from (b), with the parent's words quoted untouched. The claim's
/// effect on a real model is proven on the instance, not here.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_fixture_like_2294639560_shows_claims_without_results(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    // The typed ask (first, always quoted), the work with its evidence —
    // an edit and a test run — and the typed parent message (quoted by
    // SME-367's clause), then a final assistant reply: the safe boundary.
    let seed: Vec<(&str, Vec<anthropic::ContentBlock>)> = vec![
        ("user", vec![text_block("Make three commits: code, notes, wording, each on its own.")]),
        ("assistant", vec![tool_use("toolu_e", "edit_file")]),
        ("user", vec![tool_result("toolu_e", "wrote src/turn/compaction.rs")]),
        ("assistant", vec![tool_use("toolu_t", "run_terminal_command")]),
        ("user", vec![tool_result("toolu_t", "test result: ok. 42 passed")]),
        ("user", vec![text_block("Message from your parent:\n\ncommit each part as you finish it.")]),
        ("assistant", vec![text_block("Noted — each part gets its own commit.")]),
    ];
    for (role, blocks) in seed {
        db::create_message(&pool, conversation.id, role, &blocks)
            .await
            .expect("seed message");
    }
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
    // The scripted 9560-style claim (the incident's words, not the
    // incident's model): commits with SHAs no tool result completed.
    let requests = start_recording_mock_upstream(&pool, vec![
        text_reply_body("Commit 1 done (6153b753): honesty prompt. Commit 2 done (9147a4a2). Commit 3 pending."),
        text_reply_body("Carrying on."),
    ])
    .await;

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("the compaction runs and the turn carries on");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 2, "a compaction call, then the real turn");
    let transcript = requests[0]["messages"][0]["content"][0]["text"]
        .as_str()
        .expect("the summarization request carries its prompt")
        .to_string();
    assert!(
        transcript.contains("[tool result: test result: ok") && transcript.contains("[tool result: wrote src/"),
        "fixture sanity: the real evidence renders: {transcript}"
    );
    assert!(
        !transcript.contains("6153b753") && !transcript.contains("git commit"),
        "fixture sanity: no tool result completes a commit, and no SHA exists to evidence the claim: {transcript}"
    );
    drop(requests);

    let messages = db::list_messages(&pool, conversation.id).await.expect("messages");
    let saved: Vec<String> = messages
        .iter()
        .flat_map(|m| m.blocks().expect("blocks"))
        .filter_map(|b| match b {
            anthropic::ContentBlock::CompactionSummary { summary, .. } => Some(summary),
            _ => None,
        })
        .collect();
    assert_eq!(saved.len(), 1);
    assert!(
        saved[0].contains("Commit 1 done (6153b753)"),
        "the unevidenced claim saves — there is no code-side check (Decided (Driver) Q1): {}",
        saved[0]
    );
    let continuation = messages
        .iter()
        .flat_map(|m| m.blocks().expect("blocks"))
        .filter_map(|b| match b {
            anthropic::ContentBlock::CompactionPlaceholder { text } if text.starts_with("Continue based") => Some(text),
            _ => None,
        })
        .next()
        .expect("the continuation the seat actually reads");
    assert!(
        continuation.contains("commit each part as you finish it."),
        "the parent's words stay quoted word for word (SME-367): {continuation}"
    );
    assert!(
        !continuation.contains("the summary says which requests are already done"),
        "and the continuation no longer says to believe it: {continuation}"
    );
    assert!(
        continuation.contains("verify") && continuation.contains("without checking them first"),
        "the post-compaction seat is told to verify state before acting: {continuation}"
    );
}

fn tool_use(id: &str, name: &str) -> anthropic::ContentBlock {
    anthropic::ContentBlock::ToolUse {
        id: id.to_string(),
        name: name.to_string(),
        input: serde_json::json!({}),
    }
}

fn tool_result(id: &str, content: &str) -> anthropic::ContentBlock {
    anthropic::ContentBlock::ToolResult {
        tool_use_id: id.to_string(),
        content: content.to_string(),
        is_error: None,
    }
}
