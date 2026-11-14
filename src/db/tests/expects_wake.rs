//! R1 (SME-523): `expects_wake`, the cache re-seed's "a wake is expected"
//! condition: a running command, a live child, or its own question to its
//! parent; never a question to the user, nor a finished child.

use super::*;

/// A child agent row under `parent`, in `state`.
async fn child(pool: &PgPool, parent: i64, state: &str) -> i64 {
    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('a child', $1) RETURNING id")
        .bind(parent)
        .fetch_one(pool)
        .await
        .expect("a child row");
    sqlx::query("INSERT INTO agents (conversation_id, root_id, depth, brief, state) VALUES ($1, $2, 1, 'a brief', $3)")
        .bind(child)
        .bind(parent)
        .bind(state)
        .execute(pool)
        .await
        .expect("its agent row");
    child
}

async fn asks(pool: &PgPool, conversation_id: i64, of: &str) {
    let mut conn = pool.acquire().await.expect("a connection");
    upsert_pending_question(&mut conn, conversation_id, "toolu_q", &[], of).await.expect("a question");
}

async fn wakes(pool: &PgPool, conversation_id: i64) -> bool {
    expects_wake(pool, conversation_id).await.expect("expects_wake")
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_running_command_is_expected_to_wake_it(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    assert!(!wakes(&pool, conversation.id).await, "nothing yet");
    let terminal = test_terminal(&pool, conversation.id).await;
    create_terminal_command(&pool, conversation.id, terminal, "wake-cmd", "sleep 600", 300).await.expect("a command");
    assert!(wakes(&pool, conversation.id).await, "its command runs");
    mark_terminal_command_finished(&pool, "wake-cmd", 0).await.expect("finished");
    assert!(!wakes(&pool, conversation.id).await, "its command finished");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_live_child_is_expected_to_wake_it_and_a_finished_one_isnt(pool: PgPool) {
    let starting = test_conversation(&pool).await;
    child(&pool, starting.id, "starting").await;
    assert!(wakes(&pool, starting.id).await, "a starting child");
    let active = test_conversation(&pool).await;
    child(&pool, active.id, "active").await;
    assert!(wakes(&pool, active.id).await, "a child at work");
    let done = test_conversation(&pool).await;
    for state in ["finished", "stopped", "failed"] {
        child(&pool, done.id, state).await;
    }
    assert!(!wakes(&pool, done.id).await, "children that are done");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_question_to_its_parent_is_expected_to_wake_it_and_one_to_the_user_isnt(pool: PgPool) {
    let parent = test_conversation(&pool).await;
    let asking = child(&pool, parent.id, "active").await;
    asks(&pool, asking, ASKED_OF_PARENT).await;
    assert!(wakes(&pool, asking).await, "its parent's answer wakes it");
    let user_only = test_conversation(&pool).await;
    asks(&pool, user_only.id, ASKED_OF_USER).await;
    assert!(!wakes(&pool, user_only.id).await, "the user may be gone for hours");
}
