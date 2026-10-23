//! `stop_agent`, the user's Stop, and who may start a turn in a child.

use super::*;

async fn stop(pool: &PgPool, caller: i64, id: i64) -> Result<String, String> {
    crate::anthropic::tools::execute(pool, caller, "toolu_x", "stop_agent", &serde_json::json!({"id": id})).await
}

async fn queued_to(pool: &PgPool, to: i64) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT body FROM agent_messages WHERE to_conversation_id = $1 ORDER BY id")
        .bind(to)
        .fetch_all(pool)
        .await
        .expect("queued rows")
}

async fn set_state(pool: &PgPool, id: i64, state: &str) {
    sqlx::query("UPDATE agents SET state = $2 WHERE conversation_id = $1")
        .bind(id)
        .bind(state)
        .execute(pool)
        .await
        .expect("set the state");
}

/// A child whose first turn is over.
async fn settled_child(pool: &PgPool, root: i64, brief: &str) -> i64 {
    let child = start_child(pool, root, brief).await;
    eventually("the child's first turn is over", || async {
        state(pool, child).await.as_deref() == Some(db::AGENT_ACTIVE)
            && transcript(pool, child).await.iter().any(|(role, _)| role == "assistant")
            && !crate::turn::turn_running(child)
    })
    .await;
    child
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_stop_agent_refuses_another_roots_child_and_any_other_id(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let other = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = settled_child(&pool, root, "Work.").await;

    assert_eq!(stop(&pool, other, child).await, Err(format!("#{child} isn't one of your agents (list_agents lists them)")));
    assert_eq!(stop(&pool, root, other).await, Err(format!("#{other} isn't one of your agents (list_agents lists them)")));
    assert_eq!(stop(&pool, root, root).await, Err(format!("#{root} isn't one of your agents (list_agents lists them)")));
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_ACTIVE), "nothing was stopped");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_stop_agent_stops_the_subtree_and_ends_a_running_turn(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    // Every agent's model call hangs: the child's turn stays in flight.
    mock_with(&pool, |request| {
        let hang = agent_of(request).is_some();
        Box::pin(async move {
            if hang {
                std::future::pending::<()>().await;
            }
            text_reply("ok")
        })
    })
    .await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = start_child(&pool, root, "Work.").await;
    eventually("the child's turn is in flight", || async { crate::turn::turn_running(child) }).await;
    // A grandchild, made directly: depth 1 can't make one through the tool.
    let grandchild = sqlx::query_scalar::<_, i64>("INSERT INTO conversations (title, parent_id) VALUES ('g', $1) RETURNING id")
        .bind(child)
        .fetch_one(&pool)
        .await
        .expect("a grandchild");
    sqlx::query("INSERT INTO agents (conversation_id, root_id, depth, brief, state, started_at) VALUES ($1, $2, 2, 'g', 'active', now())")
        .bind(grandchild)
        .bind(root)
        .execute(&pool)
        .await
        .expect("its agent row");

    assert_eq!(stop(&pool, root, child).await, Ok(format!("Stopped agent #{child} and 1 agent under it.")));
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_STOPPED));
    assert_eq!(state(&pool, grandchild).await.as_deref(), Some(db::AGENT_STOPPED));
    eventually("the child's turn ends", || async { !crate::turn::turn_running(child) }).await;
    assert_eq!(stop(&pool, root, child).await, Ok(format!("agent #{child} was already stopped")));
}

/// Stopped while its start is held: the start's done step finds it no
/// longer starting, so it never becomes active, and the user's send in it
/// is refused.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_child_stopped_during_its_start_never_becomes_active(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let (reached, release) = test_hooks::pause(test_hooks::BEFORE_START_DONE, root);
    let child = start_child(&pool, root, "Work.").await;
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the start runs").expect("paused");

    assert_eq!(stop(&pool, root, child).await, Ok(format!("Stopped agent #{child}.")));
    release.send(()).expect("release the start");
    eventually("the start is over", || async { test_hooks::starts_ended(child) }).await;
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_STOPPED));
    assert!(queued_to(&pool, child).await.is_empty(), "no start notice for a stopped child");
    let sent = crate::turn::start_turn(pool.clone(), child, "hello?".to_string()).await;
    let error = sent.expect_err("refused");
    assert!(
        crate::api::chat::chat_error_text(&error).contains("This agent was stopped while it was starting; start a new one."),
        "{error:?}"
    );
    assert_eq!(requests.lock().expect("log").len(), 0, "no turn ever ran");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_users_send_makes_a_finished_or_stopped_child_active_and_a_failed_one_refuses(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let finished = settled_child(&pool, root, "One.").await;
    let stopped = settled_child(&pool, root, "Two.").await;
    let failed = settled_child(&pool, root, "Three.").await;
    set_state(&pool, finished, db::AGENT_FINISHED).await;
    stop(&pool, root, stopped).await.expect("stop it");
    set_state(&pool, failed, db::AGENT_FAILED).await;

    for child in [finished, stopped] {
        crate::turn::start_turn(pool.clone(), child, "Carry on.".to_string()).await.expect("the send goes through");
        assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_ACTIVE));
        eventually("its turn is over", || async {
            transcript(&pool, child).await.iter().filter(|(role, _)| role == "assistant").count() == 2
                && !crate::turn::turn_running(child)
        })
        .await;
    }
    let error = crate::turn::start_turn(pool.clone(), failed, "Carry on.".to_string()).await.expect_err("refused");
    assert!(crate::api::chat::chat_error_text(&error).contains("This agent's start failed; start a new one."), "{error:?}");
    assert_eq!(state(&pool, failed).await.as_deref(), Some(db::AGENT_FAILED));
}

/// The user's Stop: refused while starting; on a working child it tells
/// the parent once, and a second Stop adds nothing.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_users_stop_on_a_child_tells_its_parent_once(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = settled_child(&pool, root, "Work.").await;
    let (reached, release) = test_hooks::pause(test_hooks::BEFORE_START_DONE, root);
    let starting = start_child(&pool, root, "Later.").await;
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the start runs").expect("paused");

    assert_eq!(
        user_stop(&pool, starting).await,
        Err("This agent is still starting: stop it with stop_agent from its parent, or delete it.".to_string())
    );
    assert!(!crate::turn::is_paused(starting), "the refused Stop paused nothing");

    user_stop(&pool, child).await.expect("the Stop goes through");
    user_stop(&pool, child).await.expect("a second Stop goes through");
    let notice = format!("the user stopped agent #{child}; it carries on if the user writes to it, or call stop_agent to end it");
    let stopped: Vec<String> =
        queued_to(&pool, root).await.into_iter().filter(|body| body.starts_with("the user stopped")).collect();
    assert_eq!(stopped, vec![notice.clone()]);
    eventually("the parent reads the notice", || async {
        transcript(&pool, root).await.iter().any(|(_, text)| *text == format!("Notice: {notice}"))
    })
    .await;

    release.send(()).expect("release the start");
    eventually("every turn is over", || async {
        state(&pool, starting).await.as_deref() == Some(db::AGENT_ACTIVE)
            && !crate::turn::turn_running(starting)
            && !crate::turn::turn_running(root)
    })
    .await;
}

/// A wake or a notice to a child still starting runs no turn: what's
/// queued waits for its first turn.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_or_notice_to_a_starting_child_runs_no_turn(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let (reached, release) = test_hooks::pause(test_hooks::BEFORE_START_DONE, root);
    let child = start_child(&pool, root, "Work.").await;
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the start runs").expect("paused");

    db::queue_notice(&pool, child, root, "smelt", "early").await.expect("queue a notice");
    crate::turn::notify(&pool, child, vec![crate::turn::Notice::Wake, crate::turn::Notice::Deliver("Docker restarted.".to_string())])
        .await
        .expect("the notices are handled");
    assert_eq!(requests.lock().expect("log").len(), 0, "no turn while starting");
    let texts: Vec<String> = transcript(&pool, child).await.into_iter().map(|(_, text)| text).collect();
    assert_eq!(texts, vec!["Work.".to_string(), "Docker restarted.".to_string()], "the notice is saved, the row still queued");

    release.send(()).expect("release the start");
    eventually("its first turn drains the queued notice", || async {
        transcript(&pool, child).await.iter().any(|(role, _)| role == "assistant")
    })
    .await;
    let texts: Vec<String> = transcript(&pool, child).await.into_iter().map(|(_, text)| text).collect();
    assert!(texts.contains(&"Notice: early".to_string()), "{texts:?}");
    eventually("its turn is over", || async { !crate::turn::turn_running(child) }).await;
}
