//! Deleting a conversation deletes its subtree.

use super::*;

async fn exists(pool: &PgPool, id: i64) -> bool {
    db::conversation_exists(pool, id).await.expect("exists")
}

async fn queued_to(pool: &PgPool, to: i64) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT body FROM agent_messages WHERE to_conversation_id = $1 ORDER BY id")
        .bind(to)
        .fetch_all(pool)
        .await
        .expect("queued rows")
}

/// Every agent's model call hangs, so its turn stays in flight.
async fn hanging_agents(pool: &PgPool) {
    db::test_support::start_ids_clear_of_other_runs(pool).await.expect("ids clear of other runs");
    mock_with(pool, |request| {
        let hang = agent_of(request).is_some();
        Box::pin(async move {
            if hang {
                std::future::pending::<()>().await;
            }
            text_reply("ok")
        })
    })
    .await;
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_deleting_a_root_deletes_its_agents_and_tears_each_down(pool: PgPool) {
    hanging_agents(&pool).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let running = start_child(&pool, root, "Run.").await;
    let other = start_child(&pool, root, "Other.").await;
    eventually("the child's turn is in flight", || async { crate::turn::turn_running(running) }).await;
    let pod = db::create_sandbox_pod(&pool, other).await.expect("a pod row").id;

    delete_subtree(&pool, root).await.expect("the delete");
    for id in [root, running, other] {
        assert!(!exists(&pool, id).await, "{id} is deleted");
    }
    assert_eq!(test_hooks::teardown_of(other), Some(vec![pod]), "its pods are torn down");
    assert_eq!(test_hooks::teardown_of(running), Some(vec![]));
    assert_eq!(test_hooks::teardown_of(root), Some(vec![]));
    eventually("the child's turn ends", || async { !crate::turn::turn_running(running) }).await;
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_deleting_a_child_tells_its_parent_and_a_failed_delete_tells_nothing(pool: PgPool) {
    setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = start_child(&pool, root, "Work.").await;
    eventually("the child's first turn is over", || async {
        transcript(&pool, child).await.iter().any(|(role, _)| role == "assistant") && !crate::turn::turn_running(child)
    })
    .await;
    // The parent is told the child's turn ended without a report, after the
    // turn: wait for that check, so it can't land inside the count below.
    eventually("the end-of-turn check ran", || async { test_hooks::last_turns_checked(child) > 0 }).await;
    eventually("the parent's turns are over", || async { !crate::turn::turn_running(root) }).await;
    let before = queued_to(&pool, root).await.len();

    test_hooks::fail_next_delete(child);
    let failed = delete_subtree(&pool, child).await.expect_err("the delete fails");
    assert!(failed.contains("a test failed the delete"), "{failed}");
    assert!(exists(&pool, child).await, "a failed delete deletes nothing");
    assert_eq!(queued_to(&pool, root).await.len(), before, "and tells no one");

    delete_subtree(&pool, child).await.expect("the delete");
    assert!(!exists(&pool, child).await);
    let queued = queued_to(&pool, root).await;
    assert_eq!(queued.last().map(String::as_str), Some(format!("the user deleted agent #{child}").as_str()));
    eventually("the parent reads it", || async {
        transcript(&pool, root).await.iter().any(|(_, text)| *text == format!("Notice: the user deleted agent #{child}"))
    })
    .await;
    eventually("the parent's turns are over", || async { !crate::turn::turn_running(root) }).await;
}

/// A `start_agent` after the delete has marked the subtree is refused, so
/// no agent joins a subtree being deleted.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_start_agent_racing_a_delete_is_refused(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let (reached, release) = test_hooks::pause(test_hooks::DELETE_MARKED, root);
    let delete = tokio::spawn({
        let pool = pool.clone();
        async move { delete_subtree(&pool, root).await }
    });
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the delete runs").expect("paused");

    let refused = crate::anthropic::tools::execute(&pool, root, "toolu_s", "start_agent", &serde_json::json!({"brief": "x"})).await;
    assert_eq!(refused, Err("this conversation is being deleted".to_string()));
    release.send(()).expect("release the delete");
    tokio::time::timeout(Duration::from_secs(10), delete).await.expect("the delete ends").expect("joined").expect("deleted");
    assert!(!exists(&pool, root).await);
}

/// The parent deleted while its child's `finish` holds its rows: the
/// finish completes and the delete follows, with no deadlock.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_finish_holding_its_rows_then_the_parents_delete(pool: PgPool) {
    let _race = crate::git::test_support::ONE_AT_A_TIME.lock().await;
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = start_child(&pool, root, "Work.").await;
    eventually("the child's first turn is over", || async {
        transcript(&pool, child).await.iter().any(|(role, _)| role == "assistant") && !crate::turn::turn_running(child)
    })
    .await;
    let (reached, release) = test_hooks::pause(test_hooks::FINISH_UPDATED, child);
    let finish = tokio::spawn({
        let pool = pool.clone();
        async move {
            crate::anthropic::tools::execute(&pool, child, "toolu_f", "finish", &serde_json::json!({"report": "Done."})).await
        }
    });
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the finish runs").expect("paused");
    let delete = tokio::spawn({
        let pool = pool.clone();
        async move { delete_subtree(&pool, root).await }
    });
    assert!(
        crate::git::test_support::waiters_or_finished(&pool, 1, &delete).await,
        "the delete waits for the finish's rows"
    );
    release.send(()).expect("release the finish");
    let finished = tokio::time::timeout(Duration::from_secs(10), finish).await.expect("the finish ends").expect("joined");
    assert!(finished.as_deref().is_ok_and(|r| r.contains("Your report was sent")), "{finished:?}");
    tokio::time::timeout(Duration::from_secs(10), delete).await.expect("the delete ends").expect("joined").expect("deleted");
    assert!(!exists(&pool, root).await && !exists(&pool, child).await);
}

/// The other order: the delete holds the rows when the finish comes; the
/// finish waits, then finds its parent gone and is refused, with no
/// deadlock.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_parents_delete_holding_its_rows_then_a_finish(pool: PgPool) {
    let _race = crate::git::test_support::ONE_AT_A_TIME.lock().await;
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = start_child(&pool, root, "Work.").await;
    eventually("the child's first turn is over", || async {
        transcript(&pool, child).await.iter().any(|(role, _)| role == "assistant") && !crate::turn::turn_running(child)
    })
    .await;
    let (reached, release) = test_hooks::pause(test_hooks::DELETE_LOCKED, root);
    let delete = tokio::spawn({
        let pool = pool.clone();
        async move { delete_subtree(&pool, root).await }
    });
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the delete runs").expect("paused");
    let finish = tokio::spawn({
        let pool = pool.clone();
        async move {
            crate::anthropic::tools::execute(&pool, child, "toolu_f", "finish", &serde_json::json!({"report": "Done."})).await
        }
    });
    assert!(
        crate::git::test_support::waiters_or_finished(&pool, 1, &finish).await,
        "the finish waits for the delete's rows"
    );
    release.send(()).expect("release the delete");
    tokio::time::timeout(Duration::from_secs(10), delete).await.expect("the delete ends").expect("joined").expect("deleted");
    let finished = tokio::time::timeout(Duration::from_secs(10), finish).await.expect("the finish ends").expect("joined");
    let refused = finished.expect_err("refused: the agent is gone");
    assert!(!refused.contains("deadlock"), "{refused}");
}
