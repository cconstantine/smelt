//! A failed start, the start's exit, and the startup sweep.

use super::*;

async fn queued_to(pool: &PgPool, to: i64) -> Vec<(String, Option<chrono::NaiveDateTime>)> {
    sqlx::query_as::<_, (String, Option<chrono::NaiveDateTime>)>(
        "SELECT body, delivered_at FROM agent_messages WHERE to_conversation_id = $1 ORDER BY id",
    )
    .bind(to)
    .fetch_all(pool)
    .await
    .expect("queued rows")
}

/// A child row as a start a restart cut off leaves it: `starting`, with a
/// trust card its AGENTS.md load made.
async fn cut_off_child(pool: &PgPool, parent: i64) -> i64 {
    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('c', $1) RETURNING id")
        .bind(parent)
        .fetch_one(pool)
        .await
        .expect("a child");
    sqlx::query("INSERT INTO agents (conversation_id, root_id, depth, brief, state) VALUES ($1, $2, 1, 'b', 'starting')")
        .bind(child)
        .bind(parent)
        .execute(pool)
        .await
        .expect("its agent row");
    card(pool, child).await;
    child
}

/// An AGENTS.md trust card in `conversation_id`.
async fn card(pool: &PgPool, conversation_id: i64) {
    let repo = db::create_conversation_repo(pool, conversation_id, "https://example.com/r.git", "example.com/r", None, "r")
        .await
        .expect("a repo row");
    let file = db::InstructionsFile { content: "Rules.".to_string(), file_bytes: 6, hash: "h".to_string(), commit: None };
    db::request_instruction(pool, conversation_id, repo.id, "AGENTS.md", &file).await.expect("a card");
}

async fn cards(pool: &PgPool, conversation_id: i64) -> usize {
    db::list_instruction_requests(pool, conversation_id).await.expect("cards").len()
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_sweep_fails_a_start_a_restart_cut_off(pool: PgPool) {
    setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = cut_off_child(&pool, root).await;

    for wake in startup_sweep(&pool).await {
        wake.await.expect("the wake");
    }
    let text = format!("agent #{child}'s start failed: smelt restarted during the start");
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_FAILED));
    assert_eq!(cards(&pool, child).await, 0, "the cut-off start's card is deleted");
    assert_eq!(transcript(&pool, child).await, vec![("user".to_string(), text.clone())], "the child says why");
    assert_eq!(
        transcript(&pool, root).await,
        vec![
            ("user".to_string(), format!("Report from agent #{child}:\n\n{text}")),
            ("assistant".to_string(), "Noted.".to_string()),
        ],
        "the parent gets the error as the report, and is woken"
    );
}

/// A report queued before a restart: turn state is gone, the sweep wakes
/// its parent, and a `stopped` recipient's row waits.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_queued_report_survives_a_restart_and_a_stopped_recipient_is_not_woken(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let stopped = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('s', $1) RETURNING id")
        .bind(root)
        .fetch_one(&pool)
        .await
        .expect("a child");
    sqlx::query("INSERT INTO agents (conversation_id, root_id, depth, brief, state, started_at) VALUES ($1, $2, 1, 'b', 'stopped', now())")
        .bind(stopped)
        .bind(root)
        .execute(&pool)
        .await
        .expect("its agent row");
    let mut conn = pool.acquire().await.expect("a connection");
    db::queue_agent_message(&mut conn, root, Some(stopped), &format!("agent #{stopped}"), db::KIND_REPORT, "Before.")
        .await
        .expect("a report");
    db::queue_agent_message(&mut conn, stopped, None, "smelt", db::KIND_NOTICE, "For you.").await.expect("a notice");
    drop(conn);
    // What a restart forgets.
    crate::turn::forget_conversation_lock(root);
    crate::turn::forget_conversation_lock(stopped);

    for wake in startup_sweep(&pool).await {
        wake.await.expect("the wake");
    }
    assert_eq!(
        transcript(&pool, root).await,
        vec![
            ("user".to_string(), format!("Report from agent #{stopped}:\n\nBefore.")),
            ("assistant".to_string(), "Noted.".to_string()),
        ]
    );
    assert_eq!(requests_for(&requests, Some(stopped)).len(), 0, "the stopped child isn't woken");
    let left = queued_to(&pool, stopped).await;
    assert!(left.len() == 1 && left[0].1.is_none(), "its notice waits: {left:?}");
}

/// The start's exit runs however the start ends: done, failed, or with
/// the child deleted under it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_starts_exit_runs_on_done_on_failure_and_on_a_delete(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;

    let done = start_child(&pool, root, "Done.").await;
    eventually("the done start's exit ran", || async { test_hooks::starts_ended(done) }).await;
    assert_eq!(state(&pool, done).await.as_deref(), Some(db::AGENT_ACTIVE));

    test_hooks::fail_next_start(root, "the clone failed: no such repo");
    let failed = start_child(&pool, root, "Fails.").await;
    eventually("the failed start's exit ran", || async { test_hooks::starts_ended(failed) }).await;
    assert_eq!(state(&pool, failed).await.as_deref(), Some(db::AGENT_FAILED));
    let text = format!("agent #{failed}'s start failed: the clone failed: no such repo");
    assert!(transcript(&pool, failed).await.contains(&("user".to_string(), text.clone())));
    eventually("the parent got the report", || async {
        transcript(&pool, root).await.iter().any(|(_, t)| *t == format!("Report from agent #{failed}:\n\n{text}"))
    })
    .await;

    let (reached, release) = test_hooks::pause(test_hooks::BEFORE_START_DONE, root);
    let deleted = start_child(&pool, root, "Deleted.").await;
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the start runs").expect("paused");
    card(&pool, deleted).await;
    db::delete_conversation(&pool, deleted).await.expect("delete the child");
    release.send(()).expect("release the start");
    eventually("the deleted child's start exit ran", || async { test_hooks::starts_ended(deleted) }).await;
    assert!(!db::conversation_exists(&pool, deleted).await.expect("exists"), "nothing brought it back");
    eventually("every turn is over", || async {
        !crate::turn::turn_running(root) && !crate::turn::turn_running(done)
    })
    .await;
}

/// The exit deletes a trust card the start's AGENTS.md load made, when a
/// stop lands before the start's done step.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stopped_starts_card_is_deleted(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let (reached, release) = test_hooks::pause(test_hooks::BEFORE_START_DONE, root);
    let child = start_child(&pool, root, "Work.").await;
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the start runs").expect("paused");
    card(&pool, child).await;
    // A stop lands first: the done step finds the child no longer
    // starting, so the start ends through its exit only.
    sqlx::query("UPDATE agents SET state = 'stopped' WHERE conversation_id = $1")
        .bind(child)
        .execute(&pool)
        .await
        .expect("stop it");
    release.send(()).expect("release the start");
    eventually("the start's exit ran", || async { test_hooks::starts_ended(child) }).await;
    assert_eq!(cards(&pool, child).await, 0);
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}
