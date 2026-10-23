//! `finish`: a child's report reaches its parent.

use super::*;

/// Each child answers its first call with `finish(report)` and any later
/// one with text; the root answers with `root_says`.
fn child_finishes(report: &'static str, root_says: &'static str) -> impl Fn(&serde_json::Value) -> Reply + Send + Sync {
    let seen = Mutex::new(std::collections::HashSet::new());
    move |request| {
        let body = match agent_of(request) {
            Some(id) if seen.lock().expect("seen").insert(id) => {
                tool_call_reply("finish", serde_json::json!({"report": report}))
            }
            Some(_) => text_reply("Still here."),
            None => text_reply(root_says),
        };
        Box::pin(async move { body })
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_childs_report_lands_in_its_parent_and_wakes_it(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let requests = mock_with(&pool, child_finishes("All done: 42.", "Thanks.")).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = start_child(&pool, root, "Find the answer.").await;

    eventually("the parent answers the report", || async {
        transcript(&pool, root).await.iter().any(|(role, text)| role == "assistant" && text == "Thanks.")
    })
    .await;
    let root_messages = transcript(&pool, root).await;
    assert_eq!(
        root_messages,
        vec![
            ("user".to_string(), format!("Report from agent #{child}:\n\nAll done: 42.")),
            ("assistant".to_string(), "Thanks.".to_string()),
        ]
    );
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_FINISHED));
    // The child's turn ended at its finish: no model call after it.
    let child_messages = db::list_messages(&pool, child).await.expect("list messages");
    let last = child_messages.last().expect("messages").blocks().expect("blocks");
    assert!(
        matches!(&last[..], [ContentBlock::ToolResult { content, is_error: None, .. }] if content.contains("Your report was sent")),
        "{last:?}"
    );
    eventually("the child's turn is over", || async { !crate::turn::turn_running(child) }).await;
    assert_eq!(requests_for(&requests, Some(child)).len(), 1, "the child called the model once");
    assert_eq!(requests_for(&requests, None).len(), 1, "the parent called the model once");
}

/// A parent mid-turn takes a report at its next iteration, in the same
/// turn: the report is queued while the parent's first call is held, and
/// its second call carries it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_busy_parent_takes_a_report_at_its_next_iteration(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let gate = Arc::new(tokio::sync::Notify::new());
    let root_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let requests = {
        let gate = gate.clone();
        let root_calls = root_calls.clone();
        mock_with(&pool, move |request| {
            if agent_of(request).is_some() {
                let body = tool_call_reply("finish", serde_json::json!({"report": "Done."}));
                return Box::pin(async move { body });
            }
            let gate = gate.clone();
            match root_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 => Box::pin(async move {
                    gate.notified().await;
                    tool_call_reply("todoread", serde_json::json!({}))
                }),
                _ => Box::pin(async { text_reply("Got the report.") }),
            }
        })
        .await
    };
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let (reached, release) = test_hooks::pause(test_hooks::BEFORE_START_DONE, root);
    let child = start_child(&pool, root, "Work.").await;
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the start runs").expect("paused");

    crate::turn::start_turn(pool.clone(), root, "Go on.".to_string()).await.expect("the parent's turn starts");
    eventually("the parent's first call is held", || async { root_calls.load(std::sync::atomic::Ordering::SeqCst) == 1 }).await;
    release.send(()).expect("release the start");
    eventually("the report is queued", || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1")
            .bind(root)
            .fetch_one(&pool)
            .await
            .expect("count")
            == 1
    })
    .await;
    gate.notify_one();

    eventually("the parent answers", || async {
        transcript(&pool, root).await.iter().any(|(_, text)| text == "Got the report.")
    })
    .await;
    eventually("the parent's turns are over", || async { !crate::turn::turn_running(root) }).await;
    let root_requests = requests_for(&requests, None);
    assert_eq!(root_requests.len(), 2, "one turn, two calls: no turn of its own for the report");
    assert!(
        root_requests[1]["messages"].to_string().contains(&format!("Report from agent #{child}:")),
        "the second call carries the report"
    );
}

/// Two reports queued while the parent's turn lock is held arrive in one
/// turn, in the order they were queued.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_two_reports_at_once_arrive_in_one_parent_turn_in_order(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let requests = mock_with(&pool, |request| {
        let body = match agent_of(request) {
            Some(id) => tool_call_reply("finish", serde_json::json!({"report": format!("Report of {id}.")})),
            None => text_reply("Both in."),
        };
        Box::pin(async move { body })
    })
    .await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let held = crate::turn::conversation_lock(root).lock_owned().await;
    let first = start_child(&pool, root, "One.").await;
    let second = start_child(&pool, root, "Two.").await;
    eventually("both reports are queued", || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1")
            .bind(root)
            .fetch_one(&pool)
            .await
            .expect("count")
            == 2
    })
    .await;
    drop(held);

    eventually("the parent answers", || async {
        transcript(&pool, root).await.iter().any(|(_, text)| text == "Both in.")
    })
    .await;
    eventually("the parent's turns are over", || async { !crate::turn::turn_running(root) }).await;
    // In the order they were queued: the two children race to finish, so
    // that's not always their ids' order (CI on the PR's first push).
    let queued: Vec<i64> = sqlx::query_scalar(
        "SELECT from_conversation_id FROM agent_messages WHERE to_conversation_id = $1 ORDER BY id",
    )
    .bind(root)
    .fetch_all(&pool)
    .await
    .expect("the queued reports");
    let mut both = vec![first, second];
    both.sort();
    let mut sorted = queued.clone();
    sorted.sort();
    assert_eq!(sorted, both, "one report from each");
    assert_eq!(
        transcript(&pool, root).await,
        vec![
            ("user".to_string(), format!("Report from agent #{}:\n\nReport of {}.", queued[0], queued[0])),
            ("user".to_string(), format!("Report from agent #{}:\n\nReport of {}.", queued[1], queued[1])),
            ("assistant".to_string(), "Both in.".to_string()),
        ]
    );
    assert_eq!(requests_for(&requests, None).len(), 1, "one model call for both");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_finish_twice_is_refused_the_second_time(pool: PgPool) {
    setup(&pool, vec![text_reply("Working.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = start_child(&pool, root, "Work.").await;
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;

    let input = serde_json::json!({"report": "Done."});
    let first = crate::anthropic::tools::execute(&pool, child, "toolu_f1", "finish", &input).await;
    assert!(first.as_deref().is_ok_and(|r| r.contains("Your report was sent")), "{first:?}");
    let second = crate::anthropic::tools::execute(&pool, child, "toolu_f2", "finish", &input).await;
    assert_eq!(second, Err("you've already finished: your report was sent".to_string()));
    let reports = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_messages WHERE kind = 'report'")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(reports, 1);
    eventually("the parent's turns are over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
}
