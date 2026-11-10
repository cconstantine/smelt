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
    let bare = continuation_prompt(&Typed::default(), &[], &[]);
    assert!(bare.starts_with("Continue based on the summary above."), "the nudge stays: {bare}");
    assert!(bare.contains("narration"), "the summary is named a narration: {bare}");
    assert!(bare.contains("verify"), "…and verified, not believed: {bare}");
    assert!(
        bare.contains("overwrite, discard or reset"),
        "…always before anything that would overwrite, discard or reset work: {bare}"
    );
    let typed = Typed { quotes: vec!["use the compose stack".to_string()], omitted: 1 };
    let with_quotes = continuation_prompt(&typed, &[], &[]);
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

/// SME-462, the ticket's Done-when: after a compaction the next model call
/// carries the conversation's todo list verbatim — without the model
/// calling `todoread`, and without the summariser having copied it. The
/// scripted summariser answers "nothing live", exactly the failure mode the
/// ticket names, so what carries the list must be the pinned block in the
/// continuation message: both items verbatim beside the nudge, under the
/// lead-in that marks them as the model's own claims.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_continuation_pins_the_todo_list_when_the_summary_omits_it(pool: PgPool) {
    use crate::anthropic::tools::{TodoItem, TodoStatus};
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
    db::set_conversation_todos(
        &pool,
        conversation.id,
        &[
            TodoItem { content: "Ship the pinned list".to_string(), status: TodoStatus::InProgress },
            TodoItem { content: "Bury the stale copy".to_string(), status: TodoStatus::Pending },
        ],
    )
    .await
    .expect("seed todos");
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
    // The summariser that omits the todos — exactly the failure mode.
    let requests = start_recording_mock_upstream(&pool, vec![
        text_reply_body("Summary: nothing live."),
        text_reply_body("Carrying on."),
    ])
    .await;

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("run_turn should succeed");

    let requests = requests.lock().expect("the request log");
    assert_eq!(requests.len(), 2, "the compaction call, then the real turn — never a `todoread` call");
    let user_text: String = requests[1]["messages"]
        .as_array()
        .expect("the turn request carries its messages")
        .iter()
        .filter(|message| message["role"].as_str() == Some("user"))
        .flat_map(|message| message["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|block| block["text"].as_str().map(|text| text.to_string()))
        .collect::<Vec<String>>()
        .join("\n");
    assert!(
        user_text.contains("Continue based on the summary above."),
        "the continuation nudge is present beside the list: {user_text}"
    );
    assert!(
        user_text.contains("- Ship the pinned list (in_progress)"),
        "the in-progress item is pinned verbatim: {user_text}"
    );
    assert!(
        user_text.contains("- Bury the stale copy (pending)"),
        "the pending item is pinned verbatim: {user_text}"
    );
    assert!(
        user_text.contains("not verified facts"),
        "the pinned block is marked as the model's own claims, not ground truth: {user_text}"
    );
}

/// A todo item for the pinned-list tests below.
fn todo_item(
    content: &str,
    status: crate::anthropic::tools::TodoStatus,
) -> crate::anthropic::tools::TodoItem {
    crate::anthropic::tools::TodoItem { content: content.to_string(), status }
}

/// The empty list renders no block at all: a conversation that never
/// called `todowrite` gains no section in its continuation (SME-462).
#[test]
fn test_pinned_todos_says_nothing_when_there_is_no_list() {
    assert_eq!(pinned_todos_text(&[]), None);
}

/// The block is the model's own tracker, not ground truth (SME-462): the
/// items verbatim in list order with the status's wire spelling, under the
/// lead-in's marker phrase. Tests pin the marker phrase and the `(N of M`
/// opening of the cut note, never whole sentences.
#[test]
fn test_pinned_todos_marks_the_list_as_the_models_own_claims() {
    use crate::anthropic::tools::TodoStatus;
    let block = pinned_todos_text(&[
        todo_item("Ship the pinned list", TodoStatus::InProgress),
        todo_item("Bury the stale copy", TodoStatus::Pending),
    ])
    .expect("a list gets a block");
    assert!(block.contains("- Ship the pinned list (in_progress)"), "the item lines are verbatim: {block}");
    assert!(block.contains("- Bury the stale copy (pending)"), "in list order, with the wire status: {block}");
    assert!(block.contains("not verified facts"), "the lead-in marks them the model's own claims: {block}");
    assert!(!block.contains("todos are shown here"), "no cut note when nothing was cut: {block}");
}

/// The 2,000-char cap keeps the list head whole and says so: whole items
/// in order while they fit, the first that doesn't fit kept cut, and a
/// trailing `(N of M` note whose `todoread` pointer names where the whole
/// list lives (SME-462). The fixture is arithmetic: 50 items of 572-char
/// lines, so exactly 3 fit whole (1,718 chars) and item 4 is the cut one.
#[test]
fn test_pinned_todos_caps_a_long_list_with_a_note_naming_todoread() {
    use crate::anthropic::tools::TodoStatus;
    let todos: Vec<crate::anthropic::tools::TodoItem> =
        (0..50).map(|i| todo_item(&format!("todo {i:03}: {}", "x".repeat(550)), TodoStatus::Pending)).collect();
    let block = pinned_todos_text(&todos).expect("a long list still gets a block");
    assert!(block.contains("(4 of 50 todos are shown here"), "the cut note names what's shown: {block}");
    assert!(block.contains("todoread lists the whole list"), "and points at the full list: {block}");
    assert!(block.contains("cut to its first"), "the first item that didn't fit is kept cut: {block}");
    assert!(block.contains("todo 000"), "the head is kept: {block}");
    assert!(!block.contains("todo 004"), "items past the cut are left out: {block}");
    assert!(block.chars().count() < 3_000, "the block stays near its 2,000-char cap, not the list's ~29,000");
}

/// The cap's boundary (SME-462's review): a whole item that fills the
/// 2,000-char item budget exactly, followed by any other todo — the
/// next item's subtraction then has nothing left, and it must stay
/// whole-item arithmetic: no underflow (debug panicked inside
/// `compact()`), no wrapped budget silently appending the next item
/// whole and dropping the cut note (release). Fixture arithmetic:
/// 1,988 chars of content in `- {content} (pending)` framing is exactly
/// 2,000 chars; one more todo follows.
#[test]
fn test_pinned_todos_exactly_at_the_cap_still_cuts_the_next_item() {
    use crate::anthropic::tools::TodoStatus;
    let big = "x".repeat(1_988);
    let block = pinned_todos_text(&[
        todo_item(&big, TodoStatus::Pending),
        todo_item("Bury the stale copy", TodoStatus::Pending),
    ])
    .expect("a list gets a block, and exactly-at-cap gets one too");
    assert!(block.contains(&format!("- {big} (pending)")), "the exactly-filling item is kept whole");
    let note_start = block.rfind("\n(").expect("the cut needs the trailing note");
    let note = &block[note_start + 1..];
    assert!(note.starts_with("(1 of 2 todos"), "the next item is cut, and the note counts it: {note}");
    assert!(note.contains("todoread lists the whole list"), "…naming where the whole list lives: {note}");
    assert!(!block.contains("- Bury the stale copy (pending)"), "the item past the cap is not appended whole");
    let items_part = &block[..note_start];
    let gap = items_part.rfind("\n\n").expect("the lead-in gap");
    assert_eq!(
        items_part[gap..].chars().count() - 2, // the gap itself, not counted
        CONTINUATION_TODOS_MAX_CHARS,
        "the item lines stop at exactly the cap"
    );
    assert!(
        block.chars().count() <= CONTINUATION_TODOS_MAX_CHARS + 425,
        "total stays cap + the documented ~425-char lead-in/marker/note overhead: {}",
        block.chars().count()
    );
}

/// 2294639622's trigger, after SME-443: an HTTP-kind summary cut off at
/// its budget is continued once — and when that continuation ends its
/// turn with nothing more to say, what the first call wrote still saves,
/// marked (PR #291's decision kept for the truncation itself; SME-393's
/// marker makes the seat see it). The marker stays reason-neutral.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_cut_off_http_summary_still_saves_marked_when_the_continuation_is_done(pool: PgPool) {
    use super::overflow::compaction_summaries;
    use super::text_calls::text_blocks_body;
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    db::create_message(
        &pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text { text: "earlier".to_string() }],
    )
    .await
    .expect("seed");
    db::create_message(
        &pool,
        conversation.id,
        "assistant",
        &[anthropic::ContentBlock::Text { text: "reply".to_string() }],
    )
    .await
    .expect("seed");
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
    // Cut off at the cap mid-claim, like 2294639622's last summary ("Fix
    // stack (5 commits, all with exact `Co-Authored-By: …`"); the
    // continuation ends its turn empty — the model says it is done.
    let cut = text_reply_body("Fix stack (5 commits, all with exact trailer")
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"max_tokens""#);
    assert!(cut.contains("max_tokens"), "shape guard: the summary is cut off at the cap: {cut}");
    let requests = start_recording_mock_upstream(&pool, vec![
        cut,
        text_blocks_body(&[], "end_turn"),
        text_reply_body("Carrying on."),
    ])
    .await;

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("the marked save stands and the turn carries on");

    // The continuation was asked — pinned on its shape, not a count an
    // old-code nudge can fake: this request must carry part 1.
    let requests = requests.lock().expect("the request log");
    let msgs = requests[1]["messages"].as_array().expect("the continuation's messages");
    let roles: Vec<&str> = msgs.iter().map(|m| m["role"].as_str().expect("a role")).collect();
    assert_eq!(roles, ["user", "assistant", "user"], "the request ends on user — no prefill (SME-443)");
    assert_eq!(
        msgs[1]["content"][0]["text"].as_str(),
        Some("Fix stack (5 commits, all with exact trailer"),
        "the continuation carries part 1 (SME-443)"
    );
    assert!(
        msgs[2]["content"][0]["text"].as_str().unwrap_or_default().contains("never repeat it"),
        "the condition prompt is the third message: {:?}",
        msgs[2]
    );
    assert_eq!(requests.len(), 3, "summary, continuation, turn");
    drop(requests);
    let saved = compaction_summaries(&pool, conversation.id).await;
    assert_eq!(saved.len(), 1, "one divider: the cut-off part still saves (PR #291's decision)");
    assert!(
        saved[0].starts_with("Fix stack (5 commits, all with exact trailer"),
        "the text that made it saves as it is: {saved:?}"
    );
    assert!(
        saved[0].contains("cut off") && saved[0].contains("mid-sentence"),
        "the continuation declared itself done, so the truncation stays marked (SME-393): {saved:?}"
    );
    assert!(
        !saved[0].to_lowercase().contains("reply cap"),
        "the marker names the truncation, not a cause this save doesn't have (SME-393 L1): {saved:?}"
    );
}

/// The marker fires for either cut-off stop — `max_tokens` at the cap or
/// the window (`model_context_window_exceeded`, `stream::is_cut_off`) —
/// so its wording must stay reason-neutral: blaming the cap for a window
/// stop points the seat at the provider's reply-token dial when the
/// window was the cause (SME-393 round 1, L1). The 2294639622 shape
/// again, stopped by the window: the save must name the truncation (the
/// pinned "cut off" and "mid-sentence") and no cause.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_window_stopped_cut_off_marker_blames_no_cause(pool: PgPool) {
    use super::overflow::compaction_summaries;
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    db::create_message(
        &pool,
        conversation.id,
        "user",
        &[anthropic::ContentBlock::Text { text: "earlier".to_string() }],
    )
    .await
    .expect("seed");
    db::create_message(
        &pool,
        conversation.id,
        "assistant",
        &[anthropic::ContentBlock::Text { text: "reply".to_string() }],
    )
    .await
    .expect("seed");
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
    // The same mid-claim truncation as the test above, this time stopped
    // by the context window rather than the reply cap.
    let cut = text_reply_body("Fix stack (5 commits, all with exact trailer")
        .replace(r#""stop_reason":"end_turn""#, r#""stop_reason":"model_context_window_exceeded""#);
    assert!(
        cut.contains("model_context_window_exceeded"),
        "shape guard: the summary is the window's stop, not the cap's: {cut}"
    );
    start_recording_mock_upstream(&pool, vec![cut, text_reply_body("Carrying on.")]).await;

    run_turn(&pool, conversation.id, hello())
        .await
        .expect("the window-stopped summary saves and the turn carries on");

    let saved = compaction_summaries(&pool, conversation.id).await;
    assert_eq!(saved.len(), 1, "a window-stopped cut-off still saves (PR #291's decision)");
    assert!(
        saved[0].contains("cut off") && saved[0].contains("mid-sentence"),
        "marked where it stopped, whatever stopped it (SME-393): {saved:?}"
    );
    assert!(
        !saved[0].to_lowercase().contains("reply cap"),
        "the marker names the truncation, not a cause this save doesn't have (SME-393 L1): {saved:?}"
    );
}
