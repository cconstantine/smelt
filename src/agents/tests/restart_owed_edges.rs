//! SME-450 review round 1: the edges of the answer-owed mark and the
//! queued `Deliver` — no mark where no turn will answer, a second admitted
//! wake answering nothing, and the age bound on undrained ends.

use super::restart_owed::docker_restarted;
use super::*;

/// How many of `id`'s queued rows are still undelivered.
async fn undelivered(pool: &PgPool, id: i64) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1 AND delivered_at IS NULL")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("count")
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
