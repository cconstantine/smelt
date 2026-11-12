//! Compacting on request (SME-482): `compact_now`, the endpoint's work,
//! between turns. T8–T13 of the plan.

use super::on_request::dividers;
use super::*;

/// A mock upstream whose summary requests (the compaction system prompt)
/// answer `summary`, once `gate` lets them when `gated`; every other
/// request answers `replies` in order, the last repeated. Every request
/// body is kept.
struct Gated {
    requests: std::sync::Mutex<Vec<serde_json::Value>>,
    gate: tokio::sync::Semaphore,
    /// A summary request has arrived (a stored permit if no one waits yet).
    summary_seen: tokio::sync::Notify,
}

impl Gated {
    fn open(&self) {
        self.gate.add_permits(1_000);
    }

    fn count(&self) -> usize {
        self.requests.lock().expect("the request log").len()
    }

    fn summaries(&self) -> Vec<serde_json::Value> {
        let log = self.requests.lock().expect("the request log");
        log.iter().filter(|r| is_summary(r)).cloned().collect()
    }

    fn turn_requests(&self) -> Vec<serde_json::Value> {
        let log = self.requests.lock().expect("the request log");
        log.iter().filter(|r| !is_summary(r)).cloned().collect()
    }
}

fn is_summary(request: &serde_json::Value) -> bool {
    request["system"].to_string().contains("You are compacting")
}

async fn gated_mock(pool: &PgPool, replies: Vec<String>, summary: String, gated: bool) -> Arc<Gated> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let mock = Arc::new(Gated {
        requests: std::sync::Mutex::new(Vec::new()),
        gate: tokio::sync::Semaphore::new(if gated { 0 } else { 1_000 }),
        summary_seen: tokio::sync::Notify::new(),
    });
    let state = mock.clone();
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |body: String| {
            let state = state.clone();
            let replies = replies.clone();
            let summary = summary.clone();
            async move {
                let parsed: serde_json::Value = serde_json::from_str(&body).expect("a JSON request body");
                let summarizing = is_summary(&parsed);
                let turn_calls = {
                    let mut log = state.requests.lock().expect("the request log");
                    log.push(parsed);
                    log.iter().filter(|r| !is_summary(r)).count()
                };
                let body = if summarizing {
                    state.summary_seen.notify_one();
                    let _permit = state.gate.acquire().await.expect("the gate");
                    summary
                } else {
                    replies[(turn_calls.max(1) - 1).min(replies.len() - 1)].clone()
                };
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], body)
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    crate::providers::test_support::add_mock_provider(pool, addr).await;
    mock
}

/// A conversation with one exchange: the user's `ask`, the reply "noted".
async fn seeded(pool: &PgPool, ask: &str) -> i64 {
    let conversation = db::create_conversation(pool).await.expect("create conversation");
    db::create_message(pool, conversation.id, "user", &[text_block(ask)]).await.expect("the ask");
    db::create_message(pool, conversation.id, "assistant", &[text_block("noted")]).await.expect("the reply");
    conversation.id
}

async fn message_count(pool: &PgPool, id: i64) -> usize {
    db::list_messages(pool, id).await.expect("messages").len()
}

/// The text of the last message saved.
async fn last_text(pool: &PgPool, id: i64) -> String {
    let messages = db::list_messages(pool, id).await.expect("messages");
    serde_json::to_string(&messages.last().expect("a message").blocks().expect("blocks")).expect("blocks serialize")
}

fn error_text(e: &ServerFnError) -> String {
    crate::api::chat::chat_error_text(e)
}

/// T8: an idle conversation compacts on request: the divider says so, the
/// note reaches the summariser, the continuation quotes the user and pins
/// the todo list, usage is cleared, and the next turn starts from it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_endpoint_compacts_an_idle_conversation(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mock = gated_mock(
        &pool,
        vec![with_usage(&text_reply_body("noted"), 900, 0, 20), text_reply_body("after")],
        text_reply_body("SUMMARY-8"),
        false,
    )
    .await;
    let conversation = db::create_conversation(&pool).await.expect("create conversation");
    let id = conversation.id;
    run_turn(&pool, id, anthropic::AnthropicMessage { role: "user".into(), content: vec![text_block("keep the plan")] })
        .await
        .expect("the first turn");
    let todo = anthropic::tools::TodoItem { content: "plan step".into(), status: anthropic::tools::TodoStatus::Pending };
    db::set_conversation_todos(&pool, id, &[todo]).await.expect("a todo");
    assert!(db::get_conversation_usage(&pool, id).await.expect("usage").is_some(), "the reply measured usage");

    compact_now(&pool, id, Some("keep the plan's SHA".to_string())).await.expect("compacted");

    assert_eq!(dividers(&pool, id).await, vec![("SUMMARY-8".to_string(), Some("on request".to_string()))]);
    let summaries = mock.summaries();
    assert_eq!(summaries.len(), 1);
    assert!(summaries[0].to_string().contains("keep the plan's SHA"), "the note reaches the summariser: {}", summaries[0]);
    let continuation = last_text(&pool, id).await;
    assert!(continuation.contains("keep the plan"), "the user's words are quoted: {continuation}");
    assert!(continuation.contains("- plan step (pending)"), "the todo list is pinned: {continuation}");
    assert_eq!(db::get_conversation_usage(&pool, id).await.expect("usage"), None, "usage is cleared");
    run_turn(&pool, id, hello()).await.expect("the next turn");
    let next = mock.turn_requests().pop().expect("the next turn's request");
    assert!(next["messages"][0].to_string().contains(COMPACTION_PLACEHOLDER_PROMPT), "{next}");
    assert!(next["messages"][1].to_string().contains("SUMMARY-8"), "{next}");
}

/// T9: refused at once while a turn runs (parked after its reply), and
/// while one is queued behind a held lock: no request, no divider.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_endpoint_refuses_while_a_turn_runs_or_is_queued(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mock = gated_mock(&pool, vec![text_reply_body("noted")], text_reply_body("SUMMARY-9"), false).await;
    let id = seeded(&pool, "hi").await;
    let (reached, release) = test_hooks::pause_after_reply_saved(id);
    let turn_pool = pool.clone();
    let turn = tokio::spawn(async move { run_turn(&turn_pool, id, hello()).await.map(|_| ()) });
    tokio::time::timeout(std::time::Duration::from_secs(10), reached).await.expect("parked within 10 s").expect("the hook");

    let started = std::time::Instant::now();
    let refused = compact_now(&pool, id, None).await.expect_err("refused while running");
    assert!(started.elapsed() < std::time::Duration::from_secs(1), "at once: {:?}", started.elapsed());
    assert!(error_text(&refused).contains("A turn is running"), "{refused}");
    assert_eq!(mock.count(), 1, "no summary request");
    release.send(()).expect("release the turn");
    tokio::time::timeout(std::time::Duration::from_secs(10), turn).await.expect("ends").expect("task").expect("Ok");

    let held = conversation_lock(id).lock_owned().await;
    let queued_pool = pool.clone();
    let queued = tokio::spawn(async move { run_turn(&queued_pool, id, hello()).await.map(|_| ()) });
    tokio::task::yield_now().await;
    let refused = compact_now(&pool, id, None).await.expect_err("refused while queued");
    assert!(error_text(&refused).contains("A turn is running"), "{refused}");
    drop(held);
    tokio::time::timeout(std::time::Duration::from_secs(10), queued).await.expect("ends").expect("task").expect("Ok");
    assert!(mock.summaries().is_empty(), "no summary at all");
    assert!(dividers(&pool, id).await.is_empty(), "no divider");
}

/// T10: the states it refuses, each with no request and no divider.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_endpoint_refuses_what_it_cant_compact(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mock = gated_mock(&pool, vec![text_reply_body("noted")], text_reply_body("SUMMARY-10"), false).await;
    let questions = crate::questions::parse_questions(&serde_json::json!({"questions": [
        {"question": "Go on?", "header": "Go", "options": [{"label": "Yes"}, {"label": "No"}]}
    ]}))
    .expect("questions");

    let empty = db::create_conversation(&pool).await.expect("create conversation").id;
    let asked_user = seeded(&pool, "hi").await;
    db::create_pending_question(&pool, asked_user, "toolu_q", &questions).await.expect("a question");
    let asked_parent = seeded(&pool, "hi").await;
    let mut conn = pool.acquire().await.expect("a connection");
    db::upsert_pending_question(&mut conn, asked_parent, "toolu_p", &questions, db::ASKED_OF_PARENT)
        .await
        .expect("a parent question");
    drop(conn);
    let unanswered = seeded(&pool, "hi").await;
    let call = anthropic::ContentBlock::ToolUse { id: "toolu_x".into(), name: "todoread".into(), input: serde_json::json!({}) };
    db::create_message(&pool, unanswered, "assistant", &[call]).await.expect("an unanswered call");
    let compacted = seeded(&pool, "hi").await;
    compact_now(&pool, compacted, None).await.expect("the first compaction");
    let requests = mock.count();

    for (id, why) in [
        (empty, "Nothing to compact yet"),
        (asked_user, "answer it first"),
        (asked_parent, "answer it first"),
        (unanswered, "send a message first"),
        (compacted, "Nothing new since the last compaction"),
    ] {
        let before = message_count(&pool, id).await;
        let refused = compact_now(&pool, id, None).await.expect_err(why);
        assert!(error_text(&refused).contains(why), "{why}: {refused}");
        assert_eq!(message_count(&pool, id).await, before, "{why}: nothing saved");
    }
    assert_eq!(mock.count(), requests, "no request for any of them");
}

/// T11: SME-443 on the endpoint: an empty summary is refused, and the
/// history stays as it was.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_empty_summary_fails_the_endpoints_compaction(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    gated_mock(&pool, vec![text_reply_body("noted")], text_reply_body(""), false).await;
    let id = seeded(&pool, "hi").await;

    let failed = compact_now(&pool, id, None).await.expect_err("an empty summary");

    assert!(error_text(&failed).contains("came back empty"), "{failed}");
    assert!(dividers(&pool, id).await.is_empty());
    assert_eq!(message_count(&pool, id).await, 2, "the history is as it was");
}

/// Waits (up to 10 s) for `id`'s divider to land whole: the summary and
/// the continuation after it.
async fn wait_for_divider(pool: &PgPool, id: i64, messages: usize) {
    for _ in 0..200 {
        if message_count(pool, id).await >= messages {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the divider didn't land: {} messages", message_count(pool, id).await);
}

/// T12: a client hanging up mid-summary doesn't cut the compaction: it
/// lands whole, and the turn lock is free after.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_hang_up_doesnt_cut_the_endpoints_compaction(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mock = gated_mock(&pool, vec![text_reply_body("noted")], text_reply_body("SUMMARY-12"), true).await;
    let id = seeded(&pool, "hi").await;

    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        tokio::select! {
            result = compact_now(&pool, id, None) => panic!("returned before the summary was let through: {result:?}"),
            () = mock.summary_seen.notified() => {}
        }
    })
    .await
    .expect("the summary is asked for within 10 s");
    mock.open();

    wait_for_divider(&pool, id, 5).await;
    assert_eq!(dividers(&pool, id).await, vec![("SUMMARY-12".to_string(), Some("on request".to_string()))]);
    assert!(last_text(&pool, id).await.contains("Continue based on the summary above"), "the continuation landed");
    for _ in 0..200 {
        if conversation_lock(id).try_lock().is_ok() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    panic!("the turn lock is still held");
}

/// T13: a send while the endpoint compacts queues behind it, and runs on
/// the compacted history.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_send_during_the_endpoints_compaction_queues_behind_it(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mock = gated_mock(&pool, vec![text_reply_body("after-reply")], text_reply_body("SUMMARY-13"), true).await;
    let id = seeded(&pool, "hi").await;
    let compact_pool = pool.clone();
    let compaction = tokio::spawn(async move { compact_now(&compact_pool, id, None).await });
    tokio::time::timeout(std::time::Duration::from_secs(10), mock.summary_seen.notified())
        .await
        .expect("the summary is asked for within 10 s");

    start_turn(pool.clone(), id, "next".to_string()).await.expect("the send is admitted");
    mock.open();
    tokio::time::timeout(std::time::Duration::from_secs(10), compaction).await.expect("ends").expect("task").expect("compacted");
    for _ in 0..200 {
        if last_reply(&db::list_messages(&pool, id).await.expect("messages")).as_deref() == Some("after-reply") {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let sent = mock.turn_requests();
    assert_eq!(sent.len(), 1, "one turn request: {sent:#?}");
    assert!(sent[0]["messages"][0].to_string().contains(COMPACTION_PLACEHOLDER_PROMPT), "{}", sent[0]);
    assert!(sent[0]["messages"][1].to_string().contains("SUMMARY-13"), "{}", sent[0]);
    let messages = db::list_messages(&pool, id).await.expect("messages");
    let at = |needle: &str| messages.iter().position(|m| m.content.contains(needle)).unwrap_or_else(|| panic!("{needle} saved"));
    assert!(at("SUMMARY-13") < at("after-reply"), "the reply lands after the divider");
}

/// SME-482 round 1 L4: a lock held by something other than a turn (another
/// compaction, an idle pass between turns) isn't called a running turn.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_endpoint_says_busy_when_no_turn_holds_the_lock(pool: PgPool) {
    let _guard = lock_turn_tests();
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("clear ids");
    let mock = gated_mock(&pool, vec![text_reply_body("noted")], text_reply_body("SUMMARY-L4"), false).await;
    let id = seeded(&pool, "hi").await;
    let held = conversation_lock(id).lock_owned().await;

    let refused = compact_now(&pool, id, None).await.expect_err("refused while the lock is held");

    let text = error_text(&refused);
    assert!(!text.contains("A turn is running"), "no turn runs: {text}");
    assert!(text.contains("busy"), "{text}");
    drop(held);
    assert_eq!(mock.count(), 0, "no summary request");
}
