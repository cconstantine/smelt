use super::agent_protocol::{AgentMessage, ClientMessage, PROTOCOL_VERSION, Reply, Stream};
use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message as WsMessage;

type Client = tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>;

const WAIT: Duration = Duration::from_secs(5);

async fn serve_agent() -> std::net::SocketAddr {
    serve_agent_with(new_state()).await
}

async fn serve_agent_with(state: Arc<AppState>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, router(state)).await });
    addr
}

async fn connect(addr: std::net::SocketAddr) -> Client {
    let stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let (client, _) = tokio_tungstenite::client_async(format!("ws://{addr}/ws"), stream)
        .await
        .expect("upgrade");
    client
}

/// Connects and reads past the hello.
async fn connect_ready(addr: std::net::SocketAddr) -> Client {
    let mut client = connect(addr).await;
    assert_eq!(next(&mut client).await, AgentMessage::Hello(PROTOCOL_VERSION));
    client
}

async fn send(client: &mut Client, message: &ClientMessage) {
    let text = serde_json::to_string(message).expect("serializes");
    client.send(WsMessage::Text(text.into())).await.expect("send");
}

async fn send_raw(client: &mut Client, text: &str) {
    client.send(WsMessage::Text(text.into())).await.expect("send");
}

async fn next(client: &mut Client) -> AgentMessage {
    loop {
        let frame = tokio::time::timeout(WAIT, client.next())
            .await
            .expect("the agent sent nothing within 5s")
            .expect("the connection ended")
            .expect("a WebSocket error");
        if let WsMessage::Text(text) = frame {
            return serde_json::from_str(&text)
                .unwrap_or_else(|e| panic!("the agent sent something that isn't v1 ({e}): {text}"));
        }
    }
}

/// Whether the agent closed this connection within 5s.
async fn closed(client: &mut Client) -> bool {
    let deadline = tokio::time::Instant::now() + WAIT;
    loop {
        match tokio::time::timeout_at(deadline, client.next()).await {
            Err(_) => return false,
            Ok(None) | Ok(Some(Err(_))) | Ok(Some(Ok(WsMessage::Close(_)))) => return true,
            Ok(Some(Ok(_))) => continue,
        }
    }
}

async fn create_terminal(client: &mut Client, terminal_id: &str) {
    send(
        client,
        &ClientMessage::CreateTerminal { request_id: 1, terminal_id: terminal_id.into() },
    )
    .await;
    assert_eq!(
        next(client).await,
        AgentMessage::Reply { request_id: 1, result: Reply::TerminalCreated }
    );
}

async fn run(client: &mut Client, terminal_id: &str, id: &str, command: &str) {
    send(
        client,
        &ClientMessage::Command { terminal_id: terminal_id.into(), id: id.into(), command: command.into() },
    )
    .await;
}

#[tokio::test]
async fn test_hello_is_the_first_message_on_a_connection() {
    let addr = serve_agent().await;
    let mut client = connect(addr).await;
    assert_eq!(next(&mut client).await, AgentMessage::Hello(PROTOCOL_VERSION));
}

#[tokio::test]
async fn test_terminal_actions_are_answered_with_their_request_id() {
    let addr = serve_agent().await;
    let mut client = connect_ready(addr).await;
    send(&mut client, &ClientMessage::CreateTerminal { request_id: 11, terminal_id: "t1".into() }).await;
    assert_eq!(next(&mut client).await, AgentMessage::Reply { request_id: 11, result: Reply::TerminalCreated });
    send(&mut client, &ClientMessage::CreateTerminal { request_id: 12, terminal_id: "t1".into() }).await;
    assert_eq!(
        next(&mut client).await,
        AgentMessage::Reply {
            request_id: 12,
            result: Reply::Error { message: "terminal_id already exists".into() }
        }
    );
    send(&mut client, &ClientMessage::TerminateTerminal { request_id: 13, terminal_id: "t1".into() }).await;
    assert_eq!(next(&mut client).await, AgentMessage::Reply { request_id: 13, result: Reply::TerminalTerminated });
    send(&mut client, &ClientMessage::TerminateTerminal { request_id: 14, terminal_id: "t1".into() }).await;
    assert_eq!(
        next(&mut client).await,
        AgentMessage::Reply { request_id: 14, result: Reply::Error { message: "unknown terminal_id".into() } }
    );
}

#[tokio::test]
async fn test_a_file_request_is_answered_with_its_request_id() {
    let addr = serve_agent().await;
    let mut client = connect_ready(addr).await;
    let dir = std::env::temp_dir().join(format!("smelt-agent-test-{}", std::process::id()));
    let path = dir.join("note.txt").to_string_lossy().into_owned();
    send(
        &mut client,
        &ClientMessage::WriteFile { request_id: 21, path: path.clone(), content: "a\nb\n".into(), expected_hash: None },
    )
    .await;
    let AgentMessage::Reply { request_id: 21, result: Reply::FileWritten { hash } } = next(&mut client).await else {
        panic!("write_file wasn't answered with file_written");
    };
    send(&mut client, &ClientMessage::ReadFile { request_id: 22, path: path.clone(), offset: 0, limit: 10 }).await;
    let AgentMessage::Reply { request_id: 22, result: Reply::FileRead(contents) } = next(&mut client).await else {
        panic!("read_file wasn't answered with file_read");
    };
    assert_eq!(contents.lines, vec!["a".to_string(), "b".to_string()]);
    assert_eq!(contents.hash, hash);
    send(&mut client, &ClientMessage::ReadFile { request_id: 23, path: format!("{path}.missing"), offset: 0, limit: 10 }).await;
    assert!(matches!(
        next(&mut client).await,
        AgentMessage::Reply { request_id: 23, result: Reply::Error { .. } }
    ));
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn test_a_command_streams_its_output_and_exit() {
    let addr = serve_agent().await;
    let mut client = connect_ready(addr).await;
    create_terminal(&mut client, "t1").await;
    // stdout and stderr are read concurrently, so the sleep keeps the
    // stderr line ahead of the exit marker on stdout.
    run(&mut client, "t1", "c1", "echo err >&2; sleep 0.2; echo out; exit_code() { return 3; }; exit_code").await;
    let mut seen = Vec::new();
    loop {
        match next(&mut client).await {
            AgentMessage::Output { id, stream, data, .. } => {
                assert_eq!(id, "c1");
                seen.push((stream, data));
            }
            AgentMessage::Exit { id, code, .. } => {
                assert_eq!((id.as_str(), code), ("c1", 3));
                break;
            }
            other => panic!("unexpected {other:?}"),
        }
    }
    seen.sort_by_key(|(stream, _)| stream.as_str());
    assert_eq!(seen, vec![(Stream::Stderr, "err".to_string()), (Stream::Stdout, "out".to_string())]);
}

/// Runs `command` in a fresh terminal and collects its stdout lines and
/// exit code, failing if no exit arrives within `WAIT`.
async fn run_to_exit(command: &str) -> (Vec<String>, i32) {
    let addr = serve_agent().await;
    let mut client = connect_ready(addr).await;
    create_terminal(&mut client, "t1").await;
    run(&mut client, "t1", "c1", command).await;
    let mut stdout = Vec::new();
    loop {
        match next(&mut client).await {
            AgentMessage::Output { stream: Stream::Stdout, data, .. } => stdout.push(data),
            AgentMessage::Output { .. } => {}
            AgentMessage::Exit { id, code, .. } => {
                assert_eq!(id, "c1");
                return (stdout, code);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

/// Runs `command` as `id` in `terminal_id` on an open client and
/// collects its stdout and stderr lines and exit code.
async fn run_collect(client: &mut Client, terminal_id: &str, id: &str, command: &str) -> (Vec<String>, Vec<String>, i32) {
    run(client, terminal_id, id, command).await;
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    loop {
        match next(client).await {
            AgentMessage::Output { stream: Stream::Stdout, data, .. } => stdout.push(data),
            AgentMessage::Output { stream: Stream::Stderr, data, .. } => stderr.push(data),
            AgentMessage::Exit { id: exited, code, .. } => {
                assert_eq!(exited, id);
                return (stdout, stderr, code);
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}

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

/// B12 (SME-122): a command's stderr is all sent before its exit. The
/// exit marker comes on stdout, so stderr still in its pipe could be
/// read after it and lost. 2,000 lines overfill the pipe, so most of
/// them are still unread when the marker arrives, every run.
#[tokio::test]
async fn test_all_of_a_commands_stderr_comes_before_its_exit() {
    let addr = serve_agent().await;
    let mut client = connect_ready(addr).await;
    create_terminal(&mut client, "t1").await;

    let (stdout, stderr, code) = run_collect(
        &mut client,
        "t1",
        "c1",
        "echo out1; for i in $(seq 1 2000); do echo \"err $i\" >&2; done; echo out2; printf tail >&2",
    )
    .await;

    assert_eq!((stdout, code), (vec!["out1".to_string(), "out2".to_string()], 0));
    let mut expected: Vec<String> = (1..=2000).map(|i| format!("err {i}")).collect();
    expected.push("tail".to_string());
    assert_eq!(stderr.len(), expected.len(), "stderr lines before the exit");
    assert_eq!(stderr, expected);

    // Nothing of c1's turns up in the next command.
    let (stdout, stderr, code) = run_collect(&mut client, "t1", "c2", "echo next").await;
    assert_eq!((stdout, stderr, code), (vec!["next".to_string()], vec![], 0));
}

/// SME-122 review 1: after `exec 2>&1` both end markers come on
/// stdout. The second is the stderr marker: it isn't shown as output,
/// and the command doesn't wait out the fallback for it.
#[tokio::test]
async fn test_a_shell_with_stderr_on_stdout_shows_no_marker_and_doesnt_wait() {
    let addr = serve_agent().await;
    let mut client = connect_ready(addr).await;
    create_terminal(&mut client, "t1").await;

    let (stdout, _, code) = run_collect(&mut client, "t1", "c1", "exec 2>&1; echo one").await;
    assert_eq!((stdout, code), (vec!["one".to_string()], 0));
    let started = std::time::Instant::now();
    let (stdout, _, code) = run_collect(&mut client, "t1", "c2", "echo two; echo err-two >&2").await;
    assert_eq!((stdout, code), (vec!["two".to_string(), "err-two".to_string()], 0));
    assert!(started.elapsed() < STDERR_MARKER_WAIT, "waited {:?} for a marker that came on stdout", started.elapsed());
}

/// SME-122 review 2: the end markers say which they are, so a command
/// that sends the shell's stdout elsewhere (to stderr, to /dev/null, or
/// swaps the two) still finishes, with its own exit code.
#[tokio::test]
async fn test_a_command_that_moves_the_shells_stdout_finishes_with_its_code() {
    let addr = serve_agent().await;
    let mut client = connect_ready(addr).await;
    for (n, (command, code)) in [
        ("exec 1>&2; echo one; false", 1),
        ("exec >/dev/null; echo gone; (exit 5)", 5),
        ("exec 3>&1 1>&2 2>&3; (exit 7)", 7),
    ]
    .into_iter()
    .enumerate()
    {
        let terminal = format!("t{n}");
        create_terminal(&mut client, &terminal).await;
        let (stdout, stderr, exited) = run_collect(&mut client, &terminal, &format!("c{n}"), command).await;
        assert_eq!(exited, code, "{command}");
        assert!(
            stdout.iter().chain(&stderr).all(|line| !line.contains(MARKER_PREFIX)),
            "{command}: a marker shown as output: {stdout:?} {stderr:?}"
        );
    }
}

/// B12 (SME-122): a command that sends the shell's stderr elsewhere
/// still finishes, once the wait for stderr's marker runs out, and the
/// terminal goes on working.
#[tokio::test]
async fn test_a_command_that_redirects_the_shells_stderr_still_finishes() {
    let addr = serve_agent().await;
    let mut client = connect_ready(addr).await;
    create_terminal(&mut client, "t1").await;

    let (stdout, _, code) = run_collect(&mut client, "t1", "c1", "exec 2>/dev/null; echo still").await;
    assert_eq!((stdout, code), (vec!["still".to_string()], 0));
    let (stdout, _, code) = run_collect(&mut client, "t1", "c2", "echo next").await;
    assert_eq!((stdout, code), (vec!["next".to_string()], 0));
}

/// B13 (SME-122): a very long line goes out in pieces no longer than
/// `MAX_OUTPUT_CHUNK`; one 20 MB line was over smelt's WebSocket frame
/// limit and dropped the connection. The pieces join up to the line,
/// a multi-byte character isn't split, and a marker glued onto the end
/// of a long line without a newline still ends the command.
#[tokio::test]
async fn test_a_long_line_is_sent_in_pieces_that_join_up() {
    let addr = serve_agent().await;
    let mut client = connect_ready(addr).await;
    create_terminal(&mut client, "t1").await;
    let cases = [
        ("head -c 200000 /dev/zero | tr '\\0' x; echo", "x".repeat(200_000)),
        ("head -c 200000 /dev/zero | tr '\\0' x", "x".repeat(200_000)),
        ("for i in $(seq 1 40001); do printf '\u{e9}'; done; echo", "\u{e9}".repeat(40_001)),
        // Review 1: bytes that are all UTF-8 continuation bytes (8-bit
        // audio's silence, a raw grey image) have no character start to
        // split before.
        ("head -c 70000 /dev/zero | tr '\\0' '\\200'; echo", "\u{FFFD}".repeat(70_000)),
    ];
    for (n, (command, line)) in cases.into_iter().enumerate() {
        let (stdout, _, code) = run_collect(&mut client, "t1", &format!("c{n}"), command).await;
        assert_eq!(code, 0, "{command}");
        // The cap is on bytes read; decoding turns each byte that isn't
        // UTF-8 into U+FFFD's three.
        let cap = if line.contains('\u{FFFD}') { 3 * MAX_OUTPUT_CHUNK } else { MAX_OUTPUT_CHUNK };
        assert!(
            stdout.iter().all(|piece| piece.len() <= cap),
            "{command}: pieces of {:?} bytes",
            stdout.iter().map(String::len).collect::<Vec<_>>()
        );
        assert!(stdout.len() > 1, "{command}: sent in one piece");
        assert!(stdout.concat() == line, "{command}: the pieces don't join up to the line");
    }
}

#[tokio::test]
async fn test_a_command_whose_output_has_no_trailing_newline_still_exits() {
    // Conversation 23: `head -c 400` glued the marker onto the output's
    // last line, so the command never finished and the terminal wedged.
    assert_eq!(run_to_exit("printf abc").await, (vec!["abc".to_string()], 0));
}

#[tokio::test]
async fn test_a_partial_stdout_line_survives_stderr_arriving_first() {
    // `read_line` isn't cancel safe: when the stderr branch of the
    // reader's `select!` won while stdout held half a line, that half
    // was dropped.
    assert_eq!(
        run_to_exit("printf abc; sleep 0.3; echo err >&2; sleep 0.3; echo def").await,
        (vec!["abcdef".to_string()], 0)
    );
}

/// B2 (SME-122): output that isn't UTF-8 (`cat` of a binary) used to
/// end the reader, so the command never finished and its terminal
/// wedged. It's shown with replacement characters instead.
#[tokio::test]
async fn test_output_that_isnt_utf8_still_finishes_the_command() {
    assert_eq!(
        run_to_exit("printf 'a\\xff\\xfeb\\n'; echo after").await,
        (vec!["a\u{FFFD}\u{FFFD}b".to_string(), "after".to_string()], 0)
    );
}

#[tokio::test]
async fn test_a_command_that_reads_stdin_exits_instead_of_eating_the_marker() {
    assert_eq!(run_to_exit("cat; echo after").await, (vec!["after".to_string()], 0));
}

#[tokio::test]
async fn test_output_that_looks_like_a_marker_does_not_end_the_command() {
    let (stdout, code) = run_to_exit(
        "echo MARKER:0; echo __smelt_done__:nonce:0; sleep 0.2; f() { return 4; }; f",
    )
    .await;
    assert_eq!(stdout, vec!["MARKER:0".to_string(), "__smelt_done__:nonce:0".to_string()]);
    assert_eq!(code, 4);
}

#[tokio::test]
async fn test_an_unparseable_message_gets_a_protocol_error_with_its_request_id() {
    let addr = serve_agent().await;
    let mut client = connect_ready(addr).await;
    send_raw(&mut client, r#"{"action":"read_file","request_id":42}"#).await;
    let AgentMessage::ProtocolError { request_id, message } = next(&mut client).await else {
        panic!("expected a protocol_error");
    };
    assert_eq!(request_id, Some(42));
    assert!(message.contains("path"), "the error should name what's wrong: {message}");

    send_raw(&mut client, "not json at all").await;
    let AgentMessage::ProtocolError { request_id, .. } = next(&mut client).await else {
        panic!("expected a protocol_error");
    };
    assert_eq!(request_id, None);
}

/// A `std::io::Write` into a shared buffer, for capturing log lines.
#[derive(Clone, Default)]
struct LogBuffer(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for LogBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("log buffer").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Every agent log line from every test in this binary. One process-wide
/// subscriber: a per-thread one (`set_default`) misses events whose
/// callsite another thread registered first, and tests run in parallel.
fn captured_logs() -> &'static LogBuffer {
    static LOGS: std::sync::OnceLock<LogBuffer> = std::sync::OnceLock::new();
    LOGS.get_or_init(|| {
        let logs = LogBuffer::default();
        let writer = logs.clone();
        // The agent's own logs only: tungstenite's trace logs in the
        // test's client dump every frame it sends.
        tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_env_filter(tracing_subscriber::EnvFilter::new("sandbox_agent=trace"))
            .init();
        logs
    })
}

/// A `write_file` carries a whole file, which may hold secrets. When it
/// can't be parsed, the log says why and how long it was, not what it
/// said.
#[tokio::test]
async fn test_an_unparseable_message_is_not_logged_verbatim() {
    let logs = captured_logs();

    let addr = serve_agent().await;
    let mut client = connect_ready(addr).await;
    send_raw(
        &mut client,
        r#"{"action":"write_file","request_id":5,"path":"/tmp/x","content":"password=hunter2-SECRET","expected_hash":7}"#,
    )
    .await;
    assert!(matches!(next(&mut client).await, AgentMessage::ProtocolError { request_id: Some(5), .. }));

    let logged = String::from_utf8_lossy(&logs.0.lock().expect("log buffer")).into_owned();
    assert!(logged.contains("unparseable"), "nothing was logged: {logged:?}");
    assert!(!logged.contains("hunter2-SECRET"), "the message's content was logged: {logged}");
}

#[tokio::test]
async fn test_a_second_connection_replaces_the_first() {
    let addr = serve_agent().await;
    let mut first = connect_ready(addr).await;
    create_terminal(&mut first, "t1").await;

    let mut second = connect_ready(addr).await;
    assert!(closed(&mut first).await, "the first connection is still open");

    run(&mut second, "t1", "c1", "echo hi").await;
    assert!(matches!(next(&mut second).await, AgentMessage::Output { data, .. } if data == "hi"));
    assert!(matches!(next(&mut second).await, AgentMessage::Exit { code: 0, .. }));
}

/// The exit of a command that finishes while smelt isn't connected (a
/// smelt restart) reaches the next connection.
#[tokio::test]
async fn test_events_queued_with_no_connection_reach_the_next_one() {
    let addr = serve_agent().await;
    let mut first = connect_ready(addr).await;
    create_terminal(&mut first, "t1").await;
    run(&mut first, "t1", "c1", "sleep 0.5; echo later").await;
    first.close(None).await.expect("close");
    drop(first);
    tokio::time::sleep(Duration::from_secs(1)).await;

    let mut second = connect_ready(addr).await;
    assert!(matches!(next(&mut second).await, AgentMessage::Output { data, .. } if data == "later"));
    assert!(matches!(next(&mut second).await, AgentMessage::Exit { id, code: 0, .. } if id == "c1"));
}

/// SME-122 review 1, without a socket's timing: an ended shell's last
/// events, still queued when the terminal's next command starts its new
/// shell, belong to the command that ended it.
#[tokio::test]
async fn test_an_ended_shells_queued_events_name_its_own_command() {
    let state = new_state();
    assert_eq!(super::create_terminal(&state, "t1".to_string()).await, Reply::TerminalCreated);
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
    assert_eq!(super::create_terminal(&state, "t1".to_string()).await, Reply::TerminalCreated);
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

/// An event the agent took off the queue but couldn't send (smelt's end
/// died before the agent noticed) goes to the next connection, after its
/// hello, instead of being lost with the socket.
#[tokio::test]
async fn test_an_event_that_failed_to_send_goes_to_the_next_connection() {
    let state = new_state();
    let mut dead = Box::pin(futures_util::sink::unfold((), |(), _: Message| async {
        Err::<(), _>(std::io::Error::other("connection reset"))
    }));
    let exit = AgentMessage::Exit { id: "c1".into(), terminal_id: "t1".into(), code: 0 };
    assert!(!send_or_keep(&mut dead, &state, exit.clone()).await, "a failed send reported success");

    let addr = serve_agent_with(state).await;
    let mut client = connect_ready(addr).await;
    assert_eq!(next(&mut client).await, exit);
}

/// An event a replaced connection fails to send after the new one has
/// already opened goes to the new one, not only to the one after it.
#[tokio::test]
async fn test_an_event_that_failed_to_send_reaches_the_live_connection() {
    let state = new_state();
    let addr = serve_agent_with(state.clone()).await;
    let mut live = connect_ready(addr).await;

    let mut dead = Box::pin(futures_util::sink::unfold((), |(), _: Message| async {
        Err::<(), _>(std::io::Error::other("connection reset"))
    }));
    let exit = AgentMessage::Exit { id: "c1".into(), terminal_id: "t1".into(), code: 0 };
    assert!(!send_or_keep(&mut dead, &state, exit.clone()).await);
    assert_eq!(next(&mut live).await, exit);
}

/// A failing `accept()` (EMFILE, say) waits before trying again, instead
/// of spinning, and the loop keeps accepting once it recovers.
#[tokio::test(start_paused = true)]
async fn test_the_accept_loop_backs_off_after_a_failed_accept() {
    let start = tokio::time::Instant::now();
    let calls = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let (handled_tx, handled_rx) = tokio::sync::oneshot::channel();
    let mut handled_tx = Some(handled_tx);
    let counter = calls.clone();
    tokio::spawn(accept_loop(
        move || {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            async move {
                if n < 5 {
                    Err(std::io::Error::other("too many open files"))
                } else {
                    // One connection, then nothing more to accept.
                    if n == 5 { Ok(n) } else { std::future::pending().await }
                }
            }
        },
        move |n| {
            if let Some(tx) = handled_tx.take() {
                let _ = tx.send((n, tokio::time::Instant::now()));
            }
        },
    ));
    let (n, at) = handled_rx.await.expect("the loop stopped accepting");
    assert_eq!(n, 5);
    assert!(
        at - start >= Duration::from_millis(5 * 50),
        "five failed accepts took {:?}, so the loop didn't wait between them",
        at - start
    );
}
