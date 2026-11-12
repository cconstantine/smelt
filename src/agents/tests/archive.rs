//! SME-480 T10 (D3): the user's send to any conversation in an archived
//! tree unarchives it; a delivered notice and its wake don't.

use super::*;

async fn archived_at(pool: &PgPool, id: i64) -> Option<chrono::NaiveDateTime> {
    sqlx::query_scalar::<_, Option<chrono::NaiveDateTime>>("SELECT archived_at FROM conversations WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("the conversation's row")
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_users_send_unarchives_the_tree_and_a_wake_does_not(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = active_child(&pool, root).await;
    db::set_archived(&pool, root, true).await.expect("archive the root");

    // A notice delivered to the agent, and the wake that runs its turn:
    // not the user, so the tree stays archived.
    let mut conn = pool.acquire().await.expect("a connection");
    db::queue_agent_message(&mut conn, child, None, "smelt", db::KIND_NOTICE, "For you.").await.expect("a notice");
    drop(conn);
    crate::turn::notify(&pool, child, vec![crate::turn::Notice::Wake]).await.expect("the wake");
    eventually("the wake's turn ran and ended", || async {
        requests_for(&requests, Some(child)).len() == 1 && !crate::turn::turn_running(child)
    })
    .await;
    assert!(archived_at(&pool, root).await.is_some(), "a wake unarchived the tree");

    // The user writing to the agent: its root is in use again.
    crate::turn::start_turn(pool.clone(), child, "Again.".to_string()).await.expect("the send");
    assert_eq!(archived_at(&pool, root).await, None, "the send unarchives the root before it returns");
    eventually("the turn is over", || async { !crate::turn::turn_running(child) }).await;
    for id in [root, child] {
        crate::turn::forget_conversation_lock(id);
    }
}
