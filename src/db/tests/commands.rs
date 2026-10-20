use super::*;

#[sqlx::test]
async fn test_create_terminal_command_starts_running(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    let command =
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-1", "echo hi", 300)
            .await
            .expect("create terminal command");
    assert_eq!(command.status, "running");
    assert_eq!(command.command, "echo hi");
    assert!(command.exit_code.is_none());
    assert!(command.finished_at.is_none());
    assert!(command.notified_at.is_none());
}

#[sqlx::test]
async fn test_terminal_command_is_running_reflects_the_single_in_flight_row(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    assert!(
        terminal_command_is_running(&pool, terminal_id)
            .await
            .expect("query should succeed")
            .is_none(),
        "nothing running yet"
    );

    let command =
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-2", "sleep 5", 300)
            .await
            .expect("create terminal command");

    let running = terminal_command_is_running(&pool, terminal_id)
        .await
        .expect("query should succeed")
        .expect("should find the running command");
    assert_eq!(running.command_id, command.command_id);

    mark_terminal_command_finished(&pool, &command.command_id, 0)
        .await
        .expect("mark finished");
    assert!(
        terminal_command_is_running(&pool, terminal_id)
            .await
            .expect("query should succeed")
            .is_none(),
        "should no longer be running once finished"
    );
}

#[sqlx::test]
async fn test_terminal_command_is_running_is_scoped_per_terminal(pool: PgPool) {
    // The whole point of moving this guard from conversation_id to
    // terminal_id: two terminals in flight at once, independently.
    let conversation = test_conversation(&pool).await;
    let terminal_a = test_terminal(&pool, conversation.id).await;
    let terminal_b = test_terminal(&pool, conversation.id).await;

    create_terminal_command(&pool, conversation.id, terminal_a, "cmd-a", "sleep 5", 300)
        .await
        .expect("create terminal command");

    assert!(
        terminal_command_is_running(&pool, terminal_a)
            .await
            .expect("query")
            .is_some(),
        "terminal_a has a running command"
    );
    assert!(
        terminal_command_is_running(&pool, terminal_b)
            .await
            .expect("query")
            .is_none(),
        "terminal_b should be unaffected by terminal_a's running command"
    );
}

#[sqlx::test]
async fn test_mark_terminal_command_finished_sets_status_and_exit_code(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    let command =
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-3", "false", 300)
            .await
            .expect("create terminal command");

    mark_terminal_command_finished(&pool, &command.command_id, 1)
        .await
        .expect("mark finished");

    let status = terminal_command_status(&pool, &command.command_id)
        .await
        .expect("query should succeed")
        .expect("row should exist");
    assert_eq!(status.status, "finished");
    assert_eq!(status.exit_code, Some(1));
}

#[sqlx::test]
async fn test_mark_terminal_command_lost_only_affects_running_rows(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    let command =
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-4", "sleep 100", 300)
            .await
            .expect("create terminal command");
    mark_terminal_command_finished(&pool, &command.command_id, 0)
        .await
        .expect("mark finished");

    // Already finished — marking it lost afterward should be a no-op,
    // not silently overwrite a real exit code with an unknown outcome.
    mark_terminal_command_lost(&pool, &command.command_id)
        .await
        .expect("mark lost should not error");

    let status = terminal_command_status(&pool, &command.command_id)
        .await
        .expect("query should succeed")
        .expect("row should exist");
    assert_eq!(status.status, "finished");
    assert_eq!(status.exit_code, Some(0));
}

#[sqlx::test]
async fn test_mark_terminal_command_lost_on_a_running_row(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    let command =
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-5", "sleep 100", 300)
            .await
            .expect("create terminal command");

    mark_terminal_command_lost(&pool, &command.command_id)
        .await
        .expect("mark lost");

    let status = terminal_command_status(&pool, &command.command_id)
        .await
        .expect("query should succeed")
        .expect("row should exist");
    assert_eq!(status.status, "lost");
    assert!(status.exit_code.is_none());
}

#[sqlx::test]
async fn test_append_terminal_event_and_read_terminal_output_orders_by_seq(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    let command = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-6", "echo", 300)
        .await
        .expect("create terminal command");

    // Interleaved on purpose — seq is the agent-assigned true order,
    // not insertion order, so this exercises that they can diverge.
    append_terminal_event(&pool, &command.command_id, "stdout", 1, "out1")
        .await
        .expect("append");
    append_terminal_event(&pool, &command.command_id, "stderr", 2, "err1")
        .await
        .expect("append");
    append_terminal_event(&pool, &command.command_id, "stdout", 3, "out2")
        .await
        .expect("append");

    let both = read_terminal_output(&pool, &command.command_id, &["stdout", "stderr"], 0, 10)
        .await
        .expect("read output");
    assert_eq!(
        both.iter().map(|l| l.data.as_str()).collect::<Vec<_>>(),
        vec!["out1", "err1", "out2"]
    );

    let stdout_only = read_terminal_output(&pool, &command.command_id, &["stdout"], 0, 10)
        .await
        .expect("read output");
    assert_eq!(
        stdout_only
            .iter()
            .map(|l| l.data.as_str())
            .collect::<Vec<_>>(),
        vec!["out1", "out2"]
    );

    // Same offset (0), different filter — proves line numbering is
    // relative to the requested stream set, not a stored absolute
    // position (the whole point of this design).
    assert_ne!(both[0].data, "out1".to_string().len().to_string()); // sanity: no accidental type confusion
    assert_eq!(both[0].data, "out1");
    assert_eq!(stdout_only[0].data, "out1");
    assert_eq!(both[1].data, "err1");
    // offset 1 against "both" is err1, but offset 1 against
    // "stdout" alone is out2 — different lines at the same offset.
    assert_eq!(stdout_only.get(1).map(|l| l.data.as_str()), Some("out2"));
}

#[sqlx::test]
async fn test_read_terminal_output_respects_offset_and_limit(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    let command =
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-7", "seq 5", 300)
            .await
            .expect("create terminal command");
    for i in 1..=5i64 {
        append_terminal_event(&pool, &command.command_id, "stdout", i, &format!("line{i}"))
            .await
            .expect("append");
    }

    let page = read_terminal_output(&pool, &command.command_id, &["stdout"], 1, 2)
        .await
        .expect("read output");
    assert_eq!(
        page.iter().map(|l| l.data.as_str()).collect::<Vec<_>>(),
        vec!["line2", "line3"]
    );
}

#[sqlx::test]
async fn test_terminal_command_status_counts_lines_per_stream(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    let command = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-8", "echo", 300)
        .await
        .expect("create terminal command");
    append_terminal_event(&pool, &command.command_id, "stdout", 1, "a")
        .await
        .expect("append");
    append_terminal_event(&pool, &command.command_id, "stdout", 2, "b")
        .await
        .expect("append");
    append_terminal_event(&pool, &command.command_id, "stderr", 3, "c")
        .await
        .expect("append");

    let status = terminal_command_status(&pool, &command.command_id)
        .await
        .expect("query should succeed")
        .expect("row should exist");
    assert_eq!(status.stdout_lines, 2);
    assert_eq!(status.stderr_lines, 1);
    assert_eq!(status.status, "running");
}

#[sqlx::test]
async fn test_terminal_command_status_returns_none_for_unknown_command(pool: PgPool) {
    let result = terminal_command_status(&pool, "no-such-command")
        .await
        .expect("query should succeed");
    assert!(result.is_none());
}

/// SME-91: a command's notice and its mark are one step, so a command
/// already notified (a stopped drain that got as far as the mark, or
/// another drain) never gets a second notice.
#[sqlx::test]
async fn test_a_command_already_notified_gets_no_second_notice(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    let command = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-n1", "true", 300)
        .await
        .expect("create");
    mark_terminal_command_finished(&pool, &command.command_id, 0)
        .await
        .expect("mark finished");
    let notice = [ContentBlock::Text { text: "cmd-n1 finished".to_string() }];

    let first = save_command_notice(&pool, conversation.id, "cmd-n1", &notice)
        .await
        .expect("first notice");
    assert!(first.is_some(), "the first notice is saved");
    let second = save_command_notice(&pool, conversation.id, "cmd-n1", &notice)
        .await
        .expect("second notice");
    assert!(second.is_none(), "a notified command gets no second notice");

    let messages = list_messages(&pool, conversation.id).await.expect("list");
    let notices = messages.iter().filter(|m| m.content.contains("cmd-n1 finished")).count();
    assert_eq!(notices, 1);
}

/// SME-91: the notice and the mark commit together: a command that
/// doesn't exist (nothing to mark) leaves no notice behind.
#[sqlx::test]
async fn test_a_notice_without_its_mark_isnt_saved(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let notice = [ContentBlock::Text { text: "nobody's notice".to_string() }];
    let saved = save_command_notice(&pool, conversation.id, "no-such-command", &notice)
        .await
        .expect("save");
    assert!(saved.is_none());
    let messages = list_messages(&pool, conversation.id).await.expect("list");
    assert!(messages.is_empty(), "no notice without its mark: {messages:?}");
}

#[sqlx::test]
async fn test_unnotified_finished_terminal_commands_matches_finished_and_lost_only(
    pool: PgPool,
) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    let _running =
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-9", "sleep 1", 300)
            .await
            .expect("create");
    let finished =
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-10", "true", 300)
            .await
            .expect("create");
    mark_terminal_command_finished(&pool, &finished.command_id, 0)
        .await
        .expect("mark finished");
    let lost =
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-11", "sleep 100", 300)
            .await
            .expect("create");
    mark_terminal_command_lost(&pool, &lost.command_id)
        .await
        .expect("mark lost");

    let unnotified = unnotified_finished_terminal_commands(&pool, conversation.id)
        .await
        .expect("query should succeed");
    let ids: Vec<_> = unnotified.iter().map(|c| c.command_id.as_str()).collect();
    assert!(ids.contains(&"cmd-10"));
    assert!(ids.contains(&"cmd-11"));
    assert!(
        !ids.contains(&"cmd-9"),
        "still-running command should not need notification"
    );

    mark_terminal_command_notified(&pool, &finished.command_id)
        .await
        .expect("mark notified");
    let remaining = unnotified_finished_terminal_commands(&pool, conversation.id)
        .await
        .expect("query should succeed");
    let ids: Vec<_> = remaining.iter().map(|c| c.command_id.as_str()).collect();
    assert!(!ids.contains(&"cmd-10"), "notified command should drop out");
    assert!(
        ids.contains(&"cmd-11"),
        "still-unnotified command should remain"
    );
}

#[sqlx::test]
async fn test_list_terminal_commands_is_most_recent_first_and_bounded(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    for i in 1..=5 {
        create_terminal_command(
            &pool,
            conversation.id,
            terminal_id,
            &format!("list-cmd-{i}"),
            "echo", 300,
        )
        .await
        .expect("create");
    }

    let limited = list_terminal_commands(&pool, terminal_id, 3)
        .await
        .expect("list should succeed");
    assert_eq!(limited.len(), 3);
    assert_eq!(
        limited
            .iter()
            .map(|c| c.command_id.as_str())
            .collect::<Vec<_>>(),
        vec!["list-cmd-5", "list-cmd-4", "list-cmd-3"]
    );
}

#[sqlx::test]
async fn test_delete_conversation_cascades_to_terminal_commands_and_events(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    let command = create_terminal_command(
        &pool,
        conversation.id,
        terminal_id,
        "cmd-cascade",
        "echo hi", 300,
    )
    .await
    .expect("create terminal command");
    append_terminal_event(&pool, &command.command_id, "stdout", 1, "hi")
        .await
        .expect("append");

    delete_conversation(&pool, conversation.id)
        .await
        .expect("delete conversation");

    let status = terminal_command_status(&pool, &command.command_id)
        .await
        .expect("query should succeed");
    assert!(
        status.is_none(),
        "terminal_commands (and its events, cascading further) should be gone with the conversation"
    );
}

#[sqlx::test]
async fn test_terminating_a_terminal_preserves_its_command_history(pool: PgPool) {
    // The whole reason terminate_terminal soft-deletes instead of
    // DELETEing (see SME-9's "How"): list_commands should still be
    // able to show what ran in a terminal that's since been torn down.
    let conversation = test_conversation(&pool).await;
    let pod = create_sandbox_pod(&pool, conversation.id)
        .await
        .expect("create pod");
    let terminal = create_sandbox_terminal(&pool, pod.id)
        .await
        .expect("create terminal");
    let command = create_terminal_command(
        &pool,
        conversation.id,
        terminal.id,
        "cmd-survives",
        "echo hi", 300,
    )
    .await
    .expect("create terminal command");
    mark_terminal_command_finished(&pool, &command.command_id, 0)
        .await
        .expect("mark finished");

    terminate_sandbox_terminal(&pool, terminal.id)
        .await
        .expect("terminate should succeed")
        .expect("terminal should exist");

    let status = terminal_command_status(&pool, &command.command_id)
        .await
        .expect("query should succeed");
    assert!(
        status.is_some(),
        "command history should survive terminate_terminal"
    );

    let history = list_terminal_commands(&pool, terminal.id, 10)
        .await
        .expect("list should still work against a terminated terminal");
    assert!(history.iter().any(|c| c.command_id == command.command_id));
}

// --- Idle notices (SME-79) ---

/// Move one column of a command's idle clock back `secs`, to the state it
/// would be in after sitting that long: `created_at` for a command nobody has
/// touched, `touched_at` for one that was checked, `idle_notice_at` for one
/// already told. The clock is the latest of the three, so which one to move
/// says which of those the command is.
async fn backdate(pool: &PgPool, column: &str, command_id: &str, secs: i64) {
    sqlx::query(&format!(
        "UPDATE terminal_commands SET {column} = now() - make_interval(secs => $1) WHERE command_id = $2"
    ))
    .bind(secs as f64)
    .bind(command_id)
    .execute(pool)
    .await
    .unwrap_or_else(|e| panic!("couldn't backdate {column} for {command_id}: {e}"));
}

/// The due commands in `conversation_id`, as ids — what every test here
/// actually wants to know.
async fn due_ids(pool: &PgPool, conversation_id: i64) -> Vec<String> {
    idle_commands_due(pool, conversation_id)
        .await
        .expect("query")
        .into_iter()
        .map(|command| command.command_id)
        .collect()
}

#[sqlx::test]
async fn test_create_terminal_command_stores_the_interval(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    let command = create_terminal_command(&pool, conversation.id, terminal_id, "idle-0", "sleep 1", 300)
        .await
        .expect("create terminal command");
    assert_eq!(command.idle_notice_secs, 300, "the interval is on the row, not looked up later");
    assert!(command.touched_at.is_none(), "nothing has touched it, so the clock counts from its start");
    assert_eq!(command.idle_notice_at, None, "no notice yet");
    let running = create_terminal_command(&pool, conversation.id, terminal_id, "idle-off", "sleep 1", 0)
        .await
        .expect("create terminal command");
    assert_eq!(running.idle_notice_secs, 0, "0 is a stored choice, not the absence of one");
}

#[sqlx::test]
async fn test_idle_commands_due_needs_the_interval_to_have_passed(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    create_terminal_command(&pool, conversation.id, terminal_id, "idle-1", "sleep 1", 300)
        .await
        .expect("create terminal command");

    assert!(due_ids(&pool, conversation.id).await.is_empty());

    backdate(&pool, "created_at", "idle-1", 299).await;
    assert!(due_ids(&pool, conversation.id).await.is_empty(), "299s of a 300s interval");

    backdate(&pool, "created_at", "idle-1", 301).await;
    assert_eq!(due_ids(&pool, conversation.id).await, vec!["idle-1".to_string()]);

    // Its own interval, in both directions: re-timing moves the interval and
    // restarts the clock from now, so a command is never told about against
    // the interval it was just given.
    let set = set_terminal_command_notice_secs(&pool, "idle-1", 60)
        .await
        .expect("set notice secs")
        .expect("a running command");
    assert_eq!(set.idle_notice_secs, 60);
    assert!(set.touched_at.is_some(), "re-timing is itself a touch");
    assert!(due_ids(&pool, conversation.id).await.is_empty(), "the clock restarted");
    backdate(&pool, "touched_at", "idle-1", 61).await;
    assert_eq!(due_ids(&pool, conversation.id).await, vec!["idle-1".to_string()], "60s is its own interval");
}

#[sqlx::test]
async fn test_idle_notice_interval_zero_is_never_due(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    create_terminal_command(&pool, conversation.id, terminal_id, "idle-quiet", "sleep 1", 0)
        .await
        .expect("create terminal command");
    backdate(&pool, "created_at", "idle-quiet", 86_400).await;
    assert!(due_ids(&pool, conversation.id).await.is_empty(), "0 means never, however long it runs");
    assert!(idle_commands_to_notice(&pool).await.expect("query").is_empty());
}

#[sqlx::test]
async fn test_idle_commands_due_carries_the_counts_and_durations(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    create_terminal_command(&pool, conversation.id, terminal_id, "idle-counts", "python3 -m http.server 8000", 300)
        .await
        .expect("create terminal command");
    for seq in 0..42 {
        append_terminal_event(&pool, "idle-counts", "stdout", seq, "serving").await.expect("append stdout");
    }
    backdate(&pool, "created_at", "idle-counts", 7_500).await;

    let due = idle_commands_due(&pool, conversation.id).await.expect("query");
    assert_eq!(due.len(), 1);
    let command = &due[0];
    assert_eq!(command.command, "python3 -m http.server 8000");
    assert_eq!(command.stdout_lines, 42);
    assert_eq!(command.stderr_lines, 0, "no output on a stream still counts as zero");
    assert!((7_500..7_560).contains(&command.running_for_secs), "{}", command.running_for_secs);
    assert_eq!(
        command.untouched_for_secs, command.running_for_secs,
        "untouched since it started, so both durations are the same"
    );
}

#[sqlx::test]
async fn test_idle_notices_repeat_until_the_command_is_touched(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    create_terminal_command(&pool, conversation.id, terminal_id, "idle-2", "sleep 1", 300)
        .await
        .expect("create terminal command");
    backdate(&pool, "created_at", "idle-2", 600).await;
    assert_eq!(due_ids(&pool, conversation.id).await, vec!["idle-2".to_string()]);

    // One notice sent: the clock restarted from it, so nothing is due now…
    let notice = [ContentBlock::Text { text: "idle-2 is still running".to_string() }];
    save_idle_notice(&pool, conversation.id, "idle-2", &notice).await.expect("save idle notice").expect("saved");
    assert!(due_ids(&pool, conversation.id).await.is_empty(), "the notice restarted the clock");

    // …and it's due again once that interval has passed. No cap: a command
    // nobody touches keeps being noticed (SME-79).
    backdate(&pool, "idle_notice_at", "idle-2", 301).await;
    assert_eq!(due_ids(&pool, conversation.id).await, vec!["idle-2".to_string()], "the second notice is owed");

    // A touch moves the clock too, and outlasts the notices: after it, the
    // next notice is a whole interval away from the touch, not from the
    // notice before.
    touch_terminal_command(&pool, "idle-2").await.expect("touch");
    assert!(due_ids(&pool, conversation.id).await.is_empty(), "the touch restarted the clock");
    backdate(&pool, "touched_at", "idle-2", 301).await;
    assert_eq!(due_ids(&pool, conversation.id).await, vec!["idle-2".to_string()]);
}

#[sqlx::test]
async fn test_idle_commands_due_skips_finished_and_lost(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    create_terminal_command(&pool, conversation.id, terminal_id, "idle-3", "sleep 1", 300)
        .await
        .expect("create terminal command");
    create_terminal_command(&pool, conversation.id, terminal_id, "idle-4", "sleep 1", 300)
        .await
        .expect("create terminal command");
    for id in ["idle-3", "idle-4"] {
        backdate(&pool, "created_at", id, 600).await;
    }
    assert_eq!(due_ids(&pool, conversation.id).await.len(), 2);

    mark_terminal_command_finished(&pool, "idle-3", 0).await.expect("mark finished");
    mark_terminal_command_lost(&pool, "idle-4").await.expect("mark lost");
    assert!(due_ids(&pool, conversation.id).await.is_empty(), "a finished or lost command is never due");
    assert!(idle_commands_to_notice(&pool).await.expect("query").is_empty());
}

#[sqlx::test]
async fn test_idle_commands_to_notice_lists_every_conversation(pool: PgPool) {
    let conversation_a = test_conversation(&pool).await;
    let conversation_b = test_conversation(&pool).await;
    let terminal_a = test_terminal(&pool, conversation_a.id).await;
    let terminal_b = test_terminal(&pool, conversation_b.id).await;
    create_terminal_command(&pool, conversation_a.id, terminal_a, "idle-a", "sleep 1", 300)
        .await
        .expect("create terminal command");
    create_terminal_command(&pool, conversation_b.id, terminal_b, "idle-b", "sleep 1", 300)
        .await
        .expect("create terminal command");
    create_terminal_command(&pool, conversation_b.id, terminal_b, "idle-b-quiet", "sleep 1", 0)
        .await
        .expect("create terminal command");
    for id in ["idle-a", "idle-b", "idle-b-quiet"] {
        backdate(&pool, "created_at", id, 600).await;
    }

    let due = idle_commands_to_notice(&pool).await.expect("query");
    let ids: Vec<_> = due.iter().map(|(_, command_id)| command_id.as_str()).collect();
    assert_eq!(ids, vec!["idle-a", "idle-b"], "due rows only, by conversation: {ids:?}");
    assert_eq!(due[0].0, conversation_a.id, "with the conversation to wake");
}

#[sqlx::test]
async fn test_save_idle_notice_saves_and_marks_once(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    create_terminal_command(&pool, conversation.id, terminal_id, "idle-n1", "sleep 1", 300)
        .await
        .expect("create terminal command");
    backdate(&pool, "created_at", "idle-n1", 600).await;
    let notice = [ContentBlock::Text { text: "idle-n1 is still running".to_string() }];

    let first = save_idle_notice(&pool, conversation.id, "idle-n1", &notice).await.expect("save").expect("marked");
    assert_eq!(first.role, "user");
    let again = save_idle_notice(&pool, conversation.id, "idle-n1", &notice).await.expect("save");
    assert!(again.is_none(), "not due anymore, so the second call saves nothing");
    let messages = list_messages(&pool, conversation.id).await.expect("list");
    assert_eq!(messages.len(), 1, "one message for the one notice: {messages:?}");
}

#[sqlx::test]
async fn test_save_idle_notice_saves_nothing_when_the_command_is_gone_or_touched(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    create_terminal_command(&pool, conversation.id, terminal_id, "idle-n2", "sleep 1", 300)
        .await
        .expect("create terminal command");
    backdate(&pool, "created_at", "idle-n2", 600).await;
    let notice = [ContentBlock::Text { text: "idle-n2 is still running".to_string() }];

    // It finished between the pass and the drain: the finish notice is its
    // own, and this one would say something untrue.
    mark_terminal_command_finished(&pool, "idle-n2", 0).await.expect("mark finished");
    assert!(save_idle_notice(&pool, conversation.id, "idle-n2", &notice).await.expect("save").is_none());

    // The model read it in the same moment: no longer due, so no notice.
    let other = create_terminal_command(&pool, conversation.id, terminal_id, "idle-n3", "sleep 1", 300)
        .await
        .expect("create terminal command");
    backdate(&pool, "created_at", "idle-n3", 600).await;
    touch_terminal_command(&pool, "idle-n3").await.expect("touch");
    assert!(save_idle_notice(&pool, conversation.id, "idle-n3", &notice).await.expect("save").is_none());
    assert!(other.idle_notice_at.is_none(), "an unmarked command can be noticed again later");

    let messages = list_messages(&pool, conversation.id).await.expect("list");
    assert!(messages.is_empty(), "a refused notice saves nothing: {messages:?}");
}

#[sqlx::test]
async fn test_idle_notice_leaves_the_finish_notice_owed(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    create_terminal_command(&pool, conversation.id, terminal_id, "idle-n4", "sleep 1", 300)
        .await
        .expect("create terminal command");
    backdate(&pool, "created_at", "idle-n4", 600).await;
    let notice = [ContentBlock::Text { text: "idle-n4 is still running".to_string() }];
    save_idle_notice(&pool, conversation.id, "idle-n4", &notice).await.expect("save idle notice").expect("saved");

    // The idle mark is `idle_notice_at`, not `notified_at`: the command's
    // finish is still owed after an idle notice.
    mark_terminal_command_finished(&pool, "idle-n4", 0).await.expect("mark finished");
    let unnotified = unnotified_finished_terminal_commands(&pool, conversation.id).await.expect("query");
    assert_eq!(unnotified.iter().map(|c| c.command_id.as_str()).collect::<Vec<_>>(), vec!["idle-n4"]);
}

#[sqlx::test]
async fn test_set_terminal_command_notice_secs_matches_no_other_row(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    create_terminal_command(&pool, conversation.id, terminal_id, "idle-f", "sleep 1", 300)
        .await
        .expect("create terminal command");
    mark_terminal_command_finished(&pool, "idle-f", 0).await.expect("mark finished");
    assert!(
        set_terminal_command_notice_secs(&pool, "idle-f", 60).await.expect("set").is_none(),
        "a finished command isn't re-timed, and says so"
    );
    assert!(set_terminal_command_notice_secs(&pool, "no-such-command", 60).await.expect("set").is_none());
    let row = get_terminal_command(&pool, "idle-f").await.expect("get").expect("row");
    assert_eq!(row.idle_notice_secs, 300, "its interval is untouched");
    assert!(row.touched_at.is_none(), "and so is its clock");
}
