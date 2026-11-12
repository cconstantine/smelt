//! SME-480 T9: the endpoints' answers, through `archive_in`.

use std::time::Duration;

use super::*;
use crate::{db, events};

/// Whether a `ConversationsChanged` arrives within a few seconds, skipping
/// the app events other tests publish alongside (as `api::chat`'s tests do).
async fn received_list_change(app: &mut tokio::sync::broadcast::Receiver<events::AppEvent>) -> bool {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match app.recv().await {
                Ok(events::AppEvent::ConversationsChanged) => return true,
                Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

fn code_and_message(e: &ServerFnError) -> (u16, String) {
    match e {
        ServerFnError::ServerError { message, code, .. } => (*code, message.clone()),
        other => panic!("a server error with a code, not {other:?}"),
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_archive_answers_200_404_and_409_and_announces_a_change(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let root = db::create_conversation(&pool).await.expect("a root").id;

    let mut app = events::subscribe_app();
    let archived = archive_in(&pool, root, true).await.expect("archive a root");
    assert_eq!(archived.id, root);
    assert!(archived.archived_at.is_some(), "the returned conversation shows the mark");
    assert!(received_list_change(&mut app).await, "archiving announced nothing");

    let unarchived = archive_in(&pool, root, false).await.expect("unarchive a root");
    assert_eq!(unarchived.archived_at, None);

    let missing = archive_in(&pool, 987_654_321, true).await.expect_err("a missing id");
    assert_eq!(code_and_message(&missing), (404, "conversation not found".to_string()));

    let child = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('c', $1) RETURNING id")
        .bind(root)
        .fetch_one(&pool)
        .await
        .expect("a child");
    sqlx::query("INSERT INTO agents (conversation_id, root_id, depth, brief, state) VALUES ($1, $2, 1, 'b', 'active')")
        .bind(child)
        .bind(root)
        .execute(&pool)
        .await
        .expect("its agent row");
    for archived in [true, false] {
        let refused = archive_in(&pool, child, archived).await.expect_err("an agent");
        assert_eq!(
            code_and_message(&refused),
            (409, format!("only a top-level conversation is archived: archive its root, conversation {root}"))
        );
    }
}
