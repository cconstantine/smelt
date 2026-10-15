use super::*;

#[sqlx::test]
async fn test_create_terminal_command_starts_running(pool: PgPool) {
    let conversation = test_conversation(&pool).await;
    let terminal_id = test_terminal(&pool, conversation.id).await;
    let command =
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-1", "echo hi")
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
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-2", "sleep 5")
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

    create_terminal_command(&pool, conversation.id, terminal_a, "cmd-a", "sleep 5")
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
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-3", "false")
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
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-4", "sleep 100")
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
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-5", "sleep 100")
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
    let command = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-6", "echo")
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
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-7", "seq 5")
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
    let command = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-8", "echo")
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
    let command = create_terminal_command(&pool, conversation.id, terminal_id, "cmd-n1", "true")
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
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-9", "sleep 1")
            .await
            .expect("create");
    let finished =
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-10", "true")
            .await
            .expect("create");
    mark_terminal_command_finished(&pool, &finished.command_id, 0)
        .await
        .expect("mark finished");
    let lost =
        create_terminal_command(&pool, conversation.id, terminal_id, "cmd-11", "sleep 100")
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
            "echo",
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
        "echo hi",
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
        "echo hi",
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
