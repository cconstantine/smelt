//! `start_agent` and the start: a plain child runs its brief.

use super::*;

/// The provider and model `id` is set to.
async fn model_of(pool: &PgPool, id: i64) -> (Option<i64>, Option<String>) {
    let row = db::get_conversation_model(pool, id).await.expect("read the model").expect("the conversation");
    (row.provider_id, row.model)
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_start_agent_returns_at_once_and_the_child_runs_its_brief(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Brief done.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let (reached, release) = test_hooks::pause(test_hooks::BEFORE_START_DONE, root);

    let result =
        crate::anthropic::tools::execute(&pool, root, "toolu_s", "start_agent", &serde_json::json!({"brief": "Say hi."}))
            .await
            .expect("start_agent succeeds");
    let child = child_of(&pool, root, &result).await;
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the start runs").expect("paused");
    // Returned while the start is still going: the child exists, starting,
    // with its brief and nothing else.
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_STARTING));
    assert_eq!(db::conversation_parent(&pool, child).await.expect("parent"), Some(root));
    assert_eq!(transcript(&pool, child).await, vec![("user".to_string(), "Say hi.".to_string())]);
    assert_eq!(requests.lock().expect("log").len(), 0, "no turn while starting");

    release.send(()).expect("release the start");
    eventually("the child's first turn replies", || async {
        transcript(&pool, child).await.iter().any(|(role, _)| role == "assistant")
    })
    .await;
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_ACTIVE));
    let messages = transcript(&pool, child).await;
    assert_eq!(messages.len(), 3, "{messages:?}");
    assert_eq!(messages[0], ("user".to_string(), "Say hi.".to_string()));
    assert!(messages[1].1.starts_with("Notice: Your start is done"), "{messages:?}");
    assert_eq!(messages[2], ("assistant".to_string(), "Brief done.".to_string()));
    assert_eq!(requests.lock().expect("log").len(), 1, "the child's turn calls the model once");
    // Its system prompt says what it is.
    let system = requests.lock().expect("log")[0]["system"].to_string();
    assert!(system.contains(&format!("You are agent #{child}, started by conversation #{root}")), "{system}");
}

/// The child runs on its parent's model, not on the default.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_child_runs_on_its_parents_model(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    // The root keeps the first mock; a second one becomes the default
    // (and moves every conversation onto it, so the root is set back).
    let (first, _) = db::get_default_model(&pool).await.expect("read the default").expect("a default");
    mock_upstream(&pool, vec![text_reply("ok")]).await;
    db::set_conversation_model(&pool, root, first, crate::providers::test_support::MOCK_MODEL)
        .await
        .expect("set the root's model back");
    let (default, _) = db::get_default_model(&pool).await.expect("read the default").expect("a default");
    assert_ne!(default, first, "the second mock is the default");

    let child = start_child(&pool, root, "Work.").await;
    assert_eq!(model_of(&pool, child).await, (Some(first), Some(crate::providers::test_support::MOCK_MODEL.to_string())));
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_send_to_a_starting_child_is_refused(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let (reached, release) = test_hooks::pause(test_hooks::BEFORE_START_DONE, root);
    let child = start_child(&pool, root, "Work.").await;
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the start runs").expect("paused");

    let sent = crate::turn::start_turn(pool.clone(), child, "hello?".to_string()).await;
    let error = sent.expect_err("refused while starting");
    assert!(crate::api::chat::chat_error_text(&error).contains("This agent is still starting."), "{error:?}");
    assert!(!crate::turn::turn_running(child), "no turn was counted");

    release.send(()).expect("release the start");
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;
}

/// A conversation being deleted can't add an agent to its subtree.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_start_agent_from_a_conversation_being_deleted_is_refused(pool: PgPool) {
    setup(&pool, vec![text_reply("ok")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    sqlx::query("UPDATE conversations SET deleting_at = now() WHERE id = $1")
        .bind(root)
        .execute(&pool)
        .await
        .expect("mark it deleting");
    let result =
        crate::anthropic::tools::execute(&pool, root, "toolu_s", "start_agent", &serde_json::json!({"brief": "go"})).await;
    assert_eq!(result, Err("this conversation is being deleted".to_string()));
    let children = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM conversations WHERE parent_id = $1")
        .bind(root)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(children, 0);
}
