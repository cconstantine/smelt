//! SME-450 review round 1: the edges of the answer-owed mark and the
//! queued `Deliver` — no mark where no turn will answer, a second admitted
//! wake answering nothing, and the age bound on undrained ends.

use super::restart_owed::{command, command_notice, docker_restarted, drain_command, requests_ending_in};
use super::*;

/// How many of `id`'s queued rows are still undelivered.
async fn undelivered(pool: &PgPool, id: i64) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1 AND delivered_at IS NULL")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("count")
}

/// A second ended command in `conversation_id`, on the terminal its first
/// one ran in (`restart_owed::command` makes the pod and terminal).
async fn another_ended_command(pool: &PgPool, conversation_id: i64, command_id: &str) {
    let terminal = sqlx::query_scalar::<_, i64>("SELECT terminal_id FROM terminal_commands WHERE conversation_id = $1 LIMIT 1")
        .bind(conversation_id)
        .fetch_one(pool)
        .await
        .expect("its terminal");
    db::create_terminal_command(pool, conversation_id, terminal, command_id, "echo hi", 300).await.expect("create it");
    db::mark_terminal_command_finished(pool, command_id, 0).await.expect("mark it finished");
}

/// D10 (review 1, L2): a `Deliver` whose wake stops at a question still
/// waiting for the user is shown at once, as before the queue, and owes
/// nothing: no turn will answer it until the question resolves.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_deliver_while_a_question_waits_is_saved_at_once(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let questions = vec![crate::questions::Question {
        question: "Delete the build directory?".to_string(),
        header: "Delete".to_string(),
        options: vec![],
        multi_select: false,
    }];
    db::create_pending_question(&pool, root, "toolu_ask", &questions).await.expect("create the question");
    let text = docker_restarted();
    let deliver = crate::turn::notify(&pool, root, vec![crate::turn::Notice::Deliver(text.clone())]);
    tokio::time::timeout(Duration::from_secs(10), deliver).await.expect("the delivery ends").expect("the task");

    let told = transcript(&pool, root).await.into_iter().filter(|(_, said)| said == &text).count();
    assert_eq!(told, 1, "the notice, once");
    assert_eq!(undelivered(&pool, root).await, 0, "its row is still queued");
    assert!(!db::answer_owed(&pool, root).await.expect("answer_owed"), "the saved notice marked it owed");
    let question = db::get_pending_question(&pool, root).await.expect("the question").expect("still there");
    assert!(question.answer.is_none(), "the question was answered");
    assert_eq!(requests.lock().expect("the request log").len(), 0, "the model was called");
    crate::turn::forget_conversation_lock(root);
}

/// Review 1, L3: two wakes both admitted, queued on the turn lock, for a
/// conversation owed an answer. The first answers it; the second, under
/// the lock, finds the mark cleared and nothing pending, and calls nothing.
/// (An owed conversation with nothing pending admits only one wake: the
/// second's pre-check sees the first counted, T9. So the first end here is
/// drained, which marks it owed, and a second is pending, which lets both
/// in.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_two_admitted_wakes_answer_an_owed_turn_once(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    command(&pool, root, "l3-drained", true).await;
    drain_command(&pool, root, "l3-drained").await;
    another_ended_command(&pool, root, "l3-pending").await;

    let turn = crate::turn::conversation_lock(root).lock_owned().await;
    let mut wakes = Vec::new();
    for _ in 0..2 {
        let (admitted, release) = crate::turn::test_hooks::pause_after_notice_admitted(root);
        wakes.push(crate::turn::notify(&pool, root, vec![crate::turn::Notice::Wake]));
        tokio::time::timeout(Duration::from_secs(10), admitted).await.expect("the wake is let in").expect("the hook");
        release.send(()).expect("the wake waits at the hook");
    }
    drop(turn);
    for wake in wakes {
        tokio::time::timeout(Duration::from_secs(30), wake).await.expect("the wake ends").expect("the wake");
    }

    assert_eq!(requests.lock().expect("the request log").len(), 1, "the owed turn was answered more than once");
    assert_eq!(requests_ending_in(&requests, &command_notice("l3-pending")), 1, "the request");
    assert!(!db::answer_owed(&pool, root).await.expect("answer_owed"), "still owed");
    crate::turn::forget_conversation_lock(root);
}
