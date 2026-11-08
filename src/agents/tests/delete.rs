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
    // SME-354 B8: and leaves it as it was, not marked deleting with its
    // agent stopped, which would refuse every later start_agent in it.
    let deleting_at = sqlx::query_scalar::<_, Option<chrono::NaiveDateTime>>("SELECT deleting_at FROM conversations WHERE id = $1")
        .bind(child)
        .fetch_one(&pool)
        .await
        .expect("the child's row");
    assert_eq!(deleting_at, None, "the failed delete's mark is cleared");
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_ACTIVE), "its state is restored");

    delete_subtree(&pool, child).await.expect("the delete");
    assert!(!exists(&pool, child).await);
    let queued = queued_to(&pool, root).await;
    assert_eq!(queued.last().map(String::as_str), Some(format!("the user deleted agent #{child}").as_str()));
    eventually("the parent reads it", || async {
        // SME-357: the delivered notice carries the mark before its words.
        transcript(&pool, root)
            .await
            .iter()
            .any(|(_, text)| *text == format!("{}Notice: the user deleted agent #{child}", crate::api::chat::NOTICE_MARK))
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

/// A child `X` of a fresh root with one notice `M` queued from it to the
/// root: what the root's drain delivers while the user deletes `X`.
async fn child_with_a_queued_notice(pool: &PgPool) -> (i64, i64, db::AgentMessage) {
    setup(pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(pool).await.expect("create conversation").id;
    let child = active_child(pool, root).await;
    db::queue_notice(pool, root, child, &format!("agent #{child}"), "ended without a report").await.expect("queue M");
    let row = db::undelivered_agent_messages(pool, root).await.expect("rows").remove(0);
    (root, child, row)
}

fn deliver(pool: &PgPool, row: &db::AgentMessage) -> tokio::task::JoinHandle<Result<Option<crate::models::Message>, sqlx::Error>> {
    let (pool, row) = (pool.clone(), row.clone());
    tokio::spawn(async move {
        let text = vec![ContentBlock::Text { text: "Notice: ended without a report".to_string() }];
        db::deliver_agent_message(&pool, &row, &text).await
    })
}

/// Both sides of the drain/delete race ended well (SME-293): the delete
/// and the delivery both succeeded, with no deadlock either side.
async fn assert_both_landed(
    pool: &PgPool,
    (root, child, row): (i64, i64, &db::AgentMessage),
    delete: tokio::task::JoinHandle<Result<(), String>>,
    drain: tokio::task::JoinHandle<Result<Option<crate::models::Message>, sqlx::Error>>,
) {
    let deleted = tokio::time::timeout(Duration::from_secs(10), delete).await.expect("the delete ends").expect("joined");
    let drained = tokio::time::timeout(Duration::from_secs(10), drain).await.expect("the drain ends").expect("joined");
    let drained = drained.map_err(|e| e.to_string());
    assert!(deleted.is_ok(), "the delete: {deleted:?}");
    assert!(matches!(drained, Ok(Some(_))), "the drain delivers M: {drained:?}");
    assert!(!exists(pool, child).await, "the child is gone");
    let (delivered, from) = sqlx::query_as::<_, (bool, Option<i64>)>(
        "SELECT delivered_at IS NOT NULL AND delivered_message_id IS NOT NULL, from_conversation_id FROM agent_messages WHERE id = $1",
    )
    .bind(row.id)
    .fetch_one(pool)
    .await
    .expect("M's row");
    assert!(delivered, "M is delivered and linked");
    assert_eq!(from, None, "M's sender is cleared by the delete's cascade");
    assert!(queued_to(pool, root).await.contains(&format!("the user deleted agent #{child}")), "the delete's notice is queued");
    eventually("the parent's turns are over", || async { !crate::turn::turn_running(root) }).await;
}

/// SME-293, the drain first: the root's drain holds M (marked delivered)
/// when the child's delete comes. The delete waits for the drain, which
/// then completes, with no deadlock.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_drain_holding_its_row_then_the_childs_delete(pool: PgPool) {
    let _race = crate::git::test_support::ONE_AT_A_TIME.lock().await;
    let (root, child, row) = child_with_a_queued_notice(&pool).await;
    let (delete_reached, delete_release) = test_hooks::pause(test_hooks::DELETE_MARKED, child);
    let delete = tokio::spawn({
        let pool = pool.clone();
        async move { delete_subtree(&pool, child).await }
    });
    tokio::time::timeout(Duration::from_secs(10), delete_reached).await.expect("the delete runs").expect("paused");
    let (drain_reached, drain_release) = test_hooks::pause(test_hooks::DELIVER_MARKED, root);
    let drain = deliver(&pool, &row);
    tokio::time::timeout(Duration::from_secs(10), drain_reached).await.expect("the drain runs").expect("paused");
    delete_release.send(()).expect("release the delete");
    assert!(
        crate::git::test_support::waiters_or_finished(&pool, 1, &delete).await,
        "the delete waits for the drain's rows"
    );
    drain_release.send(()).expect("release the drain");
    assert_both_landed(&pool, (root, child, &row), delete, drain).await;
}

/// SME-293, the delete first: the child's delete holds its rows when the
/// root's drain comes. The drain waits for the delete, then delivers M
/// from a sender that's now gone, with no deadlock.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_childs_delete_holding_its_rows_then_a_drain(pool: PgPool) {
    let _race = crate::git::test_support::ONE_AT_A_TIME.lock().await;
    let (root, child, row) = child_with_a_queued_notice(&pool).await;
    let (reached, release) = test_hooks::pause(test_hooks::DELETE_LOCKED, child);
    let delete = tokio::spawn({
        let pool = pool.clone();
        async move { delete_subtree(&pool, child).await }
    });
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the delete runs").expect("paused");
    let drain = deliver(&pool, &row);
    assert!(
        crate::git::test_support::waiters_or_finished(&pool, 1, &drain).await,
        "the drain waits for the delete's rows"
    );
    release.send(()).expect("release the delete");
    assert_both_landed(&pool, (root, child, &row), delete, drain).await;
}

/// Review 1, finding 2: two deletes of one child that both got past their
/// mark (two tabs, a double confirm): the second finds it gone and succeeds
/// without a notice, rather than failing on the notice's foreign key.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_second_delete_of_a_deleted_child_succeeds_quietly(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('c', $1) RETURNING id")
        .bind(root)
        .fetch_one(&pool)
        .await
        .expect("a child");
    let first = db::mark_subtree_deleting(&pool, child).await.expect("mark").expect("there");
    let second = db::mark_subtree_deleting(&pool, child).await.expect("mark").expect("there");
    db::delete_subtree_rows(&pool, child, &first, "agent").await.expect("the first delete");
    db::delete_subtree_rows(&pool, child, &second, "agent").await.expect("the second delete finds nothing to do");
    assert_eq!(queued_to(&pool, root).await, vec!["the user deleted agent".to_string()], "one notice");
}
