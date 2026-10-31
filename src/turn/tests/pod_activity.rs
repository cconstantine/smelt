//! SME-78: which turns count as pod activity. A turn that brought the model
//! something new counts at its end; a wake for SME-79's idle notice alone
//! doesn't (the user's answer 2).

use super::*;

async fn fresh_conversation(pool: &PgPool) -> i64 {
    db::test_support::start_ids_clear_of_other_runs(pool).await.expect("ids clear of other runs");
    db::create_conversation(pool).await.expect("create conversation").id
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_users_turn_notes_activity_at_its_end_not_its_start(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation_id = fresh_conversation(&pool).await;
    start_mock_upstream(&pool, vec![text_reply_body("Done.")]).await;
    let (reached, release) = test_hooks::pause_after_message_saved(conversation_id);

    let turn = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation_id, hello()).await }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), reached)
        .await
        .expect("the turn saves its message")
        .expect("hook");
    let mid_turn = crate::sandbox::activity_noted(conversation_id);
    let _ = release.send(());
    let result = tokio::time::timeout(std::time::Duration::from_secs(10), turn)
        .await
        .expect("the turn ends")
        .expect("join");

    assert!(result.is_ok(), "{result:?}");
    assert!(!mid_turn, "noted while the turn was still running");
    assert!(crate::sandbox::activity_noted(conversation_id), "the turn's end is activity");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_for_an_idle_notice_alone_notes_nothing(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation_id = fresh_conversation(&pool).await;
    let pod = db::create_sandbox_pod(&pool, conversation_id).await.expect("pod");
    let terminal = db::create_sandbox_terminal(&pool, pod.id).await.expect("terminal");
    let command = format!("idle-{conversation_id}");
    db::create_terminal_command(&pool, conversation_id, terminal.id, &command, "npm run dev", 300)
        .await
        .expect("command");
    sqlx::query("UPDATE terminal_commands SET created_at = now() - interval '10 minutes' WHERE command_id = $1")
        .bind(&command)
        .execute(&pool)
        .await
        .expect("leave it untouched for ten minutes");
    start_mock_upstream(&pool, vec![text_reply_body("Still going; leaving it.")]).await;

    let saved = wake_conversation(&pool, conversation_id).await.expect("the wake runs");

    assert!(
        saved.iter().any(|m| m.content.contains("is still running")),
        "the wake saved the idle notice: {saved:?}"
    );
    assert!(!crate::sandbox::activity_noted(conversation_id), "an idle notice's turn isn't activity");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_that_drains_a_finished_command_notes_activity(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation_id = fresh_conversation(&pool).await;
    unnotified_finished_command(&pool, conversation_id, &format!("done-{conversation_id}")).await;
    start_mock_upstream(&pool, vec![text_reply_body("Noted.")]).await;

    let saved = wake_conversation(&pool, conversation_id).await.expect("the wake runs");

    assert!(!saved.is_empty(), "the wake drained the notice");
    assert!(crate::sandbox::activity_noted(conversation_id), "a finished command's notice is activity");
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_wake_that_drains_an_agent_message_notes_activity(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation_id = fresh_conversation(&pool).await;
    sqlx::query(
        "INSERT INTO agent_messages (to_conversation_id, from_label, kind, body) VALUES ($1, 'child', 'report', 'All done.')",
    )
    .bind(conversation_id)
    .execute(&pool)
    .await
    .expect("a report waiting");
    start_mock_upstream(&pool, vec![text_reply_body("Thanks.")]).await;

    let saved = wake_conversation(&pool, conversation_id).await.expect("the wake runs");

    assert!(saved.iter().any(|m| m.content.contains("All done.")), "the report was delivered: {saved:?}");
    assert!(crate::sandbox::activity_noted(conversation_id), "an agent's report is activity");
}

const IDLE_LINE: &str = "- Your sandbox pod is stopped after 60 minutes with no activity and no command running; \
    a running command keeps it, but a process left in the background (&, nohup, a detached container) doesn't. \
    /workspace, Docker's data and volumes are kept.\n";
const FINISH_LINE: &str = "- Your sandbox pod is stopped when you finish, unless a command is still running; \
    /workspace and volumes are kept for if you're given more work.\n";

#[test]
fn test_the_environment_says_when_the_pod_is_stopped_on_idle() {
    let env = super::prompt::prompt_env();
    let prompt = system_prompt(&env);
    assert!(prompt.contains(IDLE_LINE), "{prompt}");
    assert!(!prompt.contains(FINISH_LINE), "only an agent finishes: {prompt}");

    let off = PromptEnvironment { idle_stop_minutes: None, ..super::prompt::prompt_env() };
    assert!(!system_prompt(&off).contains("with no activity"), "no idle line while idle stops are off");

    let three = PromptEnvironment { idle_stop_minutes: Some(3), ..super::prompt::prompt_env() };
    assert!(system_prompt(&three).contains("stopped after 3 minutes with no activity"), "the minutes in force");
}

#[test]
fn test_an_agents_environment_says_finish_stops_its_pod() {
    let agent = PromptEnvironment {
        agent: Some(AgentPrompt { id: 7, parent_id: 42, persona: None }),
        idle_stop_minutes: None,
        ..super::prompt::prompt_env()
    };
    assert!(system_prompt(&agent).contains(FINISH_LINE), "even with idle stops off");
}

/// A mock model whose replies each wait for a permit on `gate`; `seen` is
/// notified as each request arrives.
async fn gated_upstream(pool: &PgPool, gate: Arc<tokio::sync::Semaphore>, seen: Arc<tokio::sync::Notify>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind ephemeral port");
    let addr = listener.local_addr().expect("local addr");
    let app = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move || {
            let (gate, seen) = (gate.clone(), seen.clone());
            async move {
                seen.notify_one();
                gate.acquire().await.expect("the gate").forget();
                ([(axum::http::header::CONTENT_TYPE, "text/event-stream")], text_reply_body("ok"))
            }
        }),
    );
    tokio::spawn(async move {
        axum::serve(listener, app).await.ok();
    });
    crate::providers::test_support::add_mock_provider(pool, addr).await;
}

/// SME-78 review 1 (B1): a user's send queued behind an idle-notice wake
/// counts at its own end, and the wake ahead of it doesn't take its
/// activity. Forced order: the wake holds the lock in its model call while
/// the send is counted and queues; the wake ends; then the send runs.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_send_queued_behind_an_idle_notice_wake_counts_at_its_own_end(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation_id = fresh_conversation(&pool).await;
    let pod = db::create_sandbox_pod(&pool, conversation_id).await.expect("pod");
    let terminal = db::create_sandbox_terminal(&pool, pod.id).await.expect("terminal");
    let command = format!("idle-queued-{conversation_id}");
    db::create_terminal_command(&pool, conversation_id, terminal.id, &command, "npm run dev", 300)
        .await
        .expect("command");
    sqlx::query("UPDATE terminal_commands SET created_at = now() - interval '10 minutes' WHERE command_id = $1")
        .bind(&command)
        .execute(&pool)
        .await
        .expect("leave it untouched for ten minutes");
    let gate = Arc::new(tokio::sync::Semaphore::new(0));
    let seen = Arc::new(tokio::sync::Notify::new());
    gated_upstream(&pool, gate.clone(), seen.clone()).await;
    let within = std::time::Duration::from_secs(10);

    let wake = tokio::spawn({
        let pool = pool.clone();
        async move { wake_conversation(&pool, conversation_id).await }
    });
    tokio::time::timeout(within, seen.notified()).await.expect("the wake calls the model");
    // The send is counted while the wake holds the lock; let go, it marks
    // what it brings and queues for the lock.
    let (counted, go_on) = test_hooks::pause_after_turn_counted(conversation_id);
    let send = tokio::spawn({
        let pool = pool.clone();
        async move { run_turn(&pool, conversation_id, hello()).await }
    });
    tokio::time::timeout(within, counted).await.expect("the send is counted").expect("hook");
    let _ = go_on.send(());
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    gate.add_permits(1);
    let woke = tokio::time::timeout(within, wake).await.expect("the wake ends").expect("join");
    let noted_at_the_wakes_end = crate::sandbox::activity_noted(conversation_id);
    gate.add_permits(1);
    let sent = tokio::time::timeout(within, send).await.expect("the send ends").expect("join");

    assert!(woke.is_ok() && sent.is_ok(), "{woke:?} {sent:?}");
    assert!(!noted_at_the_wakes_end, "the idle-notice wake took the queued send's activity");
    assert!(crate::sandbox::activity_noted(conversation_id), "the send's own end is activity");
}

/// SME-78 review 1 (B2): a flag a turn left behind (one that panicked
/// before its end took it) doesn't make the next idle-notice wake count.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_stale_flag_from_an_earlier_turn_does_not_count(pool: PgPool) {
    let _guard = lock_turn_tests();
    let conversation_id = fresh_conversation(&pool).await;
    let pod = db::create_sandbox_pod(&pool, conversation_id).await.expect("pod");
    let terminal = db::create_sandbox_terminal(&pool, pod.id).await.expect("terminal");
    let command = format!("idle-stale-{conversation_id}");
    db::create_terminal_command(&pool, conversation_id, terminal.id, &command, "npm run dev", 300)
        .await
        .expect("command");
    sqlx::query("UPDATE terminal_commands SET created_at = now() - interval '10 minutes' WHERE command_id = $1")
        .bind(&command)
        .execute(&pool)
        .await
        .expect("leave it untouched for ten minutes");
    start_mock_upstream(&pool, vec![text_reply_body("Still going.")]).await;
    // The leftover: the conversation's state is held (a kept lock handle),
    // so the flag survives with no turn running.
    let held = crate::turn::conversation_lock(conversation_id);
    set_activity_owed(conversation_id);

    let saved = wake_conversation(&pool, conversation_id).await.expect("the wake runs");
    drop(held);

    assert!(saved.iter().any(|m| m.content.contains("is still running")), "{saved:?}");
    assert!(!crate::sandbox::activity_noted(conversation_id), "a stale flag counted for an idle-notice wake");
}
