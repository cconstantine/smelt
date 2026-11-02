//! Races between a child's `create_child` and a stop or a delete of the
//! subtree it's in (SME-354): each forces one named interleaving by
//! pausing `create_child` holding its caller's row, its checks passed.

use super::*;
use crate::agents::test_hooks;
use std::time::{Duration, Instant};

/// These tests hold several connections at once; they take turns
/// (testing.md: a held-row race test shares sqlx's parent pool).
static HELD_ROWS: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// An `active` agent under `parent`, rooted at `root`, at `depth`.
async fn active_agent(pool: &PgPool, parent: i64, root: i64, depth: i32) -> i64 {
    let id = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('a', $1) RETURNING id")
        .bind(parent)
        .fetch_one(pool)
        .await
        .expect("a conversation");
    sqlx::query(
        "INSERT INTO agents (conversation_id, root_id, depth, brief, state, started_at)
         VALUES ($1, $2, $3, 'b', 'active', now())",
    )
    .bind(id)
    .bind(root)
    .bind(depth)
    .execute(pool)
    .await
    .expect("an agent row");
    id
}

fn new_child(caller: i64) -> NewChild<'static> {
    NewChild { caller, brief: "b", persona_name: None, model: None, repo: None, max_depth: 5, volumes: &[] }
}

async fn lock_waiters(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock'")
        .fetch_one(pool)
        .await
        .expect("pg_stat_activity")
}

/// Waits until `task` has ended or is waiting on a row lock: the point
/// where the paused `create_child` may go on without racing it.
async fn ended_or_waiting<T>(pool: &PgPool, task: &tokio::task::JoinHandle<T>) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !task.is_finished() && lock_waiters(pool).await == 0 {
        assert!(Instant::now() < deadline, "the task neither ended nor waited on a lock within 10 s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `caller`'s `create_child`, paused holding its row; returns the task
/// and the release.
async fn paused_create_child(
    pool: &PgPool,
    caller: i64,
) -> (tokio::task::JoinHandle<CreateChild>, tokio::sync::oneshot::Sender<()>) {
    let (reached, release) = test_hooks::pause(test_hooks::CREATE_CHILD_LOCKED, caller);
    let creating = tokio::spawn({
        let pool = pool.clone();
        async move { create_child(&pool, &new_child(caller)).await.expect("create_child") }
    });
    reached.await.expect("create_child holds its caller's row");
    (creating, release)
}

/// Interleaving: the stopped child's `create_child` holds its row and
/// has read itself `active` when `stop_subtree` runs, then commits a
/// grandchild. The stop must take the grandchild too, or it runs under a
/// stopped parent (B1).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_takes_the_grandchild_its_child_is_creating(pool: PgPool) {
    let _turn = HELD_ROWS.lock().await;
    let root = test_conversation(&pool).await.id;
    let child = active_agent(&pool, root, root, 1).await;
    let (creating, release) = paused_create_child(&pool, child).await;
    let stopping = tokio::spawn({
        let pool = pool.clone();
        async move { stop_subtree(&pool, root, child).await.expect("stop_subtree") }
    });
    ended_or_waiting(&pool, &stopping).await;
    release.send(()).expect("release create_child");
    let CreateChild::Created { child: grandchild, .. } = creating.await.expect("create_child's task") else {
        panic!("the child was still active when it locked its row");
    };
    let stopped = stopping.await.expect("stop_subtree's task");
    assert_eq!(stopped, StopSubtree::Stopped(vec![child, grandchild]), "the stop covers the grandchild");
    assert_eq!(agent_state(&pool, grandchild).await.expect("state").as_deref(), Some(AGENT_STOPPED));
}

/// Interleaving: a delete of the root marks its subtree while an agent
/// below the root holds its own row in `create_child`. The delete must
/// count the grandchild in its subtree, or its teardown misses it (B1).
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_delete_takes_the_grandchild_a_non_root_agent_is_creating(pool: PgPool) {
    let _turn = HELD_ROWS.lock().await;
    let root = test_conversation(&pool).await.id;
    let child = active_agent(&pool, root, root, 1).await;
    let (creating, release) = paused_create_child(&pool, child).await;
    let marking = tokio::spawn({
        let pool = pool.clone();
        async move { mark_subtree_deleting(&pool, root).await.expect("mark_subtree_deleting") }
    });
    ended_or_waiting(&pool, &marking).await;
    release.send(()).expect("release create_child");
    let CreateChild::Created { child: grandchild, .. } = creating.await.expect("create_child's task") else {
        panic!("the child wasn't being deleted when it locked its row");
    };
    let deleting = marking.await.expect("mark_subtree_deleting's task").expect("the root exists");
    assert_eq!(deleting.subtree, vec![root, child, grandchild], "the delete covers the grandchild");
    assert_eq!(agent_state(&pool, grandchild).await.expect("state").as_deref(), Some(AGENT_STOPPED));
}
