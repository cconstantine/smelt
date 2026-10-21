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

/// Kills a background job's process group when a test ends, pass or
/// fail: under `set -m` the job's pid (`$!`) is its group's id too.
struct JobGuard(i32);

impl Drop for JobGuard {
    fn drop(&mut self) {
        let _ = signal::kill(Pid::from_raw(-self.0), Signal::SIGKILL);
    }
}

/// The pid in a `job <pid>` line a command printed.
fn job_pid(lines: &[String]) -> i32 {
    lines
        .iter()
        .find_map(|line| line.strip_prefix("job ")?.trim().parse().ok())
        .unwrap_or_else(|| panic!("no `job <pid>` line in {lines:?}"))
}

/// SME-191: a shell that exits while a background job it started still
/// holds its output pipes finishes its command at once, with its status
/// and a note that the job is still running, instead of when the job
/// ends; the shell's own output all comes first, and the job is left
/// running.
#[tokio::test]
async fn test_a_shell_that_exits_leaving_a_background_job_finishes_its_command() {
    let addr = serve_agent().await;
    let mut client = connect_ready(addr).await;
    create_terminal(&mut client, "t1").await;

    let (stdout, stderr, code) =
        run_collect(&mut client, "t1", "c1", "seq 1 10000; sleep 30 & echo \"job $!\" >&2; exit 7").await;
    let job = JobGuard(job_pid(&stderr));
    assert_eq!(code, 7);
    let expected: Vec<String> = (1..=10000).map(|n| n.to_string()).collect();
    assert!(stdout == expected, "stdout should be 1..=10000, got {} lines", stdout.len());
    assert!(
        stderr
            .iter()
            .any(|line| line.contains("shell exited") && line.contains("background") && line.contains("new shell")),
        "the note should say a background job is still running: {stderr:?}"
    );
    assert!(signal::kill(Pid::from_raw(job.0), None).is_ok(), "the background job should be left running");

    let (stdout, stderr, code) = run_collect(&mut client, "t1", "c2", "echo again").await;
    assert_eq!((stdout, stderr, code), (vec!["again".to_string()], Vec::<String>::new(), 0));
}

/// SME-191: what a job writes after its shell's command has finished
/// is read and thrown away, so it reaches no command and the job never
/// blocks on a full pipe or dies of SIGPIPE: it writes 300 KB (over a
/// pipe's 64 KiB) a second after the exit, then touches a file.
#[tokio::test]
async fn test_a_background_jobs_output_after_its_shell_exited_reaches_no_command_and_doesnt_stop_it() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let file = std::env::temp_dir().join(format!("sme191-t2-{}-{nanos}", std::process::id()));
    let addr = serve_agent().await;
    let mut client = connect_ready(addr).await;
    create_terminal(&mut client, "t1").await;

    let command = format!(
        "{{ sleep 1; yes | head -c 300000 && touch '{}'; sleep 2; }} & echo \"job $!\" >&2; exit 0",
        file.display()
    );
    let (stdout, stderr, code) = run_collect(&mut client, "t1", "c1", &command).await;
    let _job = JobGuard(job_pid(&stderr));
    assert_eq!(code, 0);
    assert!(stdout.is_empty(), "the job's later output reached the command: {} lines", stdout.len());

    let (stdout, stderr, code) = run_collect(&mut client, "t1", "c2", "sleep 3; echo next").await;
    assert_eq!(code, 0);
    assert!(
        stdout == vec!["next".to_string()] && stderr.is_empty(),
        "the job's output reached the next command: {} stdout lines, stderr {stderr:?}",
        stdout.len()
    );

    let deadline = tokio::time::Instant::now() + WAIT;
    while !file.exists() {
        assert!(tokio::time::Instant::now() < deadline, "the job never finished writing (blocked, or killed by SIGPIPE)");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = std::fs::remove_file(&file);
}

/// SME-191: a shell killed after its command's stdout marker and before
/// its stderr one (B12), while a job holds its stdout, finishes that
/// command with the marker's code and is marked ended, so the next
/// command runs in a new shell instead of being written to the dead one.
#[tokio::test]
async fn test_a_shell_killed_between_its_markers_with_a_job_holding_its_pipes_lets_the_next_command_run() {
    let addr = serve_agent().await;
    let mut client = connect_ready(addr).await;
    create_terminal(&mut client, "t1").await;

    let (stdout, stderr, code) =
        run_collect(&mut client, "t1", "c1", "exec 2>&-; (sleep 0.3; kill -9 $$; sleep 30) & echo \"job $!\"").await;
    let _job = JobGuard(job_pid(&stdout));
    assert_eq!((code, stderr), (0, Vec::<String>::new()));

    let (stdout, stderr, code) = run_collect(&mut client, "t1", "c2", "echo next").await;
    assert_eq!((stdout, stderr, code), (vec!["next".to_string()], Vec::<String>::new(), 0));
}

/// SME-191: terminating a terminal whose idle shell has a job holding
/// its pipes ends the shell at once (the next command would get a new
/// one), rather than when the job ends, and leaves the job running.
#[tokio::test]
async fn test_terminating_a_terminal_whose_job_holds_its_pipes_ends_its_shell_promptly() {
    let state = new_state();
    assert_eq!(crate::create_terminal(&state, "t1".to_string()).await, Reply::TerminalCreated);
    let shell = state.terminals.lock().await.get("t1").cloned().expect("the terminal");
    start_command(&state, "t1", "c1".to_string(), "sleep 30 & echo \"job $!\"").await;
    let mut stdout = Vec::new();
    loop {
        let event = tokio::time::timeout(WAIT, recv_shell_event(&state))
            .await
            .expect("c1 didn't exit")
            .expect("the event queue closed");
        match event_message(event) {
            Some(AgentMessage::Output { stream: Stream::Stdout, data, .. }) => stdout.push(data),
            Some(AgentMessage::Exit { id, code, .. }) => {
                assert_eq!((id.as_str(), code), ("c1", 0));
                break;
            }
            _ => {}
        }
    }
    let job = JobGuard(job_pid(&stdout));

    assert_eq!(crate::terminate_terminal(&state, "t1".to_string()).await, Reply::TerminalTerminated);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while !shell.ended.load(Ordering::SeqCst) {
        assert!(tokio::time::Instant::now() < deadline, "the shell didn't end");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(signal::kill(Pid::from_raw(job.0), None).is_ok(), "the background job should be left running");
}
