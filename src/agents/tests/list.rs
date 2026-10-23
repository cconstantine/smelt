//! `list_agents`: a conversation's own children, with their status.

use super::*;

async fn list(pool: &PgPool, caller: i64) -> Vec<serde_json::Value> {
    let listed = crate::anthropic::tools::execute(pool, caller, "toolu_l", "list_agents", &serde_json::json!({}))
        .await
        .expect("list_agents succeeds");
    let parsed: serde_json::Value = serde_json::from_str(&listed).expect("JSON");
    parsed["agents"].as_array().expect("an agents array").clone()
}

async fn set_state(pool: &PgPool, id: i64, state: &str) {
    sqlx::query("UPDATE agents SET state = $2 WHERE conversation_id = $1")
        .bind(id)
        .bind(state)
        .execute(pool)
        .await
        .expect("set the state");
}

/// Each status, from a child put in it; and only the caller's children.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_list_agents_shows_only_the_callers_children_with_their_status(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let gate = Arc::new(tokio::sync::Notify::new());
    // Once set, every agent's model call is held until the gate opens.
    let hold = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let gate = gate.clone();
        let hold = hold.clone();
        mock_with(&pool, move |request| {
            let held = agent_of(request).is_some() && hold.load(std::sync::atomic::Ordering::SeqCst);
            let gate = gate.clone();
            Box::pin(async move {
                if held {
                    gate.notified().await;
                }
                text_reply("ok")
            })
        })
        .await;
    }
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let other_root = db::create_conversation(&pool).await.expect("create conversation").id;
    let others = start_child(&pool, other_root, "Not yours.").await;

    let idle = start_child(&pool, root, "Idle.").await;
    let finished = start_child(&pool, root, "Finished.").await;
    let stopped = start_child(&pool, root, "Stopped.").await;
    let failed = start_child(&pool, root, "Failed.").await;
    let waiting = start_child(&pool, root, "Waiting.").await;
    for child in [others, idle, finished, stopped, failed, waiting] {
        eventually("the child's first turn is over", || async {
            state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) && !crate::turn::turn_running(child)
                && transcript(&pool, child).await.iter().any(|(role, _)| role == "assistant")
        })
        .await;
    }
    set_state(&pool, finished, db::AGENT_FINISHED).await;
    set_state(&pool, stopped, db::AGENT_STOPPED).await;
    set_state(&pool, failed, db::AGENT_FAILED).await;
    sqlx::query("INSERT INTO pending_questions (conversation_id, tool_use_id, questions) VALUES ($1, 'toolu_q', '[]')")
        .bind(waiting)
        .execute(&pool)
        .await
        .expect("a question for the user");
    // A grandchild row, made directly: depth 1 can't make one through the tool.
    sqlx::query("INSERT INTO conversations (title, parent_id) VALUES ('grandchild', $1)")
        .bind(idle)
        .execute(&pool)
        .await
        .expect("a grandchild");

    // One still starting, and one running (its model call held).
    let (reached, release) = test_hooks::pause(test_hooks::BEFORE_START_DONE, root);
    let starting = start_child(&pool, root, "Starting.").await;
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the start runs").expect("paused");
    hold.store(true, std::sync::atomic::Ordering::SeqCst);
    let running = start_child(&pool, root, "Running.").await;
    eventually("the running child's turn is in flight", || async { crate::turn::turn_running(running) }).await;

    let listed = list(&pool, root).await;
    let status_of = |id: i64| {
        listed
            .iter()
            .find(|agent| agent["id"] == id)
            .map(|agent| agent["status"].as_str().expect("a status").to_string())
    };
    assert_eq!(status_of(others), None, "another root's child is never listed");
    assert_eq!(status_of(starting).as_deref(), Some("starting"));
    assert_eq!(status_of(running).as_deref(), Some("running"));
    assert_eq!(status_of(waiting).as_deref(), Some("waiting for the user"));
    assert_eq!(status_of(finished).as_deref(), Some("finished"));
    assert_eq!(status_of(idle).as_deref(), Some("idle"));
    assert_eq!(status_of(stopped).as_deref(), Some("stopped"));
    assert_eq!(status_of(failed).as_deref(), Some("failed"));
    let ids: Vec<i64> = listed.iter().map(|agent| agent["id"].as_i64().expect("an id")).collect();
    let mut newest_first = ids.clone();
    newest_first.sort_by(|a, b| b.cmp(a));
    assert_eq!(ids, newest_first, "newest first");
    let idle_row = listed.iter().find(|agent| agent["id"] == idle).expect("idle listed");
    assert_eq!(idle_row["agents_started"], 1);
    assert_eq!(idle_row["title"], "Idle.");
    assert!(idle_row["persona"].is_null());
    assert!(idle_row["last_message_at"].is_string(), "{idle_row}");

    gate.notify_one();
    release.send(()).expect("release the start");
    eventually("every turn is over", || async {
        !crate::turn::turn_running(running) && state(&pool, starting).await.as_deref() == Some(db::AGENT_ACTIVE)
    })
    .await;
}

#[test]
fn test_status_takes_the_first_that_holds() {
    assert_eq!(status(db::AGENT_STOPPED, true, true), "stopped", "a stored end wins over a turn in flight");
    assert_eq!(status(db::AGENT_FINISHED, true, false), "running", "a finished child the user wrote to runs");
    assert_eq!(status(db::AGENT_FINISHED, false, true), "waiting for the user");
    assert_eq!(status(db::AGENT_ACTIVE, false, false), "idle");
}

#[test]
fn test_persona_label_is_dir_and_name() {
    assert_eq!(persona_label("/workspace/smelt/.agents/personas/developer.md").as_deref(), Some("smelt:developer"));
    assert_eq!(persona_label("/elsewhere/developer.md"), None);
}
