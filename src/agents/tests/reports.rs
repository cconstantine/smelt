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

    // SME-390: the report row, then the root's request, then its answer —
    // one wait racing the whole wake chain lost under full-gate load.
    eventually("the report is queued", || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1 AND kind = 'report'")
            .bind(root)
            .fetch_one(&pool)
            .await
            .expect("count")
            == 1
    })
    .await;
    eventually("the root's request runs", || async { requests_for(&requests, None).len() == 1 }).await;
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
async fn test_late_finishes_deliver_follow_up_reports(pool: PgPool) {
    setup(&pool, vec![text_reply("Working.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = start_child(&pool, root, "Work.").await;
    eventually("the child goes active", || async { state(&pool, child).await.as_deref() == Some(db::AGENT_ACTIVE) }).await;

    let input = serde_json::json!({"report": "Done."});
    let first = crate::anthropic::tools::execute(&pool, child, "toolu_f1", "finish", &input).await;
    assert!(first.as_deref().is_ok_and(|r| r.contains("Your report was sent")), "{first:?}");
    let second = crate::anthropic::tools::execute(&pool, child, "toolu_f2", "finish", &input).await;
    assert!(second.as_deref().is_ok_and(|r| r.contains("follow-up") && r.contains("Your turn ends here")), "{second:?}");
    let third = crate::anthropic::tools::execute(&pool, child, "toolu_f3", "finish", &input).await;
    assert!(third.as_deref().is_ok_and(|r| r.contains("follow-up")), "{third:?}");
    let reports = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_messages WHERE kind = 'report'")
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(reports, 3, "every finish a seat makes delivers one row");
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_FINISHED), "the follow-ups wrote no state");
    eventually("the parent's turns are over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
}

/// Review 2, finding 1: the user writes to a working child; its running
/// turn finishes; the user's turn, queued behind it, does more work and
/// finishes again. Its second report reaches the parent too: the user's
/// turn makes the child active again once it holds the turn lock.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_users_message_queued_behind_a_finish_can_finish_again(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let gate = Arc::new(tokio::sync::Notify::new());
    let child_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    {
        let gate = gate.clone();
        let child_calls = child_calls.clone();
        mock_with(&pool, move |request| {
            if agent_of(request).is_none() {
                return Box::pin(async { text_reply("Noted.") });
            }
            let gate = gate.clone();
            match child_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                0 => Box::pin(async move {
                    gate.notified().await;
                    tool_call_reply("finish", serde_json::json!({"report": "First."}))
                }),
                1 => Box::pin(async { tool_call_reply("finish", serde_json::json!({"report": "Second."})) }),
                _ => Box::pin(async { text_reply("ok") }),
            }
        })
        .await;
    }
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = start_child(&pool, root, "Work.").await;
    eventually("the child's first call is held", || async {
        child_calls.load(std::sync::atomic::Ordering::SeqCst) == 1
    })
    .await;
    crate::turn::start_turn(pool.clone(), child, "Also do this.".to_string()).await.expect("the send is admitted");
    gate.notify_one();

    eventually("both of the child's calls ran", || async {
        child_calls.load(std::sync::atomic::Ordering::SeqCst) >= 2 && !crate::turn::turn_running(child)
    })
    .await;
    eventually("both reports are queued", || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1 AND kind = 'report'")
            .bind(root)
            .fetch_one(&pool)
            .await
            .expect("count")
            == 2
    })
    .await;
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_FINISHED));
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}

/// SME-354 B2: a child can't `finish` while an agent of its own still
/// runs, nor while a report from one hasn't reached it: once finished,
/// it would send that report on to no one.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_finish_is_refused_while_the_callers_own_agents_run(pool: PgPool) {
    setup(&pool, vec![text_reply("Thanks.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = active_child(&pool, root).await;
    let grandchild = active_child(&pool, child).await;
    let input = serde_json::json!({"report": "Done."});

    let refused = crate::anthropic::tools::execute(&pool, child, "toolu_f1", "finish", &input).await;
    assert_eq!(
        refused,
        Err(format!(
            "you can't finish while your own agents still run: agent #{grandchild} (active). Wait for their \
             reports, or stop them with stop_agent, then finish"
        ))
    );
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_ACTIVE), "the refusal changed nothing");

    // Its agent finishes; the report is queued and not yet read.
    let queued = db::finish_agent(&pool, grandchild, &format!("agent #{grandchild}"), "Sub-report.").await.expect("finish");
    assert_eq!(queued, db::Finish::Queued { parent: child });
    let refused = crate::anthropic::tools::execute(&pool, child, "toolu_f2", "finish", &input).await;
    assert_eq!(refused, Err("a message arrived since your last step; read it, then finish".to_string()));
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_ACTIVE));

    // Read, it may finish.
    sqlx::query("UPDATE agent_messages SET delivered_at = now() WHERE to_conversation_id = $1")
        .bind(child)
        .execute(&pool)
        .await
        .expect("mark it read");
    let finished = crate::anthropic::tools::execute(&pool, child, "toolu_f3", "finish", &input).await;
    assert!(finished.as_deref().is_ok_and(|r| r.contains("Your report was sent")), "{finished:?}");
    eventually("the parent's turns are over", || async { !crate::turn::turn_running(root) }).await;
}

/// SME-420: the symptom the ticket names, end to end. The child's
/// llama.cpp slot stop is cut in the middle of its `finish` call, and its
/// turn is where the decision lives: the stop learns a tight window, and
/// if that same step's room decision read the changed window, the
/// progress record it needs is wiped as a window change — the child's
/// turn would end and its report would never exist. Here the turn
/// compacts, carries on, and the full `finish` runs: the report row
/// lands in the root, the root's transcript carries it and its reply,
/// the child reaches AGENT_FINISHED, and the retry carried the cut-off
/// notice. A scripted LlamaCpp-kind provider drives the whole chain
/// (mock → run → `finish` → `agent_messages` → parent). Nothing asserted
/// here says which learner branch fired — the candidate cache is
/// process-global, keyed by provider id and reused across test databases
/// — so leftovers from other tests cannot move the outcome; like every
/// writer of it, this test holds `lock_turn_tests`.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_child_slot_stopped_before_its_finish_still_reports_its_parent(pool: PgPool) {
    let _guard = crate::providers::test_support::lock_turn_tests();
    crate::turn::forget_window_stop_candidates();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let child_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let requests = {
        let child_calls = child_calls.clone();
        mock_of_kind(&pool, "llama_cpp", move |request| {
            let body = if request["system"].to_string().contains("You are compacting") {
                text_reply("The summary.")
            } else if agent_of(request).is_some() {
                // The child's steps, by call count: the same numbers the
                // turn-level test scripts — two compactions learn `tight`,
                // a RoomLeft step, then the slot stop cut mid-`finish`;
                // its retry calls `finish` whole.
                match child_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) {
                    0 => with_usage(&tool_call_reply("todoread", serde_json::json!({})), 16_500, 0, 100),
                    1 => with_usage(&tool_call_reply("todoread", serde_json::json!({})), 13_600, 0, 3_000),
                    2 => with_usage(&tool_call_reply("todoread", serde_json::json!({})), 13_600, 0, 100),
                    3 => with_usage(&tool_call_reply("todoread", serde_json::json!({})), 24_000, 0, 100),
                    4 => {
                        let max_tokens = request["max_tokens"].as_i64().expect("the RoomLeft request carries its room as a budget");
                        slot_stop_mid_finish(24_000, max_tokens / 8)
                    }
                    5 => tool_call_reply("finish", serde_json::json!({"report": "All done: 42."})),
                    _ => text_reply("Should never be asked: the child ran past its finish."),
                }
            } else {
                text_reply("Thanks.")
            };
            Box::pin(async move { body })
        })
        .await
    };
    set_context_window(&pool, 32_768).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = start_child(&pool, root, "Work.").await;

    // SME-390's shape: one wait per link of the chain, none racing all of it.
    eventually("the report is queued", || async {
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1 AND kind = 'report'")
            .bind(root)
            .fetch_one(&pool)
            .await
            .expect("count")
            == 1
    })
    .await;
    // The root's one real call. Not `requests_for(None).len()`: the
    // child's compaction summaries carry no "You are agent #" system
    // line, so they land in the no-parent bucket too — all three are
    // in by the time the report queues, and the count passes through
    // 4, never 1. Filter to what isn't a summary.
    eventually("the root's request runs", || async {
        requests_for(&requests, None)
            .iter()
            .filter(|r| !r["system"].to_string().contains("You are compacting"))
            .count()
            == 1
    })
    .await;
    eventually("the parent answers the report", || async {
        transcript(&pool, root).await.iter().any(|(role, text)| role == "assistant" && text == "Thanks.")
    })
    .await;
    assert_eq!(
        transcript(&pool, root).await,
        vec![
            ("user".to_string(), format!("Report from agent #{child}:\n\nAll done: 42.")),
            ("assistant".to_string(), "Thanks.".to_string()),
        ]
    );
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_FINISHED), "the child reached its end");
    let child_requests = requests_for(&requests, Some(child));
    assert_eq!(child_requests.len(), 6, "through the stop, the room compaction, and the full finish");
    assert!(
        child_requests[5]["messages"].to_string().contains(crate::api::chat::CUT_OFF_NOTICE_START),
        "the retry after the stop carried the cut-off notice: {}",
        child_requests[5]["messages"]
    );
}

/// A mock upstream shaped like `mock_with`, saved as a provider of
/// `kind` — the llama.cpp kind's stop tell is what SME-420 needs.
async fn mock_of_kind(
    pool: &PgPool,
    kind: &str,
    answer: impl Fn(&serde_json::Value) -> Reply + Send + Sync + 'static,
) -> Arc<Mutex<Vec<serde_json::Value>>> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let requests = Arc::new(Mutex::new(Vec::new()));
    let recorded = requests.clone();
    let answer = Arc::new(answer);
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |request: String| {
            let recorded = recorded.clone();
            let answer = answer.clone();
            async move {
                let parsed: serde_json::Value = serde_json::from_str(&request).expect("a JSON request body");
                let reply = answer(&parsed);
                recorded.lock().expect("the request log").push(parsed);
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], reply.await)
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    crate::providers::test_support::add_mock_provider_kind(pool, addr, kind).await;
    requests
}

/// Saves `mock-model`'s Context window, as the provider page's field
/// would — a copy of the turn tests' `set_context_window`, which is
/// `pub(super)` inside `turn::tests` and unreachable from here.
async fn set_context_window(pool: &PgPool, window: i32) {
    sqlx::query("INSERT INTO provider_models (provider_id, model, context_window, max_output) SELECT id, $1, $2, $3 FROM inference_providers")
        .bind(crate::providers::test_support::MOCK_MODEL)
        .bind(window)
        .bind(None::<i32>)
        .execute(pool)
        .await
        .expect("seed the window");
}

/// `body` reporting `input`/`cache_read` input tokens and `output` output
/// tokens, as a real reply's `message_start` and `message_delta` do — a
/// copy of the turn tests' `with_usage`, same reason.
fn with_usage(body: &str, input: i64, cache_read: i64, output: i64) -> String {
    body.replacen(
        r#"{"type":"message_start"}"#,
        &format!(
            r#"{{"type":"message_start","message":{{"usage":{{"input_tokens":{input},"cache_creation_input_tokens":0,"cache_read_input_tokens":{cache_read},"output_tokens":1}}}}}}"#
        ),
        1,
    )
    .replacen(r#""delta":{"stop_reason""#, &format!(r#""usage":{{"output_tokens":{output}}},"delta":{{"stop_reason""#), 1)
}

/// A llama.cpp slot stop cut in the middle of a `finish` call: the stream
/// ends `max_tokens` with only the start of the call written, so the
/// call is dropped and never runs — the state SME-420 is about.
fn slot_stop_mid_finish(input: i64, output: i64) -> String {
    let body = sse_body(&[
        ("message_start", r#"{"type":"message_start"}"#),
        ("content_block_start", r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"toolu_f","name":"finish","input":{}}}"#),
        ("content_block_delta", r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"report\": \"All done"}}"#),
        ("message_delta", r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"}}"#),
        ("message_stop", r#"{"type":"message_stop"}"#),
    ]);
    with_usage(&body, input, 0, output)
}
