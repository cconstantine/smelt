//! `ask_parent` (SME-238): a child asks its parent and waits; the parent's
//! next `message_agent` to it is the answer.

use super::*;

const ASKED: &str = "Which file?";

/// A mock upstream: each agent's requests get `agent_replies` in order
/// (the last repeated), anyone else's "Noted.".
async fn agent_mock(pool: &PgPool, agent_replies: Vec<String>) -> Arc<Mutex<Vec<serde_json::Value>>> {
    db::test_support::start_ids_clear_of_other_runs(pool).await.expect("ids clear of other runs");
    let served = Arc::new(Mutex::new(std::collections::HashMap::<i64, usize>::new()));
    mock_with(pool, move |request| {
        let body = match agent_of(request) {
            Some(agent) => {
                let mut served = served.lock().expect("the counts");
                let i = served.entry(agent).or_default();
                *i += 1;
                agent_replies[(*i - 1).min(agent_replies.len() - 1)].clone()
            }
            None => text_reply("Noted."),
        };
        Box::pin(async move { body })
    })
    .await
}

fn ask_reply() -> String {
    tool_call_reply("ask_parent", serde_json::json!({"question": ASKED}))
}

/// A reply calling each of `calls` (`(id, name, input)`) in one message.
fn calls_reply(calls: &[(&str, &str, serde_json::Value)]) -> String {
    let mut events = vec![("message_start".to_string(), r#"{"type":"message_start"}"#.to_string())];
    for (index, (id, name, input)) in calls.iter().enumerate() {
        let start = serde_json::json!({
            "type": "content_block_start", "index": index,
            "content_block": {"type": "tool_use", "id": id, "name": name, "input": {}}
        });
        let delta = serde_json::json!({
            "type": "content_block_delta", "index": index,
            "delta": {"type": "input_json_delta", "partial_json": input.to_string()}
        });
        events.push(("content_block_start".to_string(), start.to_string()));
        events.push(("content_block_delta".to_string(), delta.to_string()));
        events.push(("content_block_stop".to_string(), format!(r#"{{"type":"content_block_stop","index":{index}}}"#)));
    }
    events.push(("message_delta".to_string(), r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#.to_string()));
    events.push(("message_stop".to_string(), r#"{"type":"message_stop"}"#.to_string()));
    events.iter().map(|(event, data)| format!("event: {event}\ndata: {data}\n\n")).collect()
}

async fn message(pool: &PgPool, from: i64, to: serde_json::Value, text: &str) -> Result<String, String> {
    crate::anthropic::tools::execute(pool, from, "toolu_m", "message_agent", &serde_json::json!({"to": to, "text": text}))
        .await
}

/// A child of `root` whose turn (the user's "Work.") ended asking its
/// parent `ASKED`; the parent's own turn for the question is over too.
async fn asking_child(pool: &PgPool, root: i64, requests: &Mutex<Vec<serde_json::Value>>) -> i64 {
    let child = active_child(pool, root).await;
    crate::turn::start_turn(pool.clone(), child, "Work.".to_string()).await.expect("the send");
    eventually("the child asked and its turn ended", || async {
        !crate::turn::turn_running(child)
            && db::get_pending_question(pool, child).await.expect("read").is_some_and(|q| q.asked_of == "parent")
    })
    .await;
    let asked = format!("Question from agent #{child}, ");
    eventually("the parent read the question", || async {
        !crate::turn::turn_running(root)
            && requests_for(requests, None).iter().any(|r| r.to_string().contains(&asked))
    })
    .await;
    child
}

/// Every text and tool result after a request's last assistant message.
fn since_last_reply(request: &serde_json::Value) -> Vec<String> {
    let messages = request["messages"].as_array().expect("messages");
    let from = messages.iter().rposition(|m| m["role"] == "assistant").map_or(0, |i| i + 1);
    messages[from..]
        .iter()
        .flat_map(|m| match &m["content"] {
            serde_json::Value::Array(blocks) => blocks
                .iter()
                .map(|b| match b["type"].as_str() {
                    Some("tool_result") => format!("tool_result: {}", b["content"].as_str().unwrap_or_default()),
                    _ => b["text"].as_str().unwrap_or_default().to_string(),
                })
                .collect(),
            other => vec![other.as_str().unwrap_or_default().to_string()],
        })
        .collect()
}

async fn status_of(pool: &PgPool, parent: i64, child: i64) -> String {
    let listed: serde_json::Value = serde_json::from_str(&list_agents(pool, parent).await.expect("list_agents")).expect("JSON");
    listed["agents"]
        .as_array()
        .expect("agents")
        .iter()
        .find(|agent| agent["id"] == child)
        .and_then(|agent| agent["status"].as_str())
        .expect("the child is listed")
        .to_string()
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_childs_question_waits_for_its_parent_not_the_user(pool: PgPool) {
    let requests = agent_mock(&pool, vec![ask_reply(), text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = asking_child(&pool, root, &requests).await;

    assert_eq!(status_of(&pool, root, child).await, "waiting for its parent");
    assert!(!db::list_waiting_conversations(&pool).await.expect("list").contains(&child), "the sidebar shows it waiting");
    assert_eq!(crate::turn::pending_question(&pool, child).await.expect("read"), None, "the card shows it");
    assert!(
        transcript(&pool, root).await.contains(&(
            "user".to_string(),
            format!("Question from agent #{child}, which waits for your answer (reply with message_agent to {child}):\n\n{ASKED}")
        )),
        "{:?}",
        transcript(&pool, root).await
    );
    let notices = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1 AND kind = 'notice'")
        .bind(root)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(notices, 0, "a child waiting for its parent didn't end its turn without a report");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_parents_reply_answers_and_wakes_the_child(pool: PgPool) {
    let requests = agent_mock(&pool, vec![ask_reply(), text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = asking_child(&pool, root, &requests).await;

    assert_eq!(
        message(&pool, root, serde_json::json!(child), "hello.txt").await,
        Ok(format!("Agent #{child} was waiting for your answer: it has it now."))
    );
    eventually("the child took the answer", || async {
        requests_for(&requests, Some(child)).len() == 2 && !crate::turn::turn_running(child)
    })
    .await;
    assert_eq!(
        since_last_reply(&requests_for(&requests, Some(child))[1]),
        vec!["tool_result: Your parent answered:\n\nhello.txt".to_string()]
    );
    let queued = sqlx::query_scalar::<_, i64>("SELECT count(*) FROM agent_messages WHERE to_conversation_id = $1")
        .bind(child)
        .fetch_one(&pool)
        .await
        .expect("count");
    assert_eq!(queued, 0, "the answer isn't also queued as a message");
    assert_eq!(status_of(&pool, root, child).await, "idle");
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_user_writing_in_the_child_takes_the_question(pool: PgPool) {
    let requests = agent_mock(&pool, vec![ask_reply(), text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = asking_child(&pool, root, &requests).await;

    crate::turn::start_turn(pool.clone(), child, "Use a.txt.".to_string()).await.expect("the send");
    eventually("the child's turn is over", || async {
        requests_for(&requests, Some(child)).len() == 2 && !crate::turn::turn_running(child)
    })
    .await;
    assert_eq!(
        since_last_reply(&requests_for(&requests, Some(child))[1]),
        vec![format!("tool_result: {}", crate::questions::NOT_ANSWERED_BY_PARENT), "Use a.txt.".to_string()]
    );
    // The parent's reply, later, is a plain message.
    assert_eq!(message(&pool, root, serde_json::json!(child), "b.txt").await, Ok(format!("Sent to agent #{child}.")));
    eventually("the child read it", || async {
        transcript(&pool, child).await.iter().any(|(_, text)| text == "Message from your parent:\n\nb.txt")
            && !crate::turn::turn_running(child)
    })
    .await;
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_the_card_cant_answer_a_parents_question(pool: PgPool) {
    let requests = agent_mock(&pool, vec![ask_reply(), text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = asking_child(&pool, root, &requests).await;

    let answers = vec![crate::questions::QuestionAnswer { selected: vec![], other: Some("forged".to_string()) }];
    let refused = crate::turn::answer_question(pool.clone(), child, "toolu_ask_parent".to_string(), answers).await;
    assert!(
        crate::api::chat::chat_error_text(&refused.expect_err("refused")).contains(crate::turn::ALREADY_ANSWERED)
    );
    let stored = db::get_pending_question(&pool, child).await.expect("read").expect("still waiting");
    assert_eq!(stored.answer, None);
    assert_eq!(requests_for(&requests, Some(child)).len(), 1, "no turn ran");
}

/// The parent's reply and the user's message race for the question, held
/// on its row: whichever queued first takes it. Reply first: the user's
/// message carries the answer. User first: the reply is a plain message.
/// Never both, never neither.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_reply_racing_the_users_message(pool: PgPool) {
    let requests = agent_mock(&pool, vec![ask_reply(), text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    for reply_first in [true, false] {
        let child = asking_child(&pool, root, &requests).await;
        let mut holding = pool.begin().await.expect("begin");
        sqlx::query("SELECT 1 FROM pending_questions WHERE conversation_id = $1 FOR UPDATE")
            .bind(child)
            .execute(&mut *holding)
            .await
            .expect("hold the question");
        let waiters = || async {
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM pg_stat_activity WHERE datname = current_database() AND wait_event_type = 'Lock'",
            )
            .fetch_one(&pool)
            .await
            .expect("pg_stat_activity")
        };
        let reply = || {
            let pool = pool.clone();
            tokio::spawn(async move { message(&pool, root, serde_json::json!(child), "hello.txt").await })
        };
        let send = || {
            let pool = pool.clone();
            tokio::spawn(async move { crate::turn::start_turn(pool, child, "Use a.txt.".to_string()).await.is_ok() })
        };
        let (replied, sent) = if reply_first {
            let replied = reply();
            eventually("the reply waits", || async { waiters().await >= 1 }).await;
            let sent = send();
            (replied, sent)
        } else {
            let sent = send();
            eventually("the user's message waits", || async { waiters().await >= 1 }).await;
            (reply(), sent)
        };
        eventually("both wait", || async { waiters().await >= 2 }).await;
        holding.rollback().await.expect("release the question");
        let replied = tokio::time::timeout(Duration::from_secs(10), replied).await.expect("no deadlock").expect("task");
        assert!(tokio::time::timeout(Duration::from_secs(10), sent).await.expect("no deadlock").expect("task"));
        eventually("the child's turns are over", || async {
            !crate::turn::turn_running(child) && db::get_pending_question(&pool, child).await.expect("read").is_none()
        })
        .await;
        let texts: Vec<String> = transcript(&pool, child).await.into_iter().map(|(_, text)| text).collect();
        let results = sqlx::query_scalar::<_, String>("SELECT content FROM messages WHERE conversation_id = $1 ORDER BY id")
            .bind(child)
            .fetch_all(&pool)
            .await
            .expect("messages")
            .join("\n");
        let answered = results.contains("Your parent answered:");
        let as_message = texts.iter().any(|t| t == "Message from your parent:\n\nhello.txt");
        if reply_first {
            assert_eq!(replied, Ok(format!("Agent #{child} was waiting for your answer: it has it now.")));
            assert!(answered && !as_message, "{results}");
        } else {
            assert_eq!(replied, Ok(format!("Sent to agent #{child}.")));
            assert!(!answered && as_message, "{results}");
            assert!(results.contains("hasn't answered"), "{results}");
        }
        eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
    }
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_ask_parent_and_ask_user_in_one_reply(pool: PgPool) {
    let both = calls_reply(&[
        ("toolu_p", "ask_parent", serde_json::json!({"question": ASKED})),
        ("toolu_u", "ask_user", serde_json::json!({"questions": [{"question": "Go?", "header": "Go", "options": []}]})),
    ]);
    let requests = agent_mock(&pool, vec![both, text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = asking_child(&pool, root, &requests).await;

    let saved = sqlx::query_scalar::<_, String>("SELECT content FROM messages WHERE conversation_id = $1 ORDER BY id")
        .bind(child)
        .fetch_all(&pool)
        .await
        .expect("messages")
        .join("\n");
    assert!(saved.contains("toolu_u") && saved.contains("ask one question at a time"), "{saved}");
    let stored = db::get_pending_question(&pool, child).await.expect("read").expect("a question");
    assert_eq!((stored.tool_use_id.as_str(), stored.asked_of.as_str()), ("toolu_p", "parent"));
}

/// smelt restarts after the parent's answer is recorded and before the
/// child's wake runs: the sweep wakes it, and it takes the answer.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_an_answer_recorded_before_a_restart_wakes_the_child(pool: PgPool) {
    let requests = agent_mock(&pool, vec![ask_reply(), text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = asking_child(&pool, root, &requests).await;
    let answered = db::message_agent(&pool, root, db::Recipient::Child(child), "your parent", "hello.txt")
        .await
        .expect("record it");
    assert_eq!(answered, db::MessageOutcome::Answered(child));

    restart(&pool, &[root, child]).await;
    assert_eq!(requests_for(&requests, Some(child)).len(), 2, "the sweep woke the child");
    assert_eq!(
        since_last_reply(&requests_for(&requests, Some(child))[1]),
        vec!["tool_result: Your parent answered:\n\nhello.txt".to_string()]
    );
    eventually("every turn is over", || async { !crate::turn::turn_running(root) && !crate::turn::turn_running(child) }).await;
}

/// The user paused the child while it waited: the answer is recorded with
/// no turn, and the user's next message carries it first.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_paused_child_waiting_for_its_parent(pool: PgPool) {
    let requests = agent_mock(&pool, vec![ask_reply(), text_reply("Noted.")]).await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = asking_child(&pool, root, &requests).await;
    user_stop(&pool, child).await.expect("the user's Stop");

    assert_eq!(
        message(&pool, root, serde_json::json!(child), "hello.txt").await,
        Ok(format!("Agent #{child} was waiting for your answer: it has it now."))
    );
    crate::turn::notify(&pool, child, vec![crate::turn::Notice::Wake]).await.expect("the wake");
    assert_eq!(requests_for(&requests, Some(child)).len(), 1, "a paused child ran a turn");
    crate::turn::start_turn(pool.clone(), child, "Go on.".to_string()).await.expect("the send");
    eventually("the child's turn is over", || async {
        requests_for(&requests, Some(child)).len() == 2 && !crate::turn::turn_running(child)
    })
    .await;
    assert_eq!(
        since_last_reply(&requests_for(&requests, Some(child))[1]),
        vec!["tool_result: Your parent answered:\n\nhello.txt".to_string(), "Go on.".to_string()]
    );
    eventually("every turn is over", || async { !crate::turn::turn_running(root) }).await;
}

/// Review 1, finding 1. Interleaving: the parent's model call is held;
/// the child asks meanwhile, so the question waits undelivered in the
/// parent's queue; the held reply calls `message_agent` to the child
/// without having read it. That message isn't the answer: it's queued as
/// a plain message, the question stays open, and the parent reads it at
/// its next step.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_message_sent_before_the_parent_read_the_question_is_not_the_answer(pool: PgPool) {
    db::test_support::start_ids_clear_of_other_runs(&pool).await.expect("ids clear of other runs");
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let child_id = Arc::new(Mutex::new(0_i64));
    let served = Arc::new(Mutex::new(std::collections::HashMap::<Option<i64>, usize>::new()));
    let requests = mock_with(&pool, {
        let (gate, child_id) = (gate.clone(), child_id.clone());
        move |request| {
            let agent = agent_of(request);
            let n = {
                let mut served = served.lock().expect("the counts");
                let n = served.entry(agent).or_default();
                *n += 1;
                *n
            };
            let (gate, child) = (gate.clone(), *child_id.lock().expect("the child's id"));
            Box::pin(async move {
                match (agent, n) {
                    (Some(_), 1) => ask_reply(),
                    (None, 1) => {
                        gate.acquire().await.expect("the gate").forget();
                        tool_call_reply("message_agent", serde_json::json!({"to": child, "text": "also run the linter"}))
                    }
                    _ => text_reply("Noted."),
                }
            })
        }
    })
    .await;
    let root = db::create_conversation(&pool).await.expect("create conversation").id;
    let child = active_child(&pool, root).await;
    *child_id.lock().expect("the child's id") = child;
    crate::turn::start_turn(pool.clone(), root, "Work.".to_string()).await.expect("the parent's send");
    eventually("the parent's call is held", || async { requests_for(&requests, None).len() == 1 }).await;
    crate::turn::start_turn(pool.clone(), child, "Work.".to_string()).await.expect("the child's send");
    eventually("the child asked", || async {
        !crate::turn::turn_running(child) && db::get_pending_question(&pool, child).await.expect("read").is_some()
    })
    .await;

    gate.add_permits(1);
    eventually("the parent's turn is over", || async {
        !crate::turn::turn_running(root) && requests_for(&requests, None).len() >= 2
    })
    .await;
    let parent_results = sqlx::query_scalar::<_, String>("SELECT content FROM messages WHERE conversation_id = $1 ORDER BY id")
        .bind(root)
        .fetch_all(&pool)
        .await
        .expect("messages")
        .join("\n");
    assert!(parent_results.contains(&format!("Sent to agent #{child}.")), "{parent_results}");
    assert!(parent_results.contains(&format!("Question from agent #{child}, which waits")), "{parent_results}");
    let stored = db::get_pending_question(&pool, child).await.expect("read").expect("still open");
    assert_eq!(stored.answer, None, "the unprompted message was taken as the answer");
    eventually("every turn is over", || async { !crate::turn::turn_running(child) }).await;
}
