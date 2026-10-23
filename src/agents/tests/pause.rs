//! The stored pause (SME-238): a Stop's pause is written to `paused_at`,
//! the user's send clears it, and a restart keeps it.

use super::*;

async fn undelivered(pool: &PgPool, to: i64) -> usize {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1 AND delivered_at IS NULL")
        .bind(to)
        .fetch_one(pool)
        .await
        .expect("count the queued rows") as usize
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_is_stored_and_the_users_send_clears_it(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;

    user_stop(&pool, root).await.expect("the Stop");
    assert!(paused_at(&pool, root).await.is_some(), "the Stop is stored");
    crate::turn::start_turn(pool.clone(), root, "Go on.".to_string()).await.expect("the send");
    eventually("the send clears it", || async { paused_at(&pool, root).await.is_none() }).await;
    eventually("the turn is over", || async { !crate::turn::turn_running(root) }).await;

    let child = active_child(&pool, root).await;
    crate::anthropic::tools::execute(&pool, root, "toolu_x", "stop_agent", &serde_json::json!({"id": child}))
        .await
        .expect("stop_agent");
    assert!(paused_at(&pool, child).await.is_some(), "stop_agent's pause is stored too");
    crate::turn::forget_conversation_lock(root);
    crate::turn::forget_conversation_lock(child);
}

/// A root and a child, both paused by the user with something queued to
/// each: a restart wakes neither, by the sweep or by a later wake, and the
/// user's send in the root drains its row into that turn.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_paused_conversation_stays_paused_across_a_restart(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = active_child(&pool, root).await;
    user_stop(&pool, root).await.expect("stop the root");
    // The Stop itself, without `user_stop`'s notice to the parent, whose
    // wake would be the test's own task.
    crate::turn::stop_conversation(&pool, child).await;
    // Queued directly, so no wake of the test's own is still running when
    // the restart forgets the turn state (a real restart ends those).
    let mut conn = pool.acquire().await.expect("a connection");
    db::queue_agent_message(&mut conn, root, Some(child), &format!("agent #{child}"), db::KIND_REPORT, "Done.")
        .await
        .expect("a report to the root");
    db::queue_agent_message(&mut conn, child, None, "smelt", db::KIND_NOTICE, "For you.").await.expect("a notice to the child");
    drop(conn);

    restart(&pool, &[root, child]).await;
    assert_eq!(requests.lock().expect("log").len(), 0, "the sweep woke a paused conversation");
    assert_eq!((undelivered(&pool, root).await, undelivered(&pool, child).await), (1, 1));
    // Anything else that wakes them (a finished command, say) is refused
    // too: the pause is back in memory, not only skipped by the sweep.
    crate::turn::notify(&pool, root, vec![crate::turn::Notice::Wake]).await.expect("the wake");
    crate::turn::notify(&pool, child, vec![crate::turn::Notice::Wake]).await.expect("the wake");
    assert_eq!(requests.lock().expect("log").len(), 0, "a wake ran a paused conversation's turn");

    crate::turn::start_turn(pool.clone(), root, "I'm back.".to_string()).await.expect("the send");
    eventually("the root's turn is over", || async {
        !crate::turn::turn_running(root) && requests_for(&requests, None).len() == 1
    })
    .await;
    let request = requests_for(&requests, None).remove(0).to_string();
    assert!(request.contains(&format!("Report from agent #{child}:\\n\\nDone.")), "{request}");
    assert_eq!(undelivered(&pool, root).await, 0);
    assert_eq!(undelivered(&pool, child).await, 1, "the child waits for the user");
    crate::turn::forget_conversation_lock(root);
    crate::turn::forget_conversation_lock(child);
}

/// Interleaving: the Stop's `store_pause` reads `paused = true` and is
/// held before its write; the user's send ends the pause and stores that;
/// the Stop's write lands last. The row ends as memory says: not paused.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stop_and_a_send_racing_leave_the_row_as_memory_says(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let (reached, release) = test_hooks::pause(test_hooks::STORE_PAUSE_READ, root);
    let stop = tokio::spawn({
        let pool = pool.clone();
        async move { user_stop(&pool, root).await }
    });
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the Stop reaches its write").expect("paused");
    crate::turn::start_turn(pool.clone(), root, "Go on.".to_string()).await.expect("the send");
    // The send's own store, in a task of its own, is done: the Stop's
    // write is the last.
    eventually("the send's store is done", || async { test_hooks::stores_done(root) == 1 }).await;
    release.send(()).expect("release the Stop");
    tokio::time::timeout(Duration::from_secs(10), stop).await.expect("the Stop ends").expect("its task").expect("the Stop");

    assert!(!crate::turn::is_paused(root), "the send came last");
    assert_eq!(paused_at(&pool, root).await, None, "the row says what memory says");
    eventually("the turn is over", || async { !crate::turn::turn_running(root) }).await;
    crate::turn::forget_conversation_lock(root);
}
