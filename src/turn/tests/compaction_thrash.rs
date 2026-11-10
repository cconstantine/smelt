//! SME-414: the compaction loop survives when each step is bigger than the
//! room a compaction leaves. Counted, the model is told at `NOTICE_AT`; at
//! `END_AT` the turn ends, wakes stay stopped until someone writes or a
//! message queues, and the user's send gets fresh runway. Red on the pre-fix
//! branch (recorded in the ticket trail): the ticket's shape finished `Ok`
//! with 30 summaries.

use super::compaction_progress::{step, stepping_mock, summaries};
use super::window_stop::set_context_window;
use super::*;

/// The ticket's shape: window 100,000, every step 60,000 in / 25,000 out —
/// every projection clears the 83,616 trigger, no measurement is ever
/// `tight`, none near `limit`, nothing refused. Main looped to the end of
/// the queue; the fix ends the turn at `END_AT` with the loop named.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_ticket_shape_ends_the_turn_not_the_loop(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let seen = stepping_mock(&pool, vec![step(60_000, 25_000); 30]).await;
    set_context_window(&pool, 100_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");

    let error = run_turn(&pool, conversation.id, hello()).await.expect_err("the loop ends the turn");

    let error = error.to_string();
    assert!(END_AT > NOTICE_AT, "D1: the notice must fire while the loop can still bend");
    assert_eq!(summaries(&pool, conversation.id).await, END_AT as usize, "{error}");
    assert!(error.contains("one step between each"), "{error}");
    assert!(error.contains("it is the steps that don't"), "the loop, not the fit: {error}");
    assert_eq!(notices(&pool, conversation.id).await, 1, "the model was told once, at NOTICE_AT");
    assert_eq!(seen.count(), END_AT as usize + 1, "the request that would follow the last compaction was never sent");
}

/// R3: what the turn learned holds for the wakes after it — none of them
/// pays for a step or a summary against a stopped loop.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_wakes_after_an_armed_thrash_turn_call_nothing(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let seen = stepping_mock(&pool, vec![step(60_000, 25_000); 30]).await;
    set_context_window(&pool, 100_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    run_turn(&pool, conversation.id, hello()).await.expect_err("the loop ends it");
    let (calls, summarized) = (seen.count(), summaries(&pool, conversation.id).await);
    let pod = db::create_sandbox_pod(&pool, conversation.id).await.expect("create sandbox pod");
    let terminal = db::create_sandbox_terminal(&pool, pod.id).await.expect("create sandbox terminal");

    for round in 0..3 {
        let command = format!("sme-414-r3-{round}");
        db::create_terminal_command(&pool, conversation.id, terminal.id, &command, "echo hi", 300).await.expect("create terminal command");
        db::mark_terminal_command_finished(&pool, &command, 0).await.expect("mark terminal command finished");
        wake_conversation(&pool, conversation.id).await.expect_err("the stop holds");
    }

    assert_eq!(seen.count(), calls, "no model step");
    assert_eq!(summaries(&pool, conversation.id).await, summarized, "no summary");
    assert_eq!(notices(&pool, conversation.id).await, 1, "and the notice is told once, not once per wake");
}

/// R4: the user writing is the way forward — a fresh full try, and the
/// smaller steps that fit finish the ask.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_users_send_after_a_thrash_stop_gets_fresh_runway(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    // One more big step than the end threshold thrashes the turn to its
    // end (the size derives from END_AT, per D1); after the user writes,
    // small ones finish it.
    let mut steps = vec![step(60_000, 25_000); END_AT as usize + 1];
    steps.extend(std::iter::repeat_with(|| step(30_000, 1_000)).take(3));
    stepping_mock(&pool, steps).await;
    set_context_window(&pool, 100_000, None).await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    run_turn(&pool, conversation.id, hello()).await.expect_err("the loop ends it");
    let mut rx = events::subscribe(conversation.id);

    start_turn(pool.clone(), conversation.id, "Now in smaller pieces.".to_string()).await.expect("send");
    wait_until_idle(&mut rx).await;

    let messages = db::list_messages(&pool, conversation.id).await.expect("messages");
    assert_eq!(last_reply(&messages).as_deref(), Some("Done."), "the user's send got a full try and finished");
}

/// R5: a message queued by `message_agent` lifts the stop — an armed agent
/// must still be steerable by its parent — and the ordinary wakes after it
/// go back to costing nothing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_queued_message_lifts_the_armed_stop(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let seen = stepping_mock(&pool, vec![step(60_000, 25_000); END_AT as usize + 1]).await;
    set_context_window(&pool, 100_000, None).await;
    let parent = db::create_conversation(&pool).await.expect("create parent");
    let child = active_child(&pool, parent.id).await;
    run_turn(&pool, child, hello()).await.expect_err("the child ends, armed");
    assert!(
        crate::turn::with_compaction_progress(child, |p| p.is_thrashing()),
        "the child is armed after its turn ends"
    );

    let mut rx = events::subscribe(child);
    crate::agents::message_agent(&pool, parent.id, &serde_json::json!({"to": child.to_string(), "text": "keep going, in smaller pieces."}))
        .await
        .expect("queued");
    // The clear is what's under test, and its own wake is what steers the
    // child: wait for that spell rather than racing it with a second wake.
    assert!(
        !crate::turn::with_compaction_progress(child, |p| p.is_thrashing()),
        "the queued message lifted the stop"
    );
    wait_until_idle(&mut rx).await;

    assert!(seen.count() > END_AT as usize + 1, "the armed child was steered: the wake called the model");
}

/// How many saved messages carry the grey notice's marker phrase. The
/// failure wording is past tense ("a compaction left"), so this counts the
/// notice alone.
async fn notices(pool: &PgPool, id: i64) -> usize {
    db::list_messages(pool, id)
        .await
        .expect("messages")
        .iter()
        .filter(|m| {
            m.blocks().unwrap_or_default().iter().any(|b| match b {
                anthropic::ContentBlock::Text { text } => text.contains("bigger than the room a compaction leaves"),
                _ => false,
            })
        })
        .count()
}

/// A child of `parent` made directly, active and started, as the agents
/// tests make one: `message_agent` needs the parented conversation.
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
