//! A Claude Code turn that returns while its reply's calls wait (SME-444):
//! each call is answered — a question with a placeholder, a success — and
//! the CLI's echo awaited before the run is interrupted, so the model
//! reads no refusal; the answer's turn says which call it answers.

use super::*;
use crate::claude_cli::run::{ASKED_PARENT, ASKED_USER};

/// The question every test here asks.
fn delete_question() -> Value {
    json!({"questions": [{"question": "Delete the build directory?", "header": "Delete", "options": [{"label": "Yes"}, {"label": "No"}]}]})
}

/// Each of process `n`'s answered calls: (id, text, is_error).
fn answered(fake: &FakeCli, n: usize) -> Vec<(String, String, bool)> {
    fake.results(n)
        .iter()
        .map(|r| (r["id"].as_str().unwrap_or_default().to_string(), r["text"].as_str().unwrap_or_default().to_string(), r["is_error"].as_bool().unwrap_or(true)))
        .collect()
}

/// The transcript entries of processes `0..=last`, in order.
fn transcripts(fake: &FakeCli, last: usize) -> Vec<Value> {
    (0..=last).flat_map(|n| fake.transcript(n)).collect()
}

/// Asserts the transcript holds no refusal: no error result, and none of
/// the CLI's words for one.
fn assert_no_refusal(entries: &[Value]) {
    for entry in entries {
        assert_ne!(entry["is_error"], json!(true), "an error result: {entries:?}");
        let text = entry.to_string();
        for word in ["doesn't want to proceed", "rejected", "STOP", "for tool use]"] {
            assert!(!text.contains(word), "{word:?} in {entries:?}");
        }
    }
}

/// Where in `entries` the first entry matching `what` is.
fn position(entries: &[Value], what: impl Fn(&Value) -> bool) -> usize {
    entries.iter().position(what).unwrap_or_else(|| panic!("not in {entries:?}"))
}

/// The Done-when: an ask_user round trip reads, to the model, as a
/// success, then at most a plain interrupt marker, then the answer tied
/// to the question.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_ask_user_round_trip_reads_as_a_success_and_its_answer(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut first = vec![json!({"read": true})];
    first.extend(tool_reply(&[("toolu_ask", crate::questions::ASK_USER, delete_question())]));
    first.push(json!({"next_call": true}));
    let fake = FakeCli::new("ask-close", vec![first, vec![json!({"read": true}), text_reply("Deleting.")]]);
    fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("asks");
    fake.until("the run is interrupted", |f| f.stdin(0).iter().any(is_interrupt)).await;
    fake.until("the run has ended", |f| f.dir.join("exited.0").exists()).await;

    assert_eq!(answered(&fake, 0), vec![("toolu_ask".to_string(), ASKED_USER.to_string(), false)], "answered as a success");
    let first = fake.transcript(0);
    let result = position(&first, |e| e["tool_result"] == "toolu_ask");
    let marker = position(&first, |e| e["marker"].is_string());
    assert!(result < marker, "the interrupt came after the answer was taken: {first:?}");

    let answers = vec![crate::questions::QuestionAnswer { selected: vec!["Yes".to_string()], other: None }];
    answer_question(pool.clone(), conversation.id, "toolu_ask".to_string(), answers).await.expect("answered");
    fake.until("the answer's run is sent its message", |f| !user_texts(f, 1).is_empty()).await;
    assert!(fake.args(1).iter().any(|a| a == "--resume"), "the answer's turn resumes the session: {:?}", fake.args(1));
    let sent = &user_texts(&fake, 1)[0];
    let tie = "(This answers your earlier `ask_user` (call toolu_ask), which smelt showed the user; smelt ended that turn while they answered:)\nThe user answered:\n1. Delete: Yes";
    assert!(sent.contains(tie), "{sent}");

    let all = transcripts(&fake, 1);
    assert_no_refusal(&all);
    let marker = position(&all, |e| e["marker"] == "[Request interrupted by user]");
    let answer = position(&all, |e| e["user"].as_str().is_some_and(|text| text.contains(tie)));
    assert!(marker < answer, "the plain marker, then the answer: {all:?}");
}

/// ask_parent, from a child: answered as sent to the parent, a success,
/// before the interrupt.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_ask_parent_is_answered_as_sent_to_the_parent(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let parent = db::create_conversation(&pool).await.expect("parent");
    // A child made directly, as the attention tests make one: ask_parent
    // needs its agent row, not a real start.
    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('c', $1) RETURNING id")
        .bind(parent.id)
        .fetch_one(&pool)
        .await
        .expect("a child");
    sqlx::query("INSERT INTO agents (conversation_id, root_id, depth, brief, state, started_at) VALUES ($1, $2, 1, 'b', 'active', now())")
        .bind(child)
        .bind(parent.id)
        .execute(&pool)
        .await
        .expect("its agent row");
    let mut first = vec![json!({"read": true})];
    first.extend(tool_reply(&[("toolu_up", crate::agents::ASK_PARENT, json!({"question": "Which reading?"}))]));
    first.push(json!({"next_call": true}));
    let fake = FakeCli::new("ask-parent-close", vec![first]);
    fake.pick_for(&pool, child).await;

    run_turn(&pool, child, hello()).await.expect("asks its parent");
    fake.until("the run is interrupted", |f| f.stdin(0).iter().any(is_interrupt)).await;
    fake.until("the run has ended", |f| f.dir.join("exited.0").exists()).await;

    assert_eq!(answered(&fake, 0), vec![("toolu_up".to_string(), ASKED_PARENT.to_string(), false)]);
    let transcript = fake.transcript(0);
    assert_no_refusal(&transcript);
    assert!(position(&transcript, |e| e["tool_result"] == "toolu_up") < position(&transcript, |e| e["marker"].is_string()), "{transcript:?}");
}

/// A reply with a tool and a question: both answered, one at a time, the
/// tool with its real result, before the interrupt.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_with_a_tool_and_ask_user_answers_both_in_order(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut first = vec![json!({"read": true})];
    first.extend(tool_reply(&[("toolu_todo", "todoread", json!({})), ("toolu_ask", crate::questions::ASK_USER, delete_question())]));
    first.push(json!({"next_call": true}));
    let fake = FakeCli::new("tool-and-ask", vec![first, vec![json!({"read": true}), text_reply("Deleting.")]]);
    fake.pick_for(&pool, conversation.id).await;

    run_turn(&pool, conversation.id, hello()).await.expect("asks");
    fake.until("the run is interrupted", |f| f.stdin(0).iter().any(is_interrupt)).await;
    fake.until("the run has ended", |f| f.dir.join("exited.0").exists()).await;

    let saved = db::list_messages(&pool, conversation.id).await.expect("messages");
    let todo = saved
        .iter()
        .flat_map(|m| m.blocks().expect("blocks"))
        .find_map(|b| match b {
            anthropic::ContentBlock::ToolResult { tool_use_id, content, .. } if tool_use_id == "toolu_todo" => Some(content),
            _ => None,
        })
        .expect("todoread's saved result");
    assert_eq!(
        answered(&fake, 0),
        vec![("toolu_todo".to_string(), todo, false), ("toolu_ask".to_string(), ASKED_USER.to_string(), false)],
        "both answered, in the reply's order"
    );
    let transcript = fake.transcript(0);
    assert_no_refusal(&transcript);
    let marker = position(&transcript, |e| e["marker"].is_string());
    assert!(position(&transcript, |e| e["tool_result"] == "toolu_todo") < marker && position(&transcript, |e| e["tool_result"] == "toolu_ask") < marker, "{transcript:?}");

    let answers = vec![crate::questions::QuestionAnswer { selected: vec!["Yes".to_string()], other: None }];
    answer_question(pool.clone(), conversation.id, "toolu_ask".to_string(), answers).await.expect("answered");
    fake.until("the answer's run is sent its message", |f| !user_texts(f, 1).is_empty()).await;
    let sent = &user_texts(&fake, 1)[0];
    assert!(sent.contains("(This answers your earlier `ask_user` (call toolu_ask)"), "the tie is kept: {sent}");
}

/// A CLI that takes the answer and never echoes it: the close gives up
/// within its bound and interrupts, and the next turn still runs.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_close_that_hears_no_echo_interrupts_within_its_bound(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut first = vec![json!({"read": true})];
    let mut reply = tool_reply(&[("toolu_ask", crate::questions::ASK_USER, delete_question())]);
    // The reply's lines, then its call made but never echoed.
    let call = reply.pop().expect("the call step");
    first.extend(reply);
    first.push(json!({"call_mute": call["call"]}));
    let fake = FakeCli::new("mute", vec![first, vec![json!({"read": true}), text_reply("Deleting.")]]);
    fake.pick_for(&pool, conversation.id).await;
    crate::claude_cli::run::close::CLOSE_WAIT_FOR_TESTS.store(300, std::sync::atomic::Ordering::SeqCst);

    let started = std::time::Instant::now();
    let asked = run_turn(&pool, conversation.id, hello()).await;
    let returned = started.elapsed();
    let mut interrupted = None;
    for _ in 0..40 {
        if fake.stdin(0).iter().any(is_interrupt) {
            interrupted = Some(started.elapsed());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    crate::claude_cli::run::close::CLOSE_WAIT_FOR_TESTS.store(0, std::sync::atomic::Ordering::SeqCst);
    asked.expect("asks");
    assert!(returned < std::time::Duration::from_secs(2), "the close runs in the background: {returned:?}");
    let interrupted = interrupted.expect("an interrupt within 2 s of the turn");
    assert!(interrupted >= std::time::Duration::from_millis(300), "the close waited for its echo first: {interrupted:?}");
    assert_eq!(answered(&fake, 0), vec![("toolu_ask".to_string(), ASKED_USER.to_string(), false)], "answered before the interrupt");

    let answers = vec![crate::questions::QuestionAnswer { selected: vec!["Yes".to_string()], other: None }];
    answer_question(pool.clone(), conversation.id, "toolu_ask".to_string(), answers).await.expect("answered");
    fake.until("the answer's run is sent its message", |f| !user_texts(f, 1).is_empty()).await;
}

/// A turn that fails after its tools ran (here the max-turns bound)
/// still answers the call with its real result before it interrupts.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_turn_that_fails_after_its_tools_still_closes(pool: PgPool) {
    crate::db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let _guard = lock_turn_tests();
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    let mut first = vec![json!({"read": true})];
    first.extend(tool_reply(&[("toolu_t", "todoread", json!({}))]));
    first.push(json!({"next_call": true}));
    let fake = FakeCli::new("bounded-close", vec![first]);
    fake.pick_for(&pool, conversation.id).await;

    let error = run_turn_bounded(&pool, TurnInFlight::start(conversation.id), Some(hello()), 1, None, false)
        .await
        .expect_err("one model call is the bound");
    assert!(error.to_string().contains("tool-use loop exceeded"), "{error}");
    fake.until("the run is interrupted", |f| f.stdin(0).iter().any(is_interrupt)).await;
    fake.until("the run has ended", |f| f.dir.join("exited.0").exists()).await;

    let answered = answered(&fake, 0);
    assert_eq!(answered.len(), 1, "{answered:?}");
    assert_eq!((answered[0].0.as_str(), answered[0].2), ("toolu_t", false), "its real result, a success");
    assert_no_refusal(&fake.transcript(0));
}
