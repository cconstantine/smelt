//! A terminal's shell ending (B11, SME-122; SME-191): the command in
//! flight finishes with the shell's status, and the terminal's next
//! command runs in a new shell.

use super::*;

/// B11 (SME-122): a command that ends its terminal's shell (`exit`,
/// `exec`) used to stay running for good, wedging the terminal. It
/// finishes with the shell's status and says so, and the terminal's
/// next command runs in a new shell.
#[tokio::test]
async fn test_a_command_that_ends_its_shell_finishes_and_the_terminal_keeps_working() {
    let addr = serve_agent().await;
    let mut client = connect_ready(addr).await;
    create_terminal(&mut client, "t1").await;

    let (stdout, stderr, code) = run_collect(&mut client, "t1", "c1", "echo bye; exit 3").await;
    assert_eq!((stdout, code), (vec!["bye".to_string()], 3));
    assert!(
        stderr.iter().any(|line| line.contains("shell exited") && line.contains("new shell")),
        "the command's output should say the shell ended: {stderr:?}"
    );

    let (stdout, _, code) = run_collect(&mut client, "t1", "c2", "echo again").await;
    assert_eq!((stdout, code), (vec!["again".to_string()], 0));
}

/// SME-122 review 1, without a socket's timing: an ended shell's last
/// events, still queued when the terminal's next command starts its new
/// shell, belong to the command that ended it.
#[tokio::test]
async fn test_an_ended_shells_queued_events_name_its_own_command() {
    let state = new_state();
    assert_eq!(crate::create_terminal(&state, "t1".to_string()).await, Reply::TerminalCreated);
    let first = state.terminals.lock().await.get("t1").cloned().expect("the terminal");
    start_command(&state, "t1", "c1".to_string(), "echo bye; exit 3").await;
    let deadline = tokio::time::Instant::now() + WAIT;
    while !first.ended.load(Ordering::SeqCst) {
        assert!(tokio::time::Instant::now() < deadline, "the shell didn't end");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Let the ended shell's last events reach the queue.
    tokio::time::sleep(Duration::from_millis(200)).await;
    start_command(&state, "t1", "c2".to_string(), "sleep 0.2; echo hi").await;

    let mut messages = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(2), recv_shell_event(&state)).await {
        messages.extend(event_message(event));
    }
    let exits: Vec<(String, i32)> = messages
        .iter()
        .filter_map(|m| match m {
            AgentMessage::Exit { id, code, .. } => Some((id.clone(), *code)),
            _ => None,
        })
        .collect();
    assert_eq!(exits, vec![("c1".to_string(), 3), ("c2".to_string(), 0)], "{messages:?}");
}

/// SME-122 review 2: a command claimed just as an idle shell dies (it
/// holds `current` while the shell's end is being handled) still gets
/// an exit, instead of being lost and staying running for good.
#[tokio::test]
async fn test_a_command_claimed_as_the_shell_dies_still_exits() {
    let state = new_state();
    assert_eq!(crate::create_terminal(&state, "t1".to_string()).await, Reply::TerminalCreated);
    let shell = state.terminals.lock().await.get("t1").cloned().expect("the terminal");

    // As `start_command` does, between its `ended` check and its claim.
    let mut current = shell.current.lock().await;
    signal::kill(Pid::from_raw(-(shell.bash_pid as i32)), Signal::SIGKILL).expect("kill the shell");
    tokio::time::sleep(Duration::from_millis(500)).await;
    *current = Some("cB".to_string());
    *shell.marker.lock().unwrap_or_else(|e| e.into_inner()) = Some(new_marker());
    drop(current);

    let mut exits = Vec::new();
    while let Ok(Some(event)) = tokio::time::timeout(Duration::from_secs(2), recv_shell_event(&state)).await {
        if let Some(AgentMessage::Exit { id, .. }) = event_message(event) {
            exits.push(id);
        }
    }
    assert_eq!(exits, vec!["cB".to_string()]);
}
