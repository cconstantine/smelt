//! A child whose turn ends without `finish` tells its parent.

use super::*;

/// Waits for `id`'s next `after_last_turn` check to be over, after `before`.
async fn checked_after(id: i64, before: usize) {
    eventually("the end-of-turn check ran", || async move { test_hooks::last_turns_checked(id) > before }).await;
}

async fn queued_to(pool: &PgPool, to: i64) -> Vec<String> {
    sqlx::query_scalar::<_, String>("SELECT body FROM agent_messages WHERE to_conversation_id = $1 ORDER BY id")
        .bind(to)
        .fetch_all(pool)
        .await
        .expect("queued rows")
}

/// A mock where each agent answers with `child` and the root with text.
async fn mock_child(pool: &PgPool, child: impl Fn() -> String + Send + Sync + 'static) {
    mock_with(pool, move |request| {
        let body = if agent_of(request).is_some() { child() } else { text_reply("Noted.") };
        Box::pin(async move { body })
    })
    .await;
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_childs_turn_ending_with_text_tells_its_parent(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let long = "x".repeat(450) + &"y".repeat(150);
    let reply = long.clone();
    mock_child(&pool, move || text_reply(&reply)).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = start_child(&pool, root, "Work.").await;

    eventually("the parent reads the notice", || async {
        transcript(&pool, root).await.iter().any(|(role, _)| role == "assistant")
    })
    .await;
    let expected = format!(
        "agent #{child}'s turn ended without a report; its last message: {}",
        long.chars().take(500).collect::<String>()
    );
    assert_eq!(queued_to(&pool, root).await, vec![expected.clone()]);
    assert_eq!(transcript(&pool, root).await[0], ("user".to_string(), format!("Notice: {expected}")));
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_failed_turns_notice_says_why(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    // An agent's call gets a stream that ends without a message: the turn fails.
    mock_child(&pool, || "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n".to_string()).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = start_child(&pool, root, "Work.").await;

    eventually("the parent is told", || async { !queued_to(&pool, root).await.is_empty() }).await;
    let queued = queued_to(&pool, root).await;
    assert!(
        queued[0].starts_with(&format!("agent #{child}'s turn ended without a report (it failed: ")),
        "{queued:?}"
    );
    assert!(queued[0].contains("Overloaded"), "{queued:?}");
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
}

/// No notice when the child asked the user, has a command running, called
/// finish, or is a conversation without a parent.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_no_notice_when_the_child_waits_on_something_or_finished(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    mock_with(&pool, |request| {
        let messages = request["messages"].to_string();
        let body = if messages.contains("Ask me.") {
            tool_call_reply("ask_user", serde_json::json!({"questions": [{"question": "Which?", "header": "Pick"}]}))
        } else if messages.contains("Finish now.") && !messages.contains("Your report was sent") {
            tool_call_reply("finish", serde_json::json!({"report": "Done."}))
        } else {
            text_reply("ok")
        };
        Box::pin(async move { body })
    })
    .await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let asks = start_child(&pool, root, "Ask me.").await;
    let finishes = start_child(&pool, root, "Finish now.").await;
    // The pause below is keyed by the parent, which all three share: taken
    // while the first two starts were still running, it could stop one of
    // them instead, and the pod row made below would then be seen by the
    // third start's credential step, which fails without a cluster, so its
    // turn never ran and the check below timed out (SME-324).
    eventually("the first two starts end", || async {
        test_hooks::starts_ended(asks) && test_hooks::starts_ended(finishes)
    })
    .await;
    // A command left running in this one before its first turn.
    let (reached, release) = test_hooks::pause(test_hooks::BEFORE_START_DONE, root);
    let commands = start_child(&pool, root, "Run things.").await;
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the start runs").expect("paused");
    let pod = db::create_sandbox_pod(&pool, commands).await.expect("a pod row");
    let terminal = db::create_sandbox_terminal(&pool, pod.id).await.expect("a terminal row");
    db::create_terminal_command(&pool, commands, terminal.id, "cmd-1", "sleep 100", 300)
        .await
        .expect("a running command");
    let before = test_hooks::last_turns_checked(commands);
    release.send(()).expect("release the start");

    for child in [asks, finishes] {
        eventually("its turn is over", || async {
            transcript(&pool, child).await.len() >= 3 && test_hooks::last_turns_checked(child) > 0
        })
        .await;
    }
    checked_after(commands, before).await;
    // The root's own turn, for the report, is checked too and tells no one.
    eventually("the root's turn is over", || async {
        transcript(&pool, root).await.iter().any(|(role, _)| role == "assistant") && test_hooks::last_turns_checked(root) > 0
    })
    .await;
    assert_eq!(
        queued_to(&pool, root).await,
        vec!["Done.".to_string()],
        "only the finished child's report: no notice for the others"
    );
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}

/// A turn the user stops tells the parent that, and nothing about a
/// missing report.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_no_ended_notice_after_the_users_stop(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
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
    user_stop(&pool, child).await.expect("the Stop goes through");
    eventually("the child's turn ends", || async { !crate::turn::turn_running(child) }).await;
    eventually("the parent reads the Stop's notice", || async {
        transcript(&pool, root).await.iter().any(|(role, _)| role == "assistant")
    })
    .await;
    let queued = queued_to(&pool, root).await;
    assert_eq!(queued.len(), 1, "{queued:?}");
    assert!(queued[0].starts_with(&format!("the user stopped agent #{child}")), "{queued:?}");
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}
