//! The terminal tools: ownership, busy refusals, and the idle notices (SME-76, SME-79).

use super::*;

/// SME-51 B2: terminal and command ids are sequential or visible,
/// so each tool checks the terminal or command is the calling
/// conversation's before touching it.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_another_conversations_terminal_and_command_are_unknown(pool: sqlx::PgPool) {
    let owner = db::create_conversation(&pool).await.expect("conversation");
    let other = db::create_conversation(&pool).await.expect("conversation");
    let pod = db::create_sandbox_pod(&pool, owner.id).await.expect("pod");
    let terminal = db::create_sandbox_terminal(&pool, pod.id).await.expect("terminal");
    db::create_terminal_command(&pool, owner.id, terminal.id, "cmd-b2", "ls", 300)
        .await
        .expect("command");
    let command = serde_json::json!({"command_id": "cmd-b2"});
    let term = serde_json::json!({"terminal_id": terminal.id});

    // The owner sees them.
    execute(&pool, owner.id, "t1", "terminal_command_status", &command)
        .await
        .expect("the owner can read its command");
    let listed = execute(&pool, owner.id, "t2", "list_commands", &term).await.expect("list");
    assert!(listed.contains("cmd-b2"), "{listed}");

    // Read-only ones first: they return data rather than reaching
    // for a pod.
    for (tool, input, unknown) in [
        ("terminal_command_status", command.clone(), "unknown command id"),
        ("read_terminal_output", command.clone(), "unknown command id"),
        ("list_commands", term.clone(), "unknown terminal id"),
        ("send_signal", serde_json::json!({"command_id": "cmd-b2", "signal": "INT"}), "unknown command id"),
        ("run_terminal_command", serde_json::json!({"terminal_id": terminal.id, "command": "id"}), "unknown terminal id"),
        ("terminate_terminal", term.clone(), "unknown terminal id"),
    ] {
        let result = execute(&pool, other.id, "t3", tool, &input).await;
        assert!(
            result.as_ref().is_err_and(|e| e.contains(unknown)),
            "{tool} from another conversation: {result:?}"
        );
    }
    assert_eq!(
        db::list_terminal_commands(&pool, terminal.id, 10).await.expect("list").len(),
        1,
        "no command was recorded against the other conversation"
    );
}

#[test]
fn test_busy_terminal_message_names_the_command_its_age_and_the_ways_out() {
    let started = chrono::NaiveDate::from_ymd_opt(2026, 9, 30)
        .and_then(|d| d.and_hms_opt(15, 6, 0))
        .expect("a valid time");
    let command = db::TerminalCommand {
        id: 1,
        conversation_id: 1,
        terminal_id: 9,
        command_id: "cmd-a07c".to_string(),
        command: format!("cd /tmp && curl -fsSL {}", "x".repeat(200)),
        status: "running".to_string(),
        exit_code: None,
        notified_at: None,
        idle_notice_secs: 300,
        touched_at: Some(started),
        idle_notice_at: None,
        created_at: started,
        finished_at: None,
    };
    let message = busy_terminal_message(&command, started + chrono::Duration::minutes(59));
    for wanted in ["cmd-a07c", "cd /tmp && curl -fsSL", "59 min", "send_signal", "create_terminal"] {
        assert!(message.contains(wanted), "{wanted:?} missing from: {message}");
    }
    assert!(!message.contains(&"x".repeat(100)), "the command should be shortened: {message}");
}

/// SME-76: both refusals a busy terminal gives say which command
/// holds it, not just that one does.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_a_busy_terminal_names_its_command_to_run_and_terminate(pool: sqlx::PgPool) {
    let owner = db::create_conversation(&pool).await.expect("conversation");
    let pod = db::create_sandbox_pod(&pool, owner.id).await.expect("pod");
    let terminal = db::create_sandbox_terminal(&pool, pod.id).await.expect("terminal");
    db::create_terminal_command(&pool, owner.id, terminal.id, "cmd-busy", "sleep 600", 300)
        .await
        .expect("command");
    for (tool, input) in [
        ("run_terminal_command", serde_json::json!({"terminal_id": terminal.id, "command": "id"})),
        ("terminate_terminal", serde_json::json!({"terminal_id": terminal.id})),
    ] {
        let message = execute(&pool, owner.id, "t1", tool, &input)
            .await
            .expect_err("a busy terminal refuses");
        assert!(
            message.contains("cmd-busy") && message.contains("sleep 600"),
            "{tool}: {message}"
        );
    }
}

/// SME-76: one minified-JSON line could be most of a result.
#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_read_terminal_output_stops_at_the_budget_with_a_next_offset(pool: sqlx::PgPool) {
    let owner = db::create_conversation(&pool).await.expect("conversation");
    let pod = db::create_sandbox_pod(&pool, owner.id).await.expect("pod");
    let terminal = db::create_sandbox_terminal(&pool, pod.id).await.expect("terminal");
    db::create_terminal_command(&pool, owner.id, terminal.id, "cmd-big", "cat big", 300)
        .await
        .expect("command");
    db::append_terminal_event(&pool, "cmd-big", "stdout", 1, &"z".repeat(5_000)).await.expect("event");
    for seq in 2..=41 {
        let line = format!("{seq:02}{}", "y".repeat(1_500));
        db::append_terminal_event(&pool, "cmd-big", "stdout", seq, &line).await.expect("event");
    }
    let read = |offset: i64| {
        let pool = pool.clone();
        async move {
            let out = execute(&pool, owner.id, "t1", "read_terminal_output", &serde_json::json!({"command_id": "cmd-big", "offset": offset}))
                .await
                .expect("read");
            serde_json::from_str::<Value>(&out).expect("json")
        }
    };
    let first = read(0).await;
    assert_eq!(first["truncated"], true, "{}", &first.to_string()[..200]);
    let next = first["next_offset"].as_i64().expect("a next_offset");
    assert_eq!(next, first["returned"].as_i64().expect("returned"));
    assert!(first["lines"][0]["data"].as_str().expect("data").ends_with("[line cut at 2000 chars]"));
    let second = read(next).await;
    let expected = format!("{:02}", next + 1);
    assert!(
        second["lines"][0]["data"].as_str().expect("data").starts_with(&expected),
        "the next read starts at line {expected}"
    );
}

// --- Idle notices (SME-79) ---

/// A running command, with the pod and terminal rows it "ran" in, created
/// straight in the database: the idle-notice bookkeeping these tests check
/// never reaches the sandbox itself.
async fn idle_command(pool: &PgPool, conversation_id: i64, command_id: &str) {
    let pod = db::create_sandbox_pod(pool, conversation_id).await.expect("pod");
    let terminal = db::create_sandbox_terminal(pool, pod.id).await.expect("terminal");
    db::create_terminal_command(pool, conversation_id, terminal.id, command_id, "sleep 100", 300)
        .await
        .expect("create terminal command");
}

/// Leave a command looking untouched for `secs`, by moving its start back.
async fn leave_untouched(pool: &PgPool, command_id: &str, secs: i64) {
    sqlx::query("UPDATE terminal_commands SET created_at = now() - make_interval(secs => $1) WHERE command_id = $2")
        .bind(secs as f64)
        .bind(command_id)
        .execute(pool)
        .await
        .expect("backdate created_at");
}

async fn command_row(pool: &PgPool, command_id: &str) -> db::TerminalCommand {
    db::get_terminal_command(pool, command_id).await.expect("row").expect("row")
}

/// How long ago the row says the command was touched.
fn touched_ago(command: &db::TerminalCommand) -> i64 {
    (chrono::Utc::now().naive_utc() - command.touched_at.expect("touched")).num_seconds()
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_terminal_command_status_shows_the_idle_clock_and_touches_it(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    idle_command(&pool, conversation.id, "idle-status").await;
    leave_untouched(&pool, "idle-status", 420).await;

    let out = execute(
        &pool,
        conversation.id,
        "toolu_s",
        "terminal_command_status",
        &serde_json::json!({"command_id": "idle-status"}),
    )
    .await
    .expect("status");
    let status: Value = serde_json::from_str(&out).expect("status JSON");
    assert_eq!(status["status"], "running");
    assert_eq!(status["idle_notice_seconds"], 300, "the interval in force");
    // The time it had gone untouched *before* this check — touching first
    // would make this always 0.
    assert!(status["untouched_for_seconds"].as_i64().unwrap_or_default() >= 419, "{status}");

    let row = command_row(&pool, "idle-status").await;
    assert!((0..=2).contains(&touched_ago(&row)), "checking is touching: {}", touched_ago(&row));
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_terminal_command_status_has_no_untouched_time_once_it_is_running_no_more(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    idle_command(&pool, conversation.id, "idle-finished").await;
    leave_untouched(&pool, "idle-finished", 420).await;
    db::mark_terminal_command_finished(&pool, "idle-finished", 0).await.expect("mark finished");

    let out = execute(
        &pool,
        conversation.id,
        "toolu_s2",
        "terminal_command_status",
        &serde_json::json!({"command_id": "idle-finished"}),
    )
    .await
    .expect("status");
    let status: Value = serde_json::from_str(&out).expect("status JSON");
    assert_eq!(status["untouched_for_seconds"], Value::Null, "a finished command isn't waiting on anyone");
    assert_eq!(status["idle_notice_seconds"], 300, "the interval it had is still the row's");
    assert!(
        command_row(&pool, "idle-finished").await.touched_at.is_none(),
        "the touch is for running commands only"
    );
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_read_terminal_output_touches_the_command(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    idle_command(&pool, conversation.id, "idle-read").await;
    leave_untouched(&pool, "idle-read", 420).await;

    execute(
        &pool,
        conversation.id,
        "toolu_r",
        "read_terminal_output",
        &serde_json::json!({"command_id": "idle-read"}),
    )
    .await
    .expect("read");

    let row = command_row(&pool, "idle-read").await;
    assert!((0..=2).contains(&touched_ago(&row)), "reading is touching: {}", touched_ago(&row));
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_set_idle_notice_retimes_a_running_command(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    idle_command(&pool, conversation.id, "idle-set").await;
    leave_untouched(&pool, "idle-set", 420).await;

    let out = execute(
        &pool,
        conversation.id,
        "toolu_n",
        "set_idle_notice",
        &serde_json::json!({"command_id": "idle-set", "idle_notice_seconds": 1800}),
    )
    .await
    .expect("set notice");
    assert_eq!(out, "idle notice for idle-set set to 30 minutes");
    let row = command_row(&pool, "idle-set").await;
    assert_eq!(row.idle_notice_secs, 1800);
    assert!((0..=2).contains(&touched_ago(&row)), "re-timing is a touch, so it counts from now");

    // 0 is the way out: no more notices for this command.
    let out = execute(
        &pool,
        conversation.id,
        "toolu_n1",
        "set_idle_notice",
        &serde_json::json!({"command_id": "idle-set", "idle_notice_seconds": 0}),
    )
    .await
    .expect("turn them off");
    assert_eq!(out, "idle notice for idle-set turned off");
    assert_eq!(command_row(&pool, "idle-set").await.idle_notice_secs, 0);
}

#[sqlx::test(migrator = "crate::test_lock::MIGRATOR")]
async fn test_set_idle_notice_refuses_a_command_that_is_not_running(pool: PgPool) {
    let conversation = db::create_conversation(&pool).await.expect("conversation");
    idle_command(&pool, conversation.id, "idle-done").await;
    db::mark_terminal_command_finished(&pool, "idle-done", 0).await.expect("mark finished");

    let result = execute(
        &pool,
        conversation.id,
        "toolu_f",
        "set_idle_notice",
        &serde_json::json!({"command_id": "idle-done", "idle_notice_seconds": 60}),
    )
    .await;
    assert!(matches!(&result, Err(m) if m.contains("not running")), "{result:?}");
    assert_eq!(command_row(&pool, "idle-done").await.idle_notice_secs, 300, "its interval is untouched");

    // An unknown command — another conversation's or nobody's — is the
    // plain unknown-id refusal, not an idle one.
    let result = execute(
        &pool,
        conversation.id,
        "toolu_u",
        "set_idle_notice",
        &serde_json::json!({"command_id": "no-such-command", "idle_notice_seconds": 60}),
    )
    .await;
    assert!(matches!(&result, Err(m) if m.contains("unknown command id")), "{result:?}");
}

#[test]
fn test_notice_intervals_are_read_from_their_field_or_refused_naming_it() {
    // Absent, or an explicit null: no interval given, so the default applies.
    assert_eq!(optional_notice_secs(&serde_json::json!({}), "x").unwrap(), None);
    assert_eq!(optional_notice_secs(&serde_json::json!({"x": null}), "x").unwrap(), None);
    // 0 is a real interval — never — and so is any whole number above it.
    assert_eq!(optional_notice_secs(&serde_json::json!({"x": 0}), "x").unwrap(), Some(0));
    assert_eq!(optional_notice_secs(&serde_json::json!({"x": 30}), "x").unwrap(), Some(30));
    assert_eq!(required_notice_secs(&serde_json::json!({"x": 0}), "x").unwrap(), 0);

    for bad in [
        serde_json::json!(-5),
        serde_json::json!("30"),
        serde_json::json!(1.5),
        serde_json::json!(true),
    ] {
        let input = serde_json::json!({"idle_notice_seconds": bad});
        let message = optional_notice_secs(&input, "idle_notice_seconds").expect_err(&input.to_string());
        assert!(message.contains("idle_notice_seconds"), "{message} should name the field");
        assert!(required_notice_secs(&input, "idle_notice_seconds").is_err(), "{input} should be refused");
    }
    // Required: missing is refused, and so is anything the optional one is.
    let message = required_notice_secs(&serde_json::json!({}), "idle_notice_seconds").expect_err("missing");
    assert!(message.contains("idle_notice_seconds"), "{message} should name the field");
}

