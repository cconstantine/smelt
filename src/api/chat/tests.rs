use super::*;
use crate::git;
use std::time::Duration;

/// Polls `check` until it holds, for up to 10 s.
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !check().await {
        assert!(std::time::Instant::now() < deadline, "{what}: not within 10 s");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// `delete_and_forget` for `id` as a request of its own, which the
/// test can drop (`abort`), as when the tab closes mid-way.
fn spawn_delete(pool: &PgPool, id: i64) -> tokio::task::JoinHandle<Result<(), String>> {
    let pool = pool.clone();
    tokio::spawn(async move { delete_and_forget(&pool, id).await })
}

/// Whether a `ConversationsChanged` arrives within a few seconds,
/// skipping the app events other tests publish alongside (SME-135).
async fn received_list_change(
    app: &mut tokio::sync::broadcast::Receiver<events::AppEvent>,
) -> bool {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match app.recv().await {
                Ok(events::AppEvent::ConversationsChanged) => return true,
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// SME-269, the ticket's case: a conversation created here reaches every
/// open tab. `create_conversation_in` is the only writer of a new row, so
/// the announcement goes out once that row exists and a listener that
/// refetches on hearing it finds the row there.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_creating_a_conversation_publishes_a_list_change(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let mut app = events::subscribe_app();
    let created = create_conversation_in(&pool).await.expect("create conversation");
    assert!(received_list_change(&mut app).await, "creating a conversation announced nothing");
    let listed = db::list_conversations(&pool).await.expect("list conversations");
    assert!(listed.iter().any(|c| c.id == created.id), "the announced row isn't listed");
}

/// SME-269. The delete announces itself after the row is gone, as its
/// `PodsChanged` already does, so a tab that refetches sees the row
/// missing rather than still there.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_deleting_a_conversation_publishes_a_list_change(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    // Teardown stops at `get()`: this delete touches no cluster.
    crate::sandbox::test_support::use_unreachable_test_manager();
    let id = db::create_conversation(&pool).await.expect("conversation").id;
    let mut app = events::subscribe_app();
    delete_and_forget(&pool, id).await.expect("delete conversation");
    assert!(received_list_change(&mut app).await, "deleting a conversation announced nothing");
    assert!(!db::conversation_exists(&pool, id).await.expect("exists"), "announced before it was deleted");
}

/// SME-269, the ticket's case for a tab that has a conversation open:
/// a conversation created, retitled or deleted elsewhere reaches it on
/// the stream it already holds. Built from an app channel of the test's
/// own — the app-wide bus carries every other test's events too, so a
/// first item here would be theirs to judge, not this test's (SME-135).
#[tokio::test]
async fn test_a_list_change_reaches_every_watched_conversation() {
    use futures_util::StreamExt;
    let conversation_id = 9_269_000_001;
    let (app, app_rx) = tokio::sync::broadcast::channel(16);
    let stream = conversation_relay(conversation_id, events::subscribe(conversation_id), app_rx);
    futures_util::pin_mut!(stream);
    app.send(events::AppEvent::ConversationsChanged).expect("publish");
    let event = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the relayed list change never arrived");
    assert!(
        matches!(event, Some(Ok(events::ConversationEvent::ConversationsChanged {}))),
        "got {event:?}"
    );
    events::forget(conversation_id);
}

/// SME-262, the ticket's case. Interleaving: the delete commits, and
/// teardown's read of this database's instance is held by a table
/// lock; the request is dropped there. The conversation's turn state
/// and event channel are still forgotten, so an open tab's stream ends.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_delete_dropped_mid_teardown_still_forgets_the_conversation(pool: PgPool) {
    let _race = git::test_support::ONE_AT_A_TIME.lock().await;
    let _turns = crate::providers::test_support::lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    // Past `get()`, so teardown reads the instance; its cluster calls
    // then fail at once.
    crate::sandbox::test_support::use_unreachable_test_manager();
    let id = db::create_conversation(&pool).await.expect("conversation").id;
    let mut events = crate::events::subscribe(id);

    let mut holding = pool.begin().await.expect("begin the holding transaction");
    sqlx::query("LOCK TABLE smelt_instance IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *holding)
        .await
        .expect("hold smelt_instance");
    let request = spawn_delete(&pool, id);
    assert!(git::test_support::waiters_or_finished(&pool, 1, &request).await, "teardown never read the instance");
    assert!(!db::conversation_exists(&pool, id).await.expect("exists"), "dropped before the delete committed");
    request.abort();
    let _ = request.await;
    holding.rollback().await.expect("release smelt_instance");

    eventually("the turn state is forgotten", || async move { !crate::turn::has_turn_state(id) }).await;
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Err(tokio::sync::broadcast::error::RecvError::Closed) = events.recv().await {
                break;
            }
        }
    })
    .await;
    assert!(closed.is_ok(), "the conversation's event stream was never closed");
}

/// SME-262. Interleaving: the conversation's row is held `FOR UPDATE`,
/// so the DELETE waits; the request is dropped there. The delete
/// still commits once the row is released, and its turn state is
/// forgotten. (No test manager: teardown stops at `get()`.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_delete_dropped_mid_delete_still_deletes_and_forgets(pool: PgPool) {
    let _race = git::test_support::ONE_AT_A_TIME.lock().await;
    let _turns = crate::providers::test_support::lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let id = db::create_conversation(&pool).await.expect("conversation").id;

    let mut holding = pool.begin().await.expect("begin the holding transaction");
    sqlx::query("SELECT 1 FROM conversations WHERE id = $1 FOR UPDATE")
        .bind(id)
        .execute(&mut *holding)
        .await
        .expect("hold the conversation's row");
    let request = spawn_delete(&pool, id);
    assert!(git::test_support::waiters_or_finished(&pool, 1, &request).await, "the delete never waited for the row");
    request.abort();
    let _ = request.await;
    holding.rollback().await.expect("release the row");

    let p = &pool;
    eventually("the conversation is deleted", || async move {
        !db::conversation_exists(p, id).await.expect("exists")
    })
    .await;
    eventually("the turn state is forgotten", || async move { !crate::turn::has_turn_state(id) }).await;
}

/// SME-262 review 1: the delete's task runs in its request's span, so
/// its teardown is the request's child in a trace, as before the task.
/// (No test manager: teardown stops at `get()`, inside its span.)
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_deletes_teardown_is_a_child_of_its_request(pool: PgPool) {
    let _race = git::test_support::ONE_AT_A_TIME.lock().await;
    let _turns = crate::providers::test_support::lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let id = db::create_conversation(&pool).await.expect("conversation").id;

    // The span is made inside the capture, which only then listens.
    let (deleted, spans) = crate::telemetry::capture_spans(async {
        crate::telemetry::in_span(tracing::info_span!("request"), delete_and_forget(&pool, id)).await
    })
    .await;
    deleted.expect("delete");
    let request = spans.iter().find(|span| span.name == "request").expect("the request's span");
    let teardown = spans.iter().find(|span| span.name == "teardown_conversation").expect("teardown's span");
    assert_eq!(teardown.parent_span_id, request.span_context.span_id(), "{spans:?}");
}
