//! `message_agent` (SME-238): a parent and its child write to each other.

use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

async fn message(pool: &PgPool, from: i64, to: serde_json::Value, text: &str) -> Result<String, String> {
    crate::anthropic::tools::execute(pool, from, "toolu_m", "message_agent", &serde_json::json!({"to": to, "text": text}))
        .await
}

async fn set_state(pool: &PgPool, id: i64, state: &str) {
    sqlx::query("UPDATE agents SET state = $2 WHERE conversation_id = $1")
        .bind(id)
        .bind(state)
        .execute(pool)
        .await
        .expect("set the state");
}

/// `(kind, body, delivered)` of every row queued to `to`.
async fn queued_to(pool: &PgPool, to: i64) -> Vec<(String, String, bool)> {
    sqlx::query_as::<_, (String, String, bool)>(
        "SELECT kind, body, delivered_at IS NOT NULL FROM agent_messages WHERE to_conversation_id = $1 ORDER BY id",
    )
    .bind(to)
    .fetch_all(pool)
    .await
    .expect("queued rows")
}

/// Sessions in this test's database waiting on a row lock.
async fn lock_waiters(pool: &PgPool) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock'")
        .fetch_one(pool)
        .await
        .expect("pg_stat_activity")
}

async fn until_waiters(pool: &PgPool, n: i64) {
    eventually(&format!("{n} lock waiters"), || async { lock_waiters(pool).await >= n }).await;
}

/// A mock upstream whose first reply to an agent waits for a permit on
/// `gate` and then is `first`; every other reply is "Noted.".
async fn gated_mock(pool: &PgPool, first: String) -> (Arc<Mutex<Vec<serde_json::Value>>>, Arc<tokio::sync::Semaphore>) {
    db::test_support::start_ids_clear_of_other_runs(pool).await.expect("ids clear of other runs");
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let served = Arc::new(AtomicUsize::new(0));
    let requests = mock_with(pool, {
        let gate = gate.clone();
        move |request| {
            let gate = gate.clone();
            let first = first.clone();
            let n = if agent_of(request).is_some() { served.fetch_add(1, Ordering::SeqCst) } else { usize::MAX };
            Box::pin(async move {
                if n == 0 {
                    gate.acquire().await.expect("the gate").forget();
                    return first;
                }
                text_reply("Noted.")
            })
        }
    })
    .await;
    (requests, gate)
}

/// Every text and tool result after a request's last assistant message,
/// in order: what the model reads since its last step.
fn last_message(request: &serde_json::Value) -> Vec<String> {
    let messages = request["messages"].as_array().expect("messages");
    let from = messages.iter().rposition(|m| m["role"] == "assistant").map_or(0, |i| i + 1);
    messages[from..]
        .iter()
        .flat_map(|m| match &m["content"] {
            serde_json::Value::String(text) => vec![text.clone()],
            serde_json::Value::Array(blocks) => blocks
                .iter()
                .map(|b| match b["type"].as_str() {
                    Some("text") => b["text"].as_str().unwrap_or_default().to_string(),
                    Some("tool_result") => format!("tool_result: {}", b["content"]),
                    other => format!("{other:?}"),
                })
                .collect(),
            other => vec![other.to_string()],
        })
        .collect()
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_message_agent_refuses_anyone_but_the_parent_and_its_own_children(pool: PgPool) {
    setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let lone = db::create_conversation(&pool).await.expect("create conversation").id;
    let names = |tools: Vec<crate::anthropic::ToolDefinition>| tools.into_iter().map(|t| t.name).collect::<Vec<_>>();
    assert!(!names(crate::anthropic::tools::tool_definitions(&pool, root).await).contains(&"message_agent".to_string()));
    let child = active_child(&pool, root).await;
    let sibling = active_child(&pool, root).await;
    let other = db::create_conversation(&pool).await.expect("create conversation").id;
    let others_child = active_child(&pool, other).await;
    assert!(names(crate::anthropic::tools::tool_definitions(&pool, root).await).contains(&"message_agent".to_string()));
    assert!(names(crate::anthropic::tools::tool_definitions(&pool, child).await).contains(&"message_agent".to_string()));

    let not_yours = |id: i64| Err(format!("#{id} isn't one of your agents (list_agents lists them)"));
    assert_eq!(message(&pool, child, serde_json::json!(sibling), "Hi.").await, not_yours(sibling));
    assert_eq!(message(&pool, root, serde_json::json!(others_child), "Hi.").await, not_yours(others_child));
    assert_eq!(message(&pool, root, serde_json::json!(root.to_string()), "Hi.").await, not_yours(root));
    assert_eq!(
        message(&pool, root, serde_json::json!("someone"), "Hi.").await,
        Err("to must be \"parent\" or the id of one of your agents (list_agents lists them)".to_string())
    );
    assert_eq!(
        message(&pool, root, serde_json::json!("parent"), "Hi.").await,
        Err("you have no parent: to is \"parent\" only for an agent".to_string())
    );
    assert_eq!(
        message(&pool, lone, serde_json::json!("parent"), "Hi.").await,
        Err("message_agent is only for a conversation with a parent or agents of its own".to_string())
    );
    assert!(message(&pool, root, serde_json::json!(child), " ").await.expect_err("empty").starts_with("text must be 1 to"));

    set_state(&pool, child, db::AGENT_STOPPED).await;
    assert_eq!(
        message(&pool, root, serde_json::json!(child), "Hi.").await,
        Err(format!("agent #{child} is stopped; it takes no messages unless the user writes to it"))
    );
    set_state(&pool, child, db::AGENT_FAILED).await;
    assert_eq!(message(&pool, root, serde_json::json!(child), "Hi.").await, Err(format!("agent #{child}'s start failed")));
    sqlx::query("UPDATE conversations SET deleting_at = now() WHERE id = $1")
        .bind(sibling)
        .execute(&pool)
        .await
        .expect("mark it deleting");
    assert_eq!(message(&pool, root, serde_json::json!(sibling), "Hi.").await, Err(format!("agent #{sibling} was deleted")));
    for id in [root, child, sibling, others_child] {
        assert!(queued_to(&pool, id).await.is_empty(), "a refused message was queued to #{id}");
    }
}

/// An idle child is woken by its parent's message, and its parent by the
/// child's; each reads who it's from.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_message_to_an_idle_child_wakes_it_and_back(pool: PgPool) {
    setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = active_child(&pool, root).await;

    assert_eq!(message(&pool, root, serde_json::json!(child.to_string()), "Hello child.").await, Ok(format!("Sent to agent #{child}.")));
    eventually("the child read it and answered", || async {
        transcript(&pool, child).await
            == vec![
                ("user".to_string(), "Message from your parent:\n\nHello child.".to_string()),
                ("assistant".to_string(), "Noted.".to_string()),
            ]
    })
    .await;
    let linked = sqlx::query_scalar::<_, bool>(
        "SELECT delivered_message_id IS NOT NULL FROM agent_messages WHERE to_conversation_id = $1 AND kind = 'message'",
    )
    .bind(child)
    .fetch_one(&pool)
    .await
    .expect("the row");
    assert!(linked, "the row names the message it became");

    assert_eq!(message(&pool, child, serde_json::json!("parent"), "Hello parent.").await, Ok("Sent to your parent.".to_string()));
    eventually("the parent read it", || async {
        transcript(&pool, root).await.contains(&("user".to_string(), format!("Message from agent #{child}:\n\nHello parent.")))
    })
    .await;
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
}

/// The child's model call is running when the message lands: it isn't put
/// into that call, and arrives with the call's tool result, before the
/// next one.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_busy_recipient_takes_a_message_at_its_next_iteration(pool: PgPool) {
    let (requests, gate) = gated_mock(&pool, tool_call_reply("list_agents", serde_json::json!({}))).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = active_child(&pool, root).await;
    crate::turn::start_turn(pool.clone(), child, "Work.".to_string()).await.expect("the send");
    eventually("the child's call is running", || async { requests_for(&requests, Some(child)).len() == 1 }).await;

    message(&pool, root, serde_json::json!(child), "Mid-call.").await.expect("sent");
    gate.add_permits(1);
    eventually("the child's turn is over", || async {
        requests_for(&requests, Some(child)).len() == 2 && !crate::turn::turn_running(child)
    })
    .await;
    let calls = requests_for(&requests, Some(child));
    assert!(!calls[0].to_string().contains("Mid-call."), "the message went into the running call");
    let last = last_message(&calls[1]);
    assert_eq!(last.len(), 2, "{last:?}");
    assert!(last[0].starts_with("tool_result: "), "{last:?}");
    assert_eq!(last[1], "Message from your parent:\n\nMid-call.");
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}

/// A paused child keeps its messages queued, unwoken; the user's next
/// message takes all of them in one turn, in the order they were sent.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_several_messages_to_a_paused_child_wait_and_arrive_in_one_turn_in_order(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = active_child(&pool, root).await;
    user_stop(&pool, child).await.expect("the user's Stop");
    for text in ["One.", "Two.", "Three."] {
        message(&pool, root, serde_json::json!(child), text).await.expect("sent");
    }
    crate::turn::start_turn(pool.clone(), child, "I'm back.".to_string()).await.expect("the send");
    eventually("the child's turn is over", || async {
        !requests_for(&requests, Some(child)).is_empty() && !crate::turn::turn_running(child)
    })
    .await;
    let calls = requests_for(&requests, Some(child));
    assert_eq!(calls.len(), 1, "a wake ran a turn before the user wrote");
    let request = calls[0].to_string();
    let at = |text: &str| request.find(&format!("Message from your parent:\\n\\n{text}")).unwrap_or_else(|| panic!("{text} missing: {request}"));
    assert!(at("One.") < at("Two.") && at("Two.") < at("Three."), "{request}");
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_message_to_a_finished_child_makes_it_active_and_wakes_it(pool: PgPool) {
    setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = active_child(&pool, root).await;
    set_state(&pool, child, db::AGENT_FINISHED).await;

    message(&pool, root, serde_json::json!(child), "One more thing.").await.expect("sent");
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_ACTIVE), "active in the message's own transaction");
    eventually("the child read it", || async {
        transcript(&pool, child).await.contains(&("assistant".to_string(), "Noted.".to_string()))
    })
    .await;
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
}

/// A message to a child still starting waits for its first turn, which
/// reads it after the brief.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_message_to_a_starting_child_arrives_in_its_first_turn(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let (reached, release) = test_hooks::pause(test_hooks::BEFORE_START_DONE, root);
    let child = start_child(&pool, root, "The brief.").await;
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the start runs").expect("paused");

    message(&pool, root, serde_json::json!(child), "Early.").await.expect("sent");
    release.send(()).expect("release the start");
    eventually("the child's first turn is over", || async {
        !requests_for(&requests, Some(child)).is_empty() && !crate::turn::turn_running(child)
    })
    .await;
    let first = requests_for(&requests, Some(child)).remove(0).to_string();
    assert!(first.contains("The brief.") && first.contains("Message from your parent:\\n\\nEarly."), "{first}");
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}

/// Interleaving: the parent's message commits while the child's model call
/// runs, then the child calls `finish`: refused, and its next step reads
/// the message. Without the refusal the child would be `finished` with the
/// message still to read, and couldn't report on it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_finish_is_refused_while_a_message_is_queued(pool: PgPool) {
    let (requests, gate) = gated_mock(&pool, tool_call_reply("finish", serde_json::json!({"report": "Done."}))).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = active_child(&pool, root).await;
    crate::turn::start_turn(pool.clone(), child, "Work.".to_string()).await.expect("the send");
    eventually("the child's call is running", || async { requests_for(&requests, Some(child)).len() == 1 }).await;

    message(&pool, root, serde_json::json!(child), "Wait, also this.").await.expect("sent");
    gate.add_permits(1);
    eventually("the child's turn is over", || async {
        requests_for(&requests, Some(child)).len() == 2 && !crate::turn::turn_running(child)
    })
    .await;
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_ACTIVE), "finish was refused");
    let last = last_message(&requests_for(&requests, Some(child))[1]);
    assert!(last[0].contains("a message arrived since your last step; read it, then finish"), "{last:?}");
    assert_eq!(last[1], "Message from your parent:\n\nWait, also this.");
    assert!(
        !queued_to(&pool, root).await.iter().any(|(kind, _, _)| kind == db::KIND_REPORT),
        "no report was sent"
    );
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}

/// Interleaving: the child's `finish` holds its row (after its update) when
/// the parent's message arrives; the message waits, then finds the child
/// `finished`, makes it active and wakes it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_message_after_finish_makes_the_child_active_again(pool: PgPool) {
    let requests = setup(&pool, vec![tool_call_reply("finish", serde_json::json!({"report": "Done."})), text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = active_child(&pool, root).await;
    let (reached, release) = test_hooks::pause(test_hooks::FINISH_UPDATED, child);
    crate::turn::start_turn(pool.clone(), child, "Work.".to_string()).await.expect("the send");
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("finish runs").expect("paused");
    let sending = tokio::spawn({
        let pool = pool.clone();
        async move { message(&pool, root, serde_json::json!(child), "And then?").await }
    });
    until_waiters(&pool, 1).await;
    release.send(()).expect("release finish");
    let sent = tokio::time::timeout(Duration::from_secs(10), sending).await.expect("the message ends").expect("its task");
    assert_eq!(sent, Ok(format!("Sent to agent #{child}.")));
    eventually("the child read it", || async {
        transcript(&pool, child).await.iter().any(|(_, text)| text == "Message from your parent:\n\nAnd then?")
            && !crate::turn::turn_running(child)
    })
    .await;
    assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_ACTIVE));
    assert!(queued_to(&pool, root).await.iter().any(|(kind, body, _)| kind == db::KIND_REPORT && body == "Done."));
    assert!(requests_for(&requests, Some(child)).len() >= 2);
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}

/// A row lock on the child's `agents` row decides the order: whichever of
/// `message_agent` and `stop_agent` queued on it first goes first. Stop
/// first: the message is refused. Message first: it is queued, and the
/// stop that follows leaves it unread, with no turn.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_message_agent_racing_stop_agent(pool: PgPool) {
    let requests = setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    for stop_first in [true, false] {
        let child = active_child(&pool, root).await;
        // Paused, so a message landing first doesn't run a turn before the stop.
        user_stop(&pool, child).await.expect("the user's Stop");
        let mut holding = pool.begin().await.expect("begin");
        sqlx::query("SELECT 1 FROM agents WHERE conversation_id = $1 FOR UPDATE")
            .bind(child)
            .execute(&mut *holding)
            .await
            .expect("hold the child's row");
        let stop = || {
            let pool = pool.clone();
            tokio::spawn(async move {
                crate::anthropic::tools::execute(&pool, root, "toolu_x", "stop_agent", &serde_json::json!({"id": child})).await
            })
        };
        let send = || {
            let pool = pool.clone();
            tokio::spawn(async move { message(&pool, root, serde_json::json!(child), "Race.").await })
        };
        let first = if stop_first { stop() } else { send() };
        until_waiters(&pool, 1).await;
        let other = if stop_first { send() } else { stop() };
        until_waiters(&pool, 2).await;
        holding.rollback().await.expect("release the row");
        let first = tokio::time::timeout(Duration::from_secs(10), first).await.expect("no deadlock").expect("task");
        let other = tokio::time::timeout(Duration::from_secs(10), other).await.expect("no deadlock").expect("task");
        let (stopped, sent) = if stop_first { (first, other) } else { (other, first) };
        assert_eq!(stopped, Ok(format!("Stopped agent #{child}.")));
        assert_eq!(state(&pool, child).await.as_deref(), Some(db::AGENT_STOPPED));
        if stop_first {
            assert_eq!(sent, Err(format!("agent #{child} is stopped; it takes no messages unless the user writes to it")));
            assert!(queued_to(&pool, child).await.is_empty());
        } else {
            assert_eq!(sent, Ok(format!("Sent to agent #{child}.")));
            assert_eq!(queued_to(&pool, child).await, vec![(db::KIND_MESSAGE.to_string(), "Race.".to_string(), false)]);
        }
        assert!(requests_for(&requests, Some(child)).is_empty(), "the child ran a turn");
        crate::turn::forget_conversation_lock(child);
    }
}

/// The delete's marking against the message, both orders: refused as
/// deleted, or queued and then cascaded away; never an error or a deadlock.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_message_agent_racing_a_delete(pool: PgPool) {
    setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;

    // The delete marks the child first; the message comes while it's held.
    let child = active_child(&pool, root).await;
    let (reached, release) = test_hooks::pause(test_hooks::DELETE_MARKED, child);
    let deleting = tokio::spawn({
        let pool = pool.clone();
        async move { delete_subtree(&pool, child).await }
    });
    tokio::time::timeout(Duration::from_secs(10), reached).await.expect("the delete runs").expect("paused");
    assert_eq!(message(&pool, root, serde_json::json!(child), "Hi.").await, Err(format!("agent #{child} was deleted")));
    release.send(()).expect("release the delete");
    tokio::time::timeout(Duration::from_secs(10), deleting).await.expect("no deadlock").expect("task").expect("the delete");

    // The message holds the child's row first; the delete's marking waits.
    let child = active_child(&pool, root).await;
    let mut holding = pool.begin().await.expect("begin");
    sqlx::query("SELECT 1 FROM agents WHERE conversation_id = $1 FOR UPDATE")
        .bind(child)
        .execute(&mut *holding)
        .await
        .expect("hold the child's row");
    let sending = tokio::spawn({
        let pool = pool.clone();
        async move { message(&pool, root, serde_json::json!(child), "Hi.").await }
    });
    until_waiters(&pool, 1).await;
    let deleting = tokio::spawn({
        let pool = pool.clone();
        async move { delete_subtree(&pool, child).await }
    });
    until_waiters(&pool, 2).await;
    holding.rollback().await.expect("release the row");
    let sent = tokio::time::timeout(Duration::from_secs(10), sending).await.expect("no deadlock").expect("task");
    tokio::time::timeout(Duration::from_secs(10), deleting).await.expect("no deadlock").expect("task").expect("the delete");
    assert_eq!(sent, Ok(format!("Sent to agent #{child}.")));
    assert!(!db::conversation_exists(&pool, child).await.expect("exists"), "deleted");
    assert!(queued_to(&pool, child).await.is_empty(), "its row went with it");
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}

/// smelt stops between the message's commit and its wake: the startup
/// sweep wakes the child, which reads it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_queued_message_survives_a_restart(pool: PgPool) {
    setup(&pool, vec![text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = active_child(&pool, root).await;
    let queued = db::message_agent(&pool, root, db::Recipient::Child(child), "your parent", "Before the restart.")
        .await
        .expect("queue it");
    assert_eq!(queued, db::MessageOutcome::Queued(child));

    restart(&pool, &[root, child]).await;
    assert_eq!(
        transcript(&pool, child).await[..2],
        [
            ("user".to_string(), "Message from your parent:\n\nBefore the restart.".to_string()),
            ("assistant".to_string(), "Noted.".to_string()),
        ]
    );
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
}
