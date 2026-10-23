//! Restart notices (SME-238): a child whose turn a restart cut off is told,
//! and woken, to carry on.

use super::*;

const NOTICE: &str = "Notice: smelt restarted during your turn; carry on";

/// Saves `blocks` as a `role` message in `id`.
async fn say(pool: &PgPool, id: i64, role: &str, blocks: Vec<ContentBlock>) {
    db::create_message(pool, id, role, &blocks).await.expect("save a message");
}

/// A turn cut off after a tool ran: the user's message, the model's call,
/// and the call's result, with no reply after it.
async fn cut_off_turn(pool: &PgPool, id: i64) {
    say(pool, id, "user", vec![ContentBlock::Text { text: "Work.".to_string() }]).await;
    say(
        pool,
        id,
        "assistant",
        vec![ContentBlock::ToolUse { id: "toolu_l".to_string(), name: "list_agents".to_string(), input: serde_json::json!({}) }],
    )
    .await;
    say(
        pool,
        id,
        "user",
        vec![ContentBlock::ToolResult { tool_use_id: "toolu_l".to_string(), content: "{\"agents\":[]}".to_string(), is_error: None }],
    )
    .await;
}

async fn restart_notices(pool: &PgPool, id: i64) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1 AND body = $2")
        .bind(id)
        .bind(RESTARTED_MID_TURN)
        .fetch_one(pool)
        .await
        .expect("count")
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_child_cut_off_mid_turn_gets_the_restart_notice_and_carries_on(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Carrying on.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = active_child(&pool, root).await;
    cut_off_turn(&pool, child).await;

    restart(&pool, &[root, child]).await;
    let calls = requests_for(&requests, Some(child));
    assert_eq!(calls.len(), 1, "the child wasn't woken");
    let messages = calls[0]["messages"].as_array().expect("messages");
    let last = messages.last().expect("a message").to_string();
    assert!(last.contains(NOTICE), "{last}");
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
}

/// Not for a root, a paused child, a child waiting for its parent, a
/// finished or stopped one, nor one whose last message is the model's.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_restart_notice_is_only_for_an_active_child_cut_off_mid_turn(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Carrying on.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    cut_off_turn(&pool, root).await;
    let paused = active_child(&pool, root).await;
    cut_off_turn(&pool, paused).await;
    crate::turn::stop_conversation(&pool, paused).await;
    let asking = active_child(&pool, root).await;
    cut_off_turn(&pool, asking).await;
    sqlx::query("INSERT INTO pending_questions (conversation_id, tool_use_id, questions, asked_of) VALUES ($1, 'toolu_p', '[]', 'parent')")
        .bind(asking)
        .execute(&pool)
        .await
        .expect("a question for its parent");
    let finished = active_child(&pool, root).await;
    cut_off_turn(&pool, finished).await;
    let stopped = active_child(&pool, root).await;
    cut_off_turn(&pool, stopped).await;
    for (id, state) in [(finished, db::AGENT_FINISHED), (stopped, db::AGENT_STOPPED)] {
        sqlx::query("UPDATE agents SET state = $2 WHERE conversation_id = $1")
            .bind(id)
            .bind(state)
            .execute(&pool)
            .await
            .expect("set the state");
    }
    let replied = active_child(&pool, root).await;
    say(&pool, replied, "user", vec![ContentBlock::Text { text: "Work.".to_string() }]).await;
    say(&pool, replied, "assistant", vec![ContentBlock::Text { text: "Done.".to_string() }]).await;

    let ids = [root, paused, asking, finished, stopped, replied];
    restart(&pool, &ids).await;
    for id in ids {
        assert_eq!(restart_notices(&pool, id).await, 0, "#{id} was told");
    }
    assert_eq!(requests.lock().expect("log").len(), 0, "something was woken");
    for id in ids {
        crate::turn::forget_conversation_lock(id);
    }
}

/// The notice waits undelivered (its wake not yet run) when smelt
/// restarts again: the second restart adds none.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_two_restarts_in_a_row_queue_one_notice(pool: PgPool) {
    setup(&pool, vec![text_reply("Carrying on.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = active_child(&pool, root).await;
    cut_off_turn(&pool, child).await;

    notify_cut_off_turns(&pool).await;
    notify_cut_off_turns(&pool).await;
    assert_eq!(restart_notices(&pool, child).await, 1);
    restart(&pool, &[root, child]).await;
    assert_eq!(restart_notices(&pool, child).await, 1);
    let told = transcript(&pool, child).await.into_iter().filter(|(_, text)| text == NOTICE).count();
    assert_eq!(told, 1);
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
}
