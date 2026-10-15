use super::*;

/// Two commands in terminal 10, `cmd-old` (with the given status and
/// code) then `cmd-new` (running).
fn two_commands(old_status: &str, old_code: Option<i32>) -> Vec<SandboxTerminalPanelEntry> {
    let mut terminals = vec![test_sandbox_terminal_entry(10, 1)];
    let mut old = test_sandbox_command_entry("cmd-old", "make");
    old.status = old_status.to_string();
    old.exit_code = old_code;
    terminals[0].commands.push(old);
    terminals[0].commands.push(test_sandbox_command_entry("cmd-new", "sleep 30"));
    terminals
}

fn finish(terminals: &mut Vec<SandboxTerminalPanelEntry>, command_id: &str, status: &str, code: Option<i32>) {
    apply_sandbox_command_update(terminals, 10, command_id.into(), None, status.into(), code, None, None, None);
}

fn statuses(terminals: &[SandboxTerminalPanelEntry]) -> Vec<(String, String, Option<i32>)> {
    terminals[0].commands.iter().map(|c| (c.command_id.clone(), c.status.clone(), c.exit_code)).collect()
}

/// SME-144: a finish for an older command (published after the model
/// already started the next one) lands on that command, not on the
/// newest, which keeps running.
#[test]
fn test_a_finish_for_an_older_command_updates_only_that_command() {
    let mut terminals = two_commands("running", None);
    finish(&mut terminals, "cmd-old", "finished", Some(2));
    assert_eq!(
        statuses(&terminals),
        vec![
            ("cmd-old".to_string(), "finished".to_string(), Some(2)),
            ("cmd-new".to_string(), "running".to_string(), None),
        ]
    );
}

#[test]
fn test_a_finish_for_a_command_the_tab_never_saw_changes_nothing() {
    let mut terminals = two_commands("finished", Some(0));
    let before = statuses(&terminals);
    finish(&mut terminals, "cmd-unknown", "finished", Some(1));
    assert_eq!(statuses(&terminals), before);
}

#[test]
fn test_a_lost_update_marks_its_command_lost_with_no_code() {
    let mut terminals = two_commands("finished", Some(0));
    finish(&mut terminals, "cmd-new", "lost", None);
    assert_eq!(statuses(&terminals)[1], ("cmd-new".to_string(), "lost".to_string(), None));
}

#[test]
fn test_a_repeated_finish_is_idempotent() {
    let mut terminals = two_commands("finished", Some(0));
    finish(&mut terminals, "cmd-new", "finished", Some(4));
    finish(&mut terminals, "cmd-new", "finished", Some(4));
    assert_eq!(statuses(&terminals)[1], ("cmd-new".to_string(), "finished".to_string(), Some(4)));
}

/// A reconnect replays a line from before the snapshot after the snapshot
/// already says the command finished: the line mustn't flip it back to
/// running.
#[test]
fn test_a_buffered_line_for_a_finished_command_keeps_it_finished() {
    let mut terminals = two_commands("finished", Some(0));
    terminals[0].commands[1].status = "finished".into();
    terminals[0].commands[1].exit_code = Some(3);
    terminals[0].commands[1].output =
        vec![SandboxOutputLinePanelEntry { stream: "stdout".into(), data: "one".into(), seq: Some(1) }];
    apply_sandbox_command_update(&mut terminals, 10, "cmd-new".into(), None, "running".into(), None, Some("stdout".into()), Some("one".into()), Some(1));
    assert_eq!(statuses(&terminals)[1], ("cmd-new".to_string(), "finished".to_string(), Some(3)));
    assert_eq!(terminals[0].commands[1].output.len(), 1, "the replayed line was added twice");
}

/// A line for an older command goes to that command.
#[test]
fn test_a_line_for_an_older_command_goes_to_that_command() {
    let mut terminals = two_commands("running", None);
    apply_sandbox_command_update(&mut terminals, 10, "cmd-old".into(), None, "running".into(), None, Some("stdout".into()), Some("late".into()), Some(9));
    assert_eq!(terminals[0].commands[0].output.len(), 1);
    assert!(terminals[0].commands[1].output.is_empty(), "the old command's line went to the new one");
}

/// A replayed start for a command the snapshot already has finished
/// leaves it finished.
#[test]
fn test_a_replayed_start_keeps_the_snapshots_finished_status() {
    let mut terminals = two_commands("finished", Some(0));
    terminals[0].commands[1].status = "finished".into();
    terminals[0].commands[1].exit_code = Some(1);
    apply_sandbox_command_update(&mut terminals, 10, "cmd-new".into(), Some("sleep 30".into()), "running".into(), None, None, None, None);
    assert_eq!(statuses(&terminals)[1], ("cmd-new".to_string(), "finished".to_string(), Some(1)));
}

/// A tab that shows a command running takes the reconnect's word that it
/// finished.
#[test]
fn test_a_snapshot_finishes_a_command_the_tab_shows_running() {
    let mut pods = Vec::new();
    let mut terminals = two_commands("finished", Some(0));
    let snapshot = SandboxSnapshot {
        pods: vec![SandboxPodSummary {
            pod_id: 1,
            status: "Running".to_string(),
            terminals: vec![SandboxTerminalSummary {
                terminal_id: 10,
                pod_id: 1,
                status: "connected".to_string(),
                commands: vec![SandboxCommandSummary {
                    command_id: "cmd-new".to_string(),
                    command: "sleep 30".to_string(),
                    status: "finished".to_string(),
                    exit_code: Some(1),
                    output: Vec::new(),
                }],
            }],
            previews: Vec::new(),
        }],
    };
    merge_sandbox_snapshot(&mut pods, &mut terminals, snapshot);
    assert_eq!(statuses(&terminals), vec![("cmd-new".to_string(), "finished".to_string(), Some(1))]);
}

#[test]
fn test_apply_sandbox_command_update_without_command_and_no_history_yet_is_a_no_op() {
    let mut terminals = vec![test_sandbox_terminal_entry(10, 1)];
    apply_sandbox_command_update(
        &mut terminals,
        10,
        "cmd-1".to_string(),
        None,
        "running".to_string(),
        None,
        Some("stdout".to_string()),
        Some("hi".to_string()), None,
    );
    assert!(
        terminals[0].commands.is_empty(),
        "an output-line update with no prior 'started' event has nothing to attach to"
    );
}

#[test]
fn test_apply_sandbox_command_update_for_unknown_terminal_is_a_no_op() {
    let mut terminals = Vec::new();
    apply_sandbox_command_update(
        &mut terminals,
        999,
        "cmd-1".to_string(),
        Some("echo hi".to_string()),
        "running".to_string(),
        None,
        None,
        None, None,
    );
    assert!(terminals.is_empty());
}

/// A conversation switch replaces the open conversation's state with
/// `ConversationState::default()` (SME-57), so every field has to start
/// empty, or the switch would carry it into the next conversation
/// (SME-51 B11). The pattern names every field: a new one doesn't compile
/// here until it's checked too.
#[test]
fn test_conversation_state_starts_empty() {
    let ConversationState {
        conversation,
        messages,
        load_error,
        streaming_reply,
        turn_running,
        turn_elapsed,
        notification_delivery_error,
        todos,
        pending_question,
        repo_url,
        repo_branch,
        repo_dir,
        repo_attaching,
        repo_attach_error,
        repo_action_error,
        repos,
        sandbox_pods,
        sandbox_terminals,
        pending_pod_stop,
        pod_stop_error,
        browsing_session_open,
        browsing_url,
        browsing_frame,
        address_draft,
        address_editing,
        address_pending,
        address_error,
        context_usage,
        context_detail,
        context_detail_open,
        layout_snap_pending,
        live,
    } = ConversationState::default();
    assert!(conversation.is_none(), "conversation");
    assert!(messages.is_empty(), "messages");
    assert!(load_error.is_none(), "load_error");
    assert!(streaming_reply.is_none(), "streaming_reply");
    assert!(!turn_running, "turn_running");
    assert_eq!(turn_elapsed, 0, "turn_elapsed");
    assert!(notification_delivery_error.is_none(), "notification_delivery_error");
    assert!(todos.is_empty(), "todos");
    assert!(pending_question.is_none(), "pending_question");
    assert!(repo_url.is_empty(), "repo_url");
    assert!(repo_branch.is_empty(), "repo_branch");
    assert!(repo_dir.is_empty(), "repo_dir");
    assert!(!repo_attaching, "repo_attaching");
    assert!(repo_attach_error.is_none(), "repo_attach_error");
    assert!(repo_action_error.is_none(), "repo_action_error");
    assert!(repos.is_empty(), "repos");
    assert!(sandbox_pods.is_empty(), "sandbox_pods");
    assert!(sandbox_terminals.is_empty(), "sandbox_terminals");
    assert!(pending_pod_stop.is_none(), "pending_pod_stop");
    assert!(pod_stop_error.is_none(), "pod_stop_error");
    assert!(!browsing_session_open, "browsing_session_open");
    assert!(browsing_url.is_none(), "browsing_url");
    assert!(browsing_frame.is_none(), "browsing_frame");
    assert!(address_draft.is_empty(), "address_draft");
    assert!(!address_editing, "address_editing");
    assert!(!address_pending, "address_pending");
    assert!(address_error.is_none(), "address_error");
    assert!(context_usage.is_none(), "context_usage");
    assert!(context_detail.is_none(), "context_detail");
    assert!(!context_detail_open, "context_detail_open");
    assert!(!layout_snap_pending, "layout_snap_pending");
    assert!(live.is_none(), "live");
}
