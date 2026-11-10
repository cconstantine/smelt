//! SME-455: a live seat that ends its turn owing its parent a report with
//! nothing running is nudged first — the user's own words, queued with the
//! owed column in one transaction so a restart loses nothing — and only
//! the *second* such plain end tells the parent. A seat waiting on a
//! command, a running child, a pending wake or its question to the user is
//! left alone entirely, and the owed state clears when the seat reports.

use super::*;
use super::restart_owed::{command, quiet_parent};

/// The user's own words for the nudge (the SME-455 comment of 2026-11-10),
/// verbatim — no trailing period, no other character changed, and pinned
/// here as a literal and never through the const, the `IDLE_NOTICE_FRAGMENT`
/// convention. If the wording ever changes, these tests go red and say
/// where to settle it: `crate::agents::queue::REPORT_OWE_NUDGE`, with the
/// user, on SME-455.
const NUDGE: &str = "you ended your turn owing your parent a report, and nothing is running that will wake you; check your state";

/// The nudge as its delivery saves it, marked like every notice (SME-357).
pub(super) fn marked_nudge() -> String {
    crate::api::chat::mark_notice(NUDGE)
}

async fn queued_to(pool: &PgPool, to: i64) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT body FROM agent_messages WHERE to_conversation_id = $1 ORDER BY id")
        .bind(to)
        .fetch_all(pool)
        .await
        .expect("queued rows")
}

async fn owed_at(pool: &PgPool, id: i64) -> Option<chrono::NaiveDateTime> {
    sqlx::query_scalar::<_, Option<chrono::NaiveDateTime>>("SELECT report_owed_at FROM agents WHERE conversation_id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the seat's agents row")
}

/// How many of `id`'s messages are the delivered nudge, marked and whole.
async fn nudges_in(pool: &PgPool, id: i64) -> usize {
    let marked = marked_nudge();
    transcript(pool, id).await.iter().filter(|(_, text)| text == &marked).count()
}

fn nudge_words_changed() -> String {
    format!(
        "the nudge's words are pinned by SME-455's user comment; if they changed, \
         settle it there and move `REPORT_OWE_NUDGE` and this literal together: {NUDGE:?}"
    )
}

/// A seat whose stuck first turn end is observed: waits for its
/// `after_last_turn` check to have run (a fresh id starts at none).
async fn ended_once(seat: i64) {
    eventually("the end-of-turn check ran", || async { test_hooks::last_turns_checked(seat) > 0 }).await;
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_seat_that_ends_its_turn_owing_a_report_is_nudged_once(pool: PgPool) {
    // The predicate now reads `agent_messages` by conversation id, and a
    // shared Postgres holds other runs' rows: keep the ids clear (as the
    // other chain tests do), or another run's row for this seat's id is
    // read as a wake pending.
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    // The seat answers its first turn with text and goes silent after: its
    // nudge-woken turn never ends, so the second stuck end — the one that
    // tells the parent, test 2 — can't race these assertions.
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let seen = calls.clone();
    mock_with(&pool, move |request| {
        let hang = agent_of(request).is_some() && seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0;
        Box::pin(async move {
            if hang {
                std::future::pending::<()>().await;
            }
            text_reply("Still waiting on CI.")
        })
    })
    .await;
    let parent = db::create_conversation(&pool).await.expect("create conversation").id;
    let seat = start_child(&pool, parent, "Work.").await;
    ended_once(seat).await;

    eventually("the nudge reaches the seat's transcript", || async { nudges_in(&pool, seat).await > 0 }).await;
    assert_eq!(nudges_in(&pool, seat).await, 1, "{}", nudge_words_changed());
    // Red today: the parent gets its notice at the first end, not the seat.
    assert!(queued_to(&pool, parent).await.is_empty(), "the first plain stuck end nudges, it tells no one");
    assert!(owed_at(&pool, seat).await.is_some(), "the owed column marks the nudge's spell");

    crate::turn::stop_conversation(&pool, seat).await;
    eventually("the hung turn ends", || async { !crate::turn::turn_running(seat) }).await;
    for id in [parent, seat] {
        crate::turn::forget_conversation_lock(id);
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_second_and_later_stuck_ends_tell_the_parent_and_never_loop(pool: PgPool) {
    // Every reply the same long text, so the escalation's 500-character
    // quote rule is asserted here (it moved with the parent notice).
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let long = "x".repeat(450) + &"y".repeat(150);
    let quote = long.chars().take(500).collect::<String>();
    let reply = long.clone();
    let requests = mock_with(&pool, move |_| {
        let reply = reply.clone();
        Box::pin(async move { text_reply(&reply) })
    })
    .await;
    let parent = db::create_conversation(&pool).await.expect("create conversation").id;
    let seat = start_child(&pool, parent, "Work.").await;

    // The chain does both ends by itself: nudge, wake, end, tell.
    let expected = format!("agent #{seat}'s turn ended without a report; its last message: {quote}");
    eventually("the parent is told at the second end", || async { queued_to(&pool, parent).await.len() == 1 }).await;
    assert_eq!(queued_to(&pool, parent).await, vec![expected], "the escalation reads as today's notice, quote rule and all");
    assert_eq!(nudges_in(&pool, seat).await, 1, "nudged once, once only");
    let seat_calls = requests_for(&requests, Some(seat));
    assert_eq!(seat_calls.len(), 2, "the seat made exactly 2 model calls: the turn and the nudge's");
    eventually("every turn is over", || async { !crate::turn::turn_running(parent) && !crate::turn::turn_running(seat) }).await;
    assert!(owed_at(&pool, seat).await.is_some(), "the owed mark stands until the seat reports");

    // Nothing loops: with both turns over, the request log grows no more.
    let before = requests.lock().expect("the request log").len();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(requests.lock().expect("the request log").len(), before, "the nudge-escalate pair looped");
    for id in [parent, seat] {
        crate::turn::forget_conversation_lock(id);
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_seat_waiting_on_a_running_command_is_not_nudged(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    mock_upstream(&pool, vec![text_reply("ok")]).await;
    let parent = quiet_parent(&pool).await;
    let seat = active_child(&pool, parent).await;
    command(&pool, seat, "stuck-running", false).await;

    crate::agents::after_last_turn(&pool, seat, None).await;

    assert!(queued_to(&pool, seat).await.is_empty(), "a running command means no nudge");
    assert!(queued_to(&pool, parent).await.is_empty(), "and no parent notice");
    assert!(owed_at(&pool, seat).await.is_none(), "and no owed mark");
    for id in [parent, seat] {
        crate::turn::forget_conversation_lock(id);
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_seat_with_a_running_child_is_not_nudged(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    mock_upstream(&pool, vec![text_reply("ok")]).await;
    let parent = quiet_parent(&pool).await;
    let seat = active_child(&pool, parent).await;
    active_child(&pool, seat).await;

    crate::agents::after_last_turn(&pool, seat, None).await;

    assert!(queued_to(&pool, seat).await.is_empty(), "an agent of its own running means no nudge");
    assert!(queued_to(&pool, parent).await.is_empty(), "and no parent notice");
    assert!(owed_at(&pool, seat).await.is_none(), "and no owed mark");
    for id in [parent, seat] {
        crate::turn::forget_conversation_lock(id);
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_seat_with_a_pending_wake_is_not_nudged(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    mock_upstream(&pool, vec![text_reply("ok")]).await;
    let parent = quiet_parent(&pool).await;
    // One seat holds an undelivered `agent_messages` row (queued, never
    // woken), the other a command end recorded but never drained — each on
    // its own is a wake coming; nudging now would spend the nudge and then
    // the real wake's turn ends owe nothing. Red today, which tells both
    // parents.
    let by_message = active_child(&pool, parent).await;
    db::queue_notice(&pool, by_message, parent, "parent", "A note for later").await.expect("queue it");
    let by_command = active_child(&pool, parent).await;
    command(&pool, by_command, "stuck-undrained", true).await;

    for seat in [by_message, by_command] {
        crate::agents::after_last_turn(&pool, seat, None).await;
    }

    for seat in [by_message, by_command] {
        assert!(queued_to(&pool, seat).await.iter().all(|b| b != &marked_nudge()), "#{seat}: a wake pending means no nudge");
        assert!(owed_at(&pool, seat).await.is_none(), "#{seat}: and no owed mark");
    }
    assert!(queued_to(&pool, parent).await.is_empty(), "a pending wake tells the parent nothing either");
    for id in [parent, by_message, by_command] {
        crate::turn::forget_conversation_lock(id);
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_seat_with_a_question_to_the_user_is_not_nudged(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    mock_upstream(&pool, vec![text_reply("ok")]).await;
    let parent = quiet_parent(&pool).await;
    let seat = active_child(&pool, parent).await;
    sqlx::query("INSERT INTO pending_questions (conversation_id, tool_use_id, questions, asked_of) VALUES ($1, 'toolu_stuck', '[]', 'user')")
        .bind(seat)
        .execute(&pool)
        .await
        .expect("an ask_user waiting on its answer");

    crate::agents::after_last_turn(&pool, seat, None).await;

    assert!(queued_to(&pool, seat).await.is_empty(), "a question waiting means no nudge");
    assert!(queued_to(&pool, parent).await.is_empty(), "and no parent notice");
    assert!(owed_at(&pool, seat).await.is_none(), "and no owed mark");
    for id in [parent, seat] {
        crate::turn::forget_conversation_lock(id);
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_nudge_survives_a_restart_once(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let requests = setup(&pool, vec![text_reply("Still waiting on CI.")]).await;
    let parent = db::create_conversation(&pool).await.expect("create conversation").id;
    let seat = active_child(&pool, parent).await;

    // The same call the turn-end branch makes, landing just before a
    // restart: the row waits, and the startup sweep wakes it.
    let queued = db::queue_report_owed_nudge(&pool, seat, &marked_nudge()).await.expect("the nudge queues");
    assert!(queued, "the first owed end queues");
    assert_eq!(queued_to(&pool, seat).await.len(), 1, "the nudge row");
    assert!(owed_at(&pool, seat).await.is_some(), "and the owed column, together");

    restart(&pool, &[parent, seat]).await;
    eventually("the sweep delivers the nudge once", || async {
        nudges_in(&pool, seat).await == 1 && !requests_for(&requests, Some(seat)).is_empty()
    })
    .await;
    // The seat answers, ends stuck again — and the second end tells.
    eventually("the parent is told", || async { queued_to(&pool, parent).await.iter().any(|b| b.contains("without a report")) }).await;
    eventually("every turn is over", || async { !crate::turn::turn_running(seat) && !crate::turn::turn_running(parent) }).await;

    let before = requests.lock().expect("the request log").len();
    restart(&pool, &[parent, seat]).await;
    assert_eq!(requests.lock().expect("the request log").len(), before, "a second restart answered something new");
    assert_eq!(nudges_in(&pool, seat).await, 1, "the nudge, once — neither lost nor doubled");
    let rows = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1 AND kind = 'server_notice'")
        .bind(seat)
        .fetch_one(&pool)
        .await
        .expect("count the nudge rows");
    assert_eq!(rows, 1, "one server_notice row");
    for id in [parent, seat] {
        crate::turn::forget_conversation_lock(id);
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_reporting_to_the_parent_clears_the_owed_state(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    // The seat never answers the model: its nudge-woken turn hangs at the
    // first call, so its only ends are the ones driven below.
    mock_with(&pool, move |request| {
        let hang = agent_of(request).is_some();
        Box::pin(async move {
            if hang {
                std::future::pending::<()>().await;
            }
            text_reply("ok")
        })
    })
    .await;
    let parent = quiet_parent(&pool).await;
    let seat = active_child(&pool, parent).await;

    // First stuck end: nudged, owed.
    crate::agents::after_last_turn(&pool, seat, None).await;
    eventually("the nudge reaches the seat", || async { nudges_in(&pool, seat).await == 1 }).await;
    assert!(owed_at(&pool, seat).await.is_some());

    // Its report clears; a later stuck end nudges afresh.
    let outcome = db::message_agent(&pool, seat, db::Recipient::Parent, "seat", "Here is my report.")
        .await
        .expect("message_agent");
    assert_eq!(outcome, db::MessageOutcome::Queued(parent));
    assert!(owed_at(&pool, seat).await.is_none(), "reporting to the parent clears the owed state");
    crate::agents::after_last_turn(&pool, seat, None).await;
    eventually("nudged afresh", || async { queued_to(&pool, seat).await.iter().filter(|b| *b == &marked_nudge()).count() == 2 }).await;
    assert!(owed_at(&pool, seat).await.is_some(), "a fresh spell, owed afresh");

    // Deliver the second nudge (its wake's turn hangs at the model, as set
    // up), then finish: the finish's own transaction clears.
    let row = {
        let mut rows = db::undelivered_agent_messages(&pool, seat).await.expect("undelivered");
        assert_eq!(rows.len(), 1, "the second nudge waits undelivered");
        rows.remove(0)
    };
    let content = [ContentBlock::Text { text: marked_nudge() }];
    db::deliver_agent_message(&pool, &row, &content).await.expect("deliver it").expect("delivered");
    let finish = db::finish_agent(&pool, seat, "seat", "Done.")
        .await
        .expect("finish_agent");
    assert_eq!(finish, db::Finish::Queued { parent }, "the finished seat was owed a fresh nudge — finish clears it");
    assert!(owed_at(&pool, seat).await.is_none(), "a finished seat never holds the column");

    // ask_parent is in contact too: it clears the same way.
    let seat2 = active_child(&pool, parent).await;
    sqlx::query("UPDATE agents SET report_owed_at = now() WHERE conversation_id = $1")
        .bind(seat2)
        .execute(&pool)
        .await
        .expect("set the mark");
    let asked = db::ask_parent(&pool, seat2, "toolu_stuck", "Which way?", "seat2")
        .await
        .expect("ask_parent");
    assert!(matches!(asked, db::AskParent::Asked { .. }), "{asked:?}");
    assert!(owed_at(&pool, seat2).await.is_none(), "a question to the parent clears the owed state");

    for body in queued_to(&pool, parent).await {
        assert!(!body.contains("without a report"), "this seat always stayed in contact: {body:?}");
    }
    crate::turn::stop_conversation(&pool, seat).await;
    eventually("the hung turn ends", || async { !crate::turn::turn_running(seat) }).await;
    for id in [parent, seat, seat2] {
        crate::turn::forget_conversation_lock(id);
    }
}
